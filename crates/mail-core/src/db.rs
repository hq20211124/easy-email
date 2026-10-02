//! The database handle: one writer thread, a small read pool, and no connection sharing across
//! `.await`.
//!
//! # Why this exists
//!
//! v0.2 opened a fresh `rusqlite::Connection` for **every command and every batch row**. At the scale
//! this app targets (a 100k-message mailbox) that is 100k implicit transactions for a first sync,
//! each with its own lock acquisition and its own durability guarantee — slow, and it makes "one
//! logical operation" impossible to express atomically.
//!
//! `rusqlite::Connection` is `!Sync`, so the naive fix (share one connection behind a `Mutex`) would
//! serialise reads behind writes and stall the UI. Hence the shape here:
//!
//! * **One writer thread** owning a `Connection`. Every write goes through it, so writes are
//!   serialised by construction (SQLite allows only one writer anyway) and a batch can be wrapped in
//!   a real transaction.
//! * **A read pool** of `read_conns` connections, each with the same pragmas. Readers never block the
//!   writer and vice versa (WAL), which is what keeps the list and search usable during a sync.
//! * **Closures are `Send + 'static`** and get `&Connection`. That bound is the honest description of
//!   "this runs on another thread": it cannot borrow from the caller, and everything it returns must
//!   be owned. It is *not* the `for<'a>` borrowing trap from `pool_op!` — there is no connection
//!   reference in the signature at all.
//!
//! # Runtime
//!
//! The writer is a plain `std::thread` and talks over `std::sync::mpsc`; replies come back on
//! `tokio::sync::oneshot`. **`Db` must never call `tokio::spawn`**, because `Db::open` runs from
//! Tauri's `setup()`, which has no Tokio reactor — spawning there panics and the window never opens
//! (see `HANDOFF.md`).
//!
//! # Two databases
//!
//! [`DbSet`] holds `cache` and `user` handles. They are separate files with separate pragmas (a
//! cache can be rebuilt, drafts and rules cannot), and they are deliberately **not** `ATTACH`ed:
//! backup, integrity checks, repair and `VACUUM` all have to be per-file operations, and no write
//! path in this app needs cross-database atomicity. The four places that span both (flag changes,
//! rule actions, contact collection, sent-copy bookkeeping) are each idempotent and reconciled by the
//! nightly check.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};
use std::thread;

use rusqlite::{Connection, OpenFlags};
use tokio::sync::oneshot;

use crate::error::{MailError, Result};

/// Kinds of database this app keeps apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DbKind {
    /// Re-fetchable data: folders, messages, bodies, index. Deleting it costs a re-sync.
    Cache,
    /// User data: drafts, outbox, pending operations, rules, contacts, settings, and any message
    /// body that exists nowhere else (imported archives, POP3 mail).
    User,
}

impl DbKind {
    /// `synchronous` mode for this database.
    ///
    /// `Full` for user data: a draft or a queued send must survive a power cut. `Normal` for the
    /// cache: a crash may lose the last transaction, which only costs one re-fetch.
    fn synchronous(self) -> &'static str {
        match self {
            DbKind::Cache => "normal",
            DbKind::User => "full",
        }
    }
}

/// Options for opening one database.
#[derive(Debug, Clone)]
pub struct DbOptions {
    /// Read connections to keep. Three covers "list, search, background count" without oversubscribing.
    pub read_conns: usize,
    pub busy_timeout_ms: u64,
    /// Override the kind's default `synchronous` (tests use this to keep things fast).
    pub synchronous: Option<String>,
    /// File name shown in error messages.
    pub label: String,
    /// A read that takes at least this long is counted (and logged) as a slow query.
    pub slow_query_ms: u64,
}

impl Default for DbOptions {
    fn default() -> Self {
        Self {
            read_conns: 3,
            busy_timeout_ms: 5000,
            synchronous: None,
            label: "cache.db".to_string(),
            slow_query_ms: 100,
        }
    }
}

impl DbOptions {
    pub fn for_kind(kind: DbKind) -> Self {
        Self {
            label: match kind {
                DbKind::Cache => "cache.db",
                DbKind::User => "user.db",
            }
            .to_string(),
            ..Self::default()
        }
    }
}

/// Counters the diagnostics bundle and the settings screen read.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DbStats {
    pub writes: u64,
    pub reads: u64,
    pub slow_queries: u64,
    pub busy_retries: u64,
    /// Depth of the writer queue at the last observation — a growing number means writes are the
    /// bottleneck.
    pub write_queue: usize,
}

struct Shared {
    /// `Some` for an in-memory handle: there is exactly one connection and no threads, so reads and
    /// writes both take this lock. Production handles have `None` and use the writer thread below.
    direct: Option<Mutex<Connection>>,
    writer: Mutex<Option<Sender<Job>>>,
    readers: Vec<Mutex<Connection>>,
    next_reader: AtomicU64,
    /// Writes accepted but not yet executed. Exposed through [`DbStats::write_queue`] so a
    /// growing backlog is visible instead of guessed.
    pending_writes: AtomicU64,
    stats: Mutex<DbStats>,
    slow_query_ms: u64,
    label: String,
    path: Option<PathBuf>,
}

type Job = Box<dyn FnOnce(&mut Connection) + Send>;

/// A handle to one SQLite database, cheap to clone and safe to share across tasks.
#[derive(Clone)]
pub struct Db {
    shared: Arc<Shared>,
}

impl std::fmt::Debug for Db {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Db")
            .field("label", &self.shared.label)
            .finish()
    }
}

impl Db {
    /// Open (creating if needed) a database at `path`, with a writer thread and a read pool.
    pub fn open(path: &Path, kind: DbKind, opts: DbOptions) -> Result<Self> {
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent).map_err(|e| {
                MailError::Cache(format!("无法创建数据库目录 {}：{e}", parent.display()))
            })?;
        }
        let writer = open_connection(path, kind, &opts)?;
        // The read pool opens after the writer so that WAL (a persistent setting) is already on.
        let mut readers = Vec::with_capacity(opts.read_conns);
        for _ in 0..opts.read_conns.max(1) {
            readers.push(Mutex::new(open_connection(path, kind, &opts)?));
        }
        Ok(Self::assemble(
            Some(writer),
            readers,
            opts,
            Some(path.to_path_buf()),
            None,
        ))
    }

    /// In-memory database for unit tests.
    ///
    /// **Single connection**, deliberately: on a shared-cache in-memory database SQLite refuses WAL
    /// (it stays `memory`), so a "concurrent reader" test would be testing a mode production never
    /// uses. Concurrency behaviour is covered by file-backed tests instead.
    pub fn open_in_memory(label: &str) -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch(
            "pragma journal_mode = memory;
             pragma synchronous = off;
             pragma busy_timeout = 5000;",
        )?;
        let opts = DbOptions {
            label: label.to_string(),
            ..DbOptions::default()
        };
        Ok(Self::assemble(None, Vec::new(), opts, None, Some(conn)))
    }

    fn assemble(
        writer: Option<Connection>,
        readers: Vec<Mutex<Connection>>,
        opts: DbOptions,
        path: Option<PathBuf>,
        direct: Option<Connection>,
    ) -> Self {
        let (tx, rx) = mpsc::channel::<Job>();
        let label = opts.label.clone();
        let slow = opts.slow_query_ms;
        let shared = Arc::new(Shared {
            direct: direct.map(Mutex::new),
            writer: Mutex::new(Some(tx)),
            readers,
            next_reader: AtomicU64::new(0),
            pending_writes: AtomicU64::new(0),
            stats: Mutex::new(DbStats::default()),
            slow_query_ms: slow,
            label,
            path,
        });

        if let Some(writer) = writer {
            let thread_shared = Arc::clone(&shared);
            thread::Builder::new()
                .name(format!("db-writer-{}", opts.label))
                .spawn(move || {
                    let mut conn = writer;
                    // `recv` ends when every `Db` clone is dropped, which is exactly when the thread
                    // should exit: the sender lives inside `Shared`.
                    while let Ok(job) = rx.recv() {
                        job(&mut conn);
                    }
                    let _ = thread_shared;
                })
                .expect("spawning the database writer thread");
        }

        Self { shared }
    }

    /// Run `f` on the writer thread, **inside a transaction**.
    ///
    /// The transaction is the whole point: a 500-row page becomes one commit instead of 500, and a
    /// failure half way through leaves the database exactly as it was. The closure gets
    /// `&mut Connection` (a `Transaction` derefs to one) so it can use savepoints when it needs
    /// finer boundaries than "all or nothing".
    pub async fn write<T, F>(&self, f: F) -> Result<T>
    where
        F: FnOnce(&Connection) -> Result<T> + Send + 'static,
        T: Send + 'static,
    {
        if let Some(direct) = &self.shared.direct {
            let conn = direct.lock().expect("direct connection");
            let tx = conn.unchecked_transaction()?;
            let out = f(&tx)?;
            tx.commit()?;
            return Ok(out);
        }
        if let Some(direct) = &self.shared.direct {
            let conn = direct.lock().expect("direct connection");
            let tx = conn.unchecked_transaction()?;
            let out = f(&tx)?;
            tx.commit()?;
            return Ok(out);
        }
        // `unchecked_transaction` (rather than `transaction`) because it takes `&Connection`: a
        // `Transaction` only implements `Deref`, not `DerefMut`, so a `&mut Connection` closure
        // cannot be handed one. "Unchecked" only means it does not verify that a transaction is
        // already open — and this layer is the only place that opens one.
        self.dispatch(|conn| {
            let tx = conn.unchecked_transaction()?;
            let out = f(&tx)?;
            tx.commit()?;
            Ok(out)
        })
        .await
    }

    /// Synchronous [`Db::write`]: the same transaction, run on the writer thread, **blocking** the
    /// calling thread until it finishes.
    ///
    /// Exists because most of the storage API is synchronous and is called from the sync driver,
    /// which is not an `async fn`. Waiting is done on a channel rather than by spinning, so a slow
    /// batch costs nothing but a parked thread.
    ///
    /// Do not call this from a Tokio worker thread — it blocks it.
    pub fn write_blocking<T, F>(&self, f: F) -> Result<T>
    where
        F: FnOnce(&Connection) -> Result<T> + Send + 'static,
        T: Send + 'static,
    {
        if let Some(direct) = &self.shared.direct {
            let conn = direct.lock().expect("direct connection");
            let tx = conn.unchecked_transaction()?;
            let out = f(&tx)?;
            tx.commit()?;
            return Ok(out);
        }
        if let Some(direct) = &self.shared.direct {
            let conn = direct.lock().expect("direct connection");
            let tx = conn.unchecked_transaction()?;
            let out = f(&tx)?;
            tx.commit()?;
            return Ok(out);
        }
        let (tx, rx) = mpsc::channel::<Result<T>>();
        let label = self.shared.label.clone();
        self.shared.pending_writes.fetch_add(1, Ordering::Relaxed);
        let pending = Arc::clone(&self.shared);
        let job: Job = Box::new(move |conn: &mut Connection| {
            // `txn` rather than `tx`: the outer `tx` is the reply channel, and shadowing it here
            // sends the result into a transaction that is about to be dropped.
            let result = match conn.unchecked_transaction() {
                Ok(txn) => match f(&txn) {
                    Ok(out) => txn.commit().map(|_| out).map_err(MailError::from),
                    Err(err) => Err(err),
                },
                Err(err) => Err(MailError::from(err)),
            };
            pending.pending_writes.fetch_sub(1, Ordering::Relaxed);
            let _ = tx.send(result);
        });
        {
            let sender = self.shared.writer.lock().expect("writer lock");
            let Some(sender) = sender.as_ref() else {
                return Err(MailError::Cache(format!("{label} 的写线程已关闭")));
            };
            sender
                .send(job)
                .map_err(|_| MailError::Cache(format!("{label} 的写线程已退出")))?;
        }
        rx.recv()
            .map_err(|_| MailError::Cache(format!("{label} 的写线程没有返回结果")))?
    }

    /// Run `f` on the writer **without a transaction**.
    ///
    /// Needed because some statements cannot run inside one: `VACUUM` is the one that matters here
    /// (turning auto-vacuum on for an existing database requires a full `VACUUM`). Everything that
    /// *can* be transactional should use [`Db::write_blocking`] instead.
    pub fn exec_blocking<T, F>(&self, f: F) -> Result<T>
    where
        F: FnOnce(&Connection) -> Result<T> + Send + 'static,
        T: Send + 'static,
    {
        if let Some(direct) = &self.shared.direct {
            let conn = direct.lock().expect("direct connection");
            return f(&conn);
        }
        let (tx, rx) = mpsc::channel::<Result<T>>();
        let label = self.shared.label.clone();
        let job: Job = Box::new(move |conn: &mut Connection| {
            let _ = tx.send(f(conn));
        });
        {
            let sender = self.shared.writer.lock().expect("writer lock");
            let Some(sender) = sender.as_ref() else {
                return Err(MailError::Cache(format!("{label} 的写线程已关闭")));
            };
            sender
                .send(job)
                .map_err(|_| MailError::Cache(format!("{label} 的写线程已退出")))?;
        }
        rx.recv()
            .map_err(|_| MailError::Cache(format!("{label} 的写线程没有返回结果")))?
    }

    /// Run `f` in a transaction, retrying the whole closure on `SQLITE_BUSY`.
    ///
    /// The closure must be idempotent (it is called more than once) — that is the point: the
    /// transaction rolled back, so re-running it from the start is the correct recovery.
    pub async fn write_retry<T, F>(&self, f: F) -> Result<T>
    where
        F: Fn(&Connection) -> Result<T> + Send + Sync + 'static,
        T: Send + 'static,
    {
        let f = Arc::new(f);
        let mut attempt = 0;
        loop {
            let f = Arc::clone(&f);
            let outcome = self.dispatch(move |conn| f(conn)).await;
            match outcome {
                Err(MailError::Busy(_)) if attempt < 2 => {
                    attempt += 1;
                    self.bump(|s| s.busy_retries += 1);
                    tokio::time::sleep(std::time::Duration::from_millis(50 * attempt as u64)).await;
                }
                other => return other,
            }
        }
    }

    async fn dispatch<T, F>(&self, f: F) -> Result<T>
    where
        F: FnOnce(&mut Connection) -> Result<T> + Send + 'static,
        T: Send + 'static,
    {
        let queue_depth = self.queue_depth();
        self.bump(|s| {
            s.writes += 1;
            s.write_queue = queue_depth;
        });

        let (tx, rx) = oneshot::channel();
        let label = self.shared.label.clone();
        let pending = Arc::clone(&self.shared);
        self.shared.pending_writes.fetch_add(1, Ordering::Relaxed);
        let job: Job = Box::new(move |conn: &mut Connection| {
            let result = f(conn);
            pending.pending_writes.fetch_sub(1, Ordering::Relaxed);
            let _ = tx.send(result);
        });

        {
            let sender = self.shared.writer.lock().expect("writer lock");
            let Some(sender) = sender.as_ref() else {
                return Err(MailError::Cache(format!("{label} 的写线程已关闭")));
            };
            sender
                .send(job)
                .map_err(|_| MailError::Cache(format!("{label} 的写线程已退出")))?;
        }

        rx.await
            .map_err(|_| MailError::Cache(format!("{label} 的写线程没有返回结果")))?
    }

    /// Run `f` on a pooled read connection.
    pub async fn read<T, F>(&self, f: F) -> Result<T>
    where
        F: FnOnce(&Connection) -> Result<T> + Send + 'static,
        T: Send + 'static,
    {
        self.bump(|s| s.reads += 1);
        let db = self.clone();
        let started = std::time::Instant::now();
        let out = tokio::task::spawn_blocking(move || db.read_blocking(f))
            .await
            .map_err(|e| MailError::Cache(format!("读任务失败：{e}")))?;
        let elapsed = started.elapsed().as_millis() as u64;
        if elapsed >= self.shared.slow_query_ms {
            self.bump(|s| s.slow_queries += 1);
            eprintln!("[db] 慢查询 {}：{} ms", self.shared.label, elapsed);
        }
        out
    }

    /// Borrow a connection for a **synchronous** read, from the pool when there is one.
    ///
    /// This is what lets the storage API stay synchronous without either opening a connection per
    /// call or wrapping every read in a `Send + 'static` closure. The guard is a `MutexGuard` over
    /// one pooled connection, so concurrent callers still use different connections.
    ///
    /// Do not do blocking work (network, big file I/O) while holding it, and never call a write
    /// through the same handle while a guard is alive: the guard is a live borrow of the pool slot.
    pub fn read_guard(&self) -> std::sync::MutexGuard<'_, Connection> {
        if let Some(direct) = &self.shared.direct {
            return direct.lock().expect("direct connection");
        }
        let index = self.shared.next_reader.fetch_add(1, Ordering::Relaxed) as usize
            % self.shared.readers.len().max(1);
        self.shared.readers[index].lock().expect("reader lock")
    }

    /// Blocking read, for startup paths that run before the Tokio runtime exists.
    ///
    /// Never call this from inside a task on the runtime's worker threads — it blocks one.
    pub fn read_blocking<T, F>(&self, f: F) -> Result<T>
    where
        F: FnOnce(&Connection) -> Result<T> + Send + 'static,
        T: Send + 'static,
    {
        if let Some(direct) = &self.shared.direct {
            let conn = direct.lock().expect("direct connection");
            return f(&conn);
        }
        if self.shared.readers.is_empty() {
            // In-memory handle: there is one connection and it belongs to the writer. Taking the
            // writer lock would deadlock, so the caller's closure runs against a read-only view…
            // except we cannot clone a `Connection`. Instead we route through the writer thread
            // synchronously, which is safe because nothing else can be in flight at open time.
            return self.blocking_via_writer(f);
        }
        let index = self.shared.next_reader.fetch_add(1, Ordering::Relaxed) as usize
            % self.shared.readers.len();
        let guarded = self.shared.readers[index].lock().expect("reader lock");
        f(&guarded)
    }

    fn blocking_via_writer<T, F>(&self, f: F) -> Result<T>
    where
        F: FnOnce(&Connection) -> Result<T> + Send + 'static,
        T: Send + 'static,
    {
        let sender = {
            let guard = self.shared.writer.lock().expect("writer lock");
            guard
                .as_ref()
                .cloned()
                .ok_or_else(|| MailError::Cache("写线程已关闭".to_string()))?
        };
        let (tx, rx) = mpsc::channel::<Result<T>>();
        let job: Job = Box::new(move |conn: &mut Connection| {
            let result = f(conn);
            let _ = tx.send(result);
        });
        sender
            .send(job)
            .map_err(|_| MailError::Cache("写线程已退出".to_string()))?;
        rx.recv()
            .map_err(|_| MailError::Cache("写线程没有返回结果".to_string()))?
    }

    /// What SQLite actually stored for auto-vacuum (`0` none, `1` full, `2` incremental).
    ///
    /// Read on the **writer** on purpose. A pooled reader that was opened before the file had any
    /// pages can report the old value (`0`) even after the writer created tables with auto-vacuum on
    /// — the reader has the pre-initialisation header page cached and a bare pragma read does not
    /// refresh it. That is a diagnostic lie, so the question is answered where the truth lives.
    pub async fn auto_vacuum_state(&self) -> Result<i64> {
        self.write(|conn| Ok(conn.query_row("pragma auto_vacuum", [], |row| row.get(0))?))
            .await
    }

    /// Bytes on the freelist: free pages that are still part of the file.
    pub async fn freelist_bytes(&self) -> Result<i64> {
        self.read(|conn| {
            let pages: i64 = conn.query_row("pragma freelist_count", [], |r| r.get(0))?;
            let page_size: i64 = conn.query_row("pragma page_size", [], |r| r.get(0))?;
            Ok(pages * page_size)
        })
        .await
    }

    /// Run `PRAGMA quick_check` on a reader and report whether the file is sound.
    pub async fn quick_check(&self) -> Result<bool> {
        self.read(|conn| {
            let answer: String = conn.query_row("pragma quick_check", [], |row| row.get(0))?;
            Ok(answer.eq_ignore_ascii_case("ok"))
        })
        .await
    }

    pub fn stats(&self) -> DbStats {
        self.shared.stats.lock().expect("stats").clone()
    }

    /// Path this handle was opened from, if any.
    pub fn path(&self) -> Option<&Path> {
        self.shared.path.as_deref()
    }

    pub fn label(&self) -> &str {
        &self.shared.label
    }

    fn queue_depth(&self) -> usize {
        self.shared.pending_writes.load(Ordering::Relaxed) as usize
    }

    fn bump(&self, f: impl FnOnce(&mut DbStats)) {
        let mut stats = self.shared.stats.lock().expect("stats");
        f(&mut stats);
    }
}

/// Both databases, as the app holds them.
#[derive(Clone, Debug)]
pub struct DbSet {
    pub cache: Db,
    pub user: Db,
}

impl DbSet {
    /// Open both, with each one's own pragmas.
    pub fn open(cache_path: &Path, user_path: &Path) -> Result<Self> {
        Ok(Self {
            cache: Db::open(
                cache_path,
                DbKind::Cache,
                DbOptions::for_kind(DbKind::Cache),
            )?,
            user: Db::open(user_path, DbKind::User, DbOptions::for_kind(DbKind::User))?,
        })
    }

    /// The handle a given kind of content belongs in.
    ///
    /// Route by **what the row is**, not by which table it is in: a message that exists only on this
    /// machine (`origin != 'server'`) is user data even though it lives in the messages table.
    pub fn writer_for(&self, kind: DbKind) -> &Db {
        match kind {
            DbKind::Cache => &self.cache,
            DbKind::User => &self.user,
        }
    }
}

fn open_connection(path: &Path, kind: DbKind, opts: &DbOptions) -> Result<Connection> {
    let conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_WRITE
            | OpenFlags::SQLITE_OPEN_CREATE
            | OpenFlags::SQLITE_OPEN_URI
            | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    let synchronous = opts
        .synchronous
        .clone()
        .unwrap_or_else(|| kind.synchronous().to_string());
    // **`auto_vacuum` must be the very first pragma — before `journal_mode = wal`.**
    //
    // The documented rule is "before any table is created", but that is not tight enough: switching
    // the journal mode writes the database header, and after that SQLite treats the file as
    // initialised and silently ignores an auto-vacuum change. Measured here (both orders, fresh
    // files): `journal_mode` first ⇒ `pragma auto_vacuum` reads back **0**; `auto_vacuum` first ⇒
    // **2**. The symptom of getting it wrong is a "cleanup freed 200 MB" message that does not
    // change the file size at all, with no error anywhere.
    let pragmas = format!(
        "pragma auto_vacuum = incremental;
         pragma journal_mode = wal;
         pragma synchronous = {synchronous};
         pragma busy_timeout = {};
         pragma temp_store = memory;",
        opts.busy_timeout_ms
    );
    conn.execute_batch(&pragmas)?;
    Ok(conn)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(name: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "easy-email-db-{}-{}-{}.sqlite",
            std::process::id(),
            name,
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        p
    }

    #[tokio::test]
    async fn a_write_runs_inside_a_transaction_that_rolls_back_on_error() {
        let db = Db::open_in_memory("rollback").expect("open");
        db.write(|conn| {
            conn.execute_batch("create table t (id integer primary key, v text);")?;
            Ok(())
        })
        .await
        .expect("ddl");

        let failed: Result<()> = db
            .write(|conn| {
                conn.execute("insert into t (v) values ('a')", [])?;
                // Fail after the insert: the transaction must take the row with it.
                Err(MailError::Cache("boom".to_string()))
            })
            .await;
        assert!(failed.is_err());

        let count: i64 = db
            .read(|conn| Ok(conn.query_row("select count(*) from t", [], |r| r.get(0))?))
            .await
            .expect("count");
        assert_eq!(
            count, 0,
            "a failed write must not leave partial rows behind"
        );
    }

    #[tokio::test]
    async fn writes_are_serialised_and_all_of_them_land() {
        let db = Db::open_in_memory("serial").expect("open");
        db.write(|conn| {
            conn.execute_batch("create table n (id integer primary key, v integer);")?;
            Ok(())
        })
        .await
        .expect("ddl");

        let mut handles = Vec::new();
        for i in 0..50i64 {
            let db = db.clone();
            handles.push(tokio::spawn(async move {
                db.write(move |conn| {
                    conn.execute("insert into n (v) values (?1)", [i])?;
                    Ok(())
                })
                .await
            }));
        }
        for h in handles {
            h.await.expect("join").expect("write");
        }

        let (count, sum): (i64, i64) = db
            .read(|conn| {
                Ok(
                    conn.query_row("select count(*), coalesce(sum(v), 0) from n", [], |r| {
                        Ok((r.get(0)?, r.get(1)?))
                    })?,
                )
            })
            .await
            .expect("aggregate");
        assert_eq!(count, 50);
        assert_eq!(sum, (0..50).sum::<i64>());
    }

    #[tokio::test]
    async fn a_reader_is_not_blocked_by_a_writer_on_a_file_database() {
        let path = temp_path("wal");
        let db = Db::open(&path, DbKind::Cache, DbOptions::default()).expect("open");
        db.write(|conn| {
            conn.execute_batch(
                "create table m (id integer primary key, v integer);
                 insert into m (v) values (1);",
            )?;
            Ok(())
        })
        .await
        .expect("seed");

        // A long write transaction on the writer thread.
        let writer = {
            let db = db.clone();
            tokio::spawn(async move {
                // No explicit BEGIN: `write` wraps the closure in a transaction, which is what
                // holds the write lock for the duration of the sleep.
                db.write(|conn| {
                    conn.execute("insert into m (v) values (2)", [])?;
                    std::thread::sleep(std::time::Duration::from_millis(150));
                    Ok(())
                })
                .await
            })
        };

        // While it is open, a read must still succeed promptly (WAL, not a shared lock).
        tokio::time::sleep(std::time::Duration::from_millis(40)).await;
        let started = std::time::Instant::now();
        let count: i64 = db
            .read(|conn| Ok(conn.query_row("select count(*) from m", [], |r| r.get(0))?))
            .await
            .expect("read during write");
        let elapsed = started.elapsed();
        assert_eq!(count, 1, "the uncommitted row must not be visible");
        assert!(
            elapsed < std::time::Duration::from_millis(120),
            "reading during a write must not wait for it (took {elapsed:?})"
        );

        writer.await.expect("join").expect("write");
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn both_databases_keep_their_own_pragmas_and_files() {
        let cache_path = temp_path("cache");
        let user_path = temp_path("user");
        let set = DbSet::open(&cache_path, &user_path).expect("open both");

        // Each handle reaches its own file.
        set.cache
            .write(|conn| {
                conn.execute_batch("create table c (x integer);")?;
                Ok(())
            })
            .await
            .expect("cache ddl");
        set.user
            .write(|conn| {
                conn.execute_batch("create table u (x integer);")?;
                Ok(())
            })
            .await
            .expect("user ddl");

        let in_cache: i64 = set
            .cache
            .read(|conn| {
                Ok(conn.query_row(
                    "select count(*) from sqlite_master where name = 'u'",
                    [],
                    |r| r.get(0),
                )?)
            })
            .await
            .expect("cache check");
        assert_eq!(
            in_cache, 0,
            "the user table must not exist in the cache file"
        );

        // And the configured synchronous mode is what SQLite reports (2 = FULL, 1 = NORMAL).
        let cache_sync: i64 = set
            .cache
            .read(|conn| Ok(conn.query_row("pragma synchronous", [], |r| r.get(0))?))
            .await
            .expect("pragma");
        let user_sync: i64 = set
            .user
            .read(|conn| Ok(conn.query_row("pragma synchronous", [], |r| r.get(0))?))
            .await
            .expect("pragma");
        assert_eq!(
            cache_sync, 1,
            "the cache trades a lost transaction for speed"
        );
        assert_eq!(user_sync, 2, "user data must survive a power cut");

        // auto_vacuum really is on (2 = incremental) in both. Read through the writer: a pooled
        // reader opened before the file was initialised can report a stale 0, which is exactly the
        // kind of "green check that means nothing" this project tries to avoid.
        for db in [&set.cache, &set.user] {
            // A brand-new connection must see it too — the setting lives in the file, not in the
            // connection that applied it.
            if let Some(path) = db.path() {
                let fresh = Connection::open(path).expect("fresh");
                let seen: i64 = fresh
                    .query_row("pragma auto_vacuum", [], |r| r.get(0))
                    .expect("fresh pragma");
                assert_eq!(seen, 2, "{} must have it recorded in the file", db.label());
            }
            let mode = db.auto_vacuum_state().await.expect("pragma");
            assert_eq!(
                mode,
                2,
                "{} must be created with incremental auto-vacuum",
                db.label()
            );
        }

        drop(set);
        let _ = std::fs::remove_file(&cache_path);
        let _ = std::fs::remove_file(&user_path);
    }

    #[tokio::test]
    async fn quick_check_reports_a_healthy_file() {
        let path = temp_path("check");
        let db = Db::open(&path, DbKind::Cache, DbOptions::default()).expect("open");
        assert!(db.quick_check().await.expect("check"));
        drop(db);
        let _ = std::fs::remove_file(&path);
    }
}
