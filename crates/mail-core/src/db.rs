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
    /// A read whose **connection hold time** reaches this is counted (and logged) as slow. The unit is
    /// the hold, not one statement — see [`ReadGuard`].
    pub slow_query_ms: u64,
    /// `cache_size` of each **read** connection, in KiB.
    ///
    /// Negative means KiB, which is what SQLite itself documents; `design/15` §4's P5 row plans the
    /// read pool as 3 × 32 MB, so that is the default. SQLite's own default is 2 MB per connection,
    /// and at the scale the P4 budget is written for, that default is what made P4 fail: a 2026-10-19
    /// same-round A/B at 100 000 messages measured the CJK high-frequency search at P50 395.74 /
    /// P95 420.89 ms without these fields against 39.08 / 45.08 ms with them (ASCII 603.53 → 36.74 ms),
    /// and a second run of the treated arm landed 0.3% / 3.8% away. Numbers and method in
    /// `reports/performance.md` §6.5.
    pub read_cache_kib: i64,
    /// `cache_size` of the **writer**, in KiB (`design/15` §4's P5 row plans 64 MB).
    pub write_cache_kib: i64,
}

impl Default for DbOptions {
    fn default() -> Self {
        Self {
            read_conns: 3,
            busy_timeout_ms: 5000,
            synchronous: None,
            label: "cache.db".to_string(),
            slow_query_ms: 100,
            read_cache_kib: -32_768,
            write_cache_kib: -65_536,
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

/// Counters kept for the life of one handle; nothing resets them.
///
/// The app reads them every 30 seconds and turns a change in [`DbStats::slow_queries`] into a line
/// in the log the user can open — which is the only consumer, and was not true until 2026-10-19. The
/// numbers themselves mean nothing without that: a counter nobody reads cannot warn anyone.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DbStats {
    pub writes: u64,
    /// Reads whose **connection hold time** was measured — see [`Db::read_guard`] for why that is the
    /// unit, and [`Db::read_blocking`] for where it is timed.
    pub reads: u64,
    pub slow_queries: u64,
    /// The slowest read since the handle was opened. Never resets, so a caller that wants "the slow
    /// one that just happened" reads [`DbStats::slow_last_ms`] instead.
    pub slow_max_ms: u64,
    /// Hold time of the most recent flagged read, so a count delta can name a magnitude.
    pub slow_last_ms: u64,
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

/// A pooled read connection on loan, plus the timer that charges the loan.
///
/// Derefs to [`Connection`], so the 40 call sites that just wanted a connection are unchanged.
///
/// **The number this reports is hold time, not statement time.** A caller that borrows the
/// connection for three queries and a JSON pass is charged all three. That is deliberate: the budget
/// in `design/15` §4 is written as "how long did the app wait for an answer", and this layer cannot
/// see per-statement time without intercepting every query — a much hotter hook. It does mean one
/// thing the number cannot be used for: naming *which* statement was slow. The log line only says
/// which database, and that is the honest limit of what a hold-time watchdog knows.
#[derive(Debug)]
pub struct ReadGuard<'a> {
    db: &'a Db,
    conn: std::sync::MutexGuard<'a, Connection>,
    started: std::time::Instant,
}

impl std::ops::Deref for ReadGuard<'_> {
    type Target = Connection;

    fn deref(&self) -> &Connection {
        &self.conn
    }
}

impl Drop for ReadGuard<'_> {
    fn drop(&mut self) {
        self.db.record_read(self.started.elapsed());
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
        let writer = open_connection(path, kind, &opts, opts.write_cache_kib)?;
        // The read pool opens after the writer so that WAL (a persistent setting) is already on.
        let mut readers = Vec::with_capacity(opts.read_conns);
        for _ in 0..opts.read_conns.max(1) {
            readers.push(Mutex::new(open_connection(
                path,
                kind,
                &opts,
                opts.read_cache_kib,
            )?));
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
    ///
    /// The counting and the slow-query threshold live in [`Db::read_blocking`], which this delegates
    /// to — one instrumented place rather than two that can drift apart.
    pub async fn read<T, F>(&self, f: F) -> Result<T>
    where
        F: FnOnce(&Connection) -> Result<T> + Send + 'static,
        T: Send + 'static,
    {
        let db = self.clone();
        tokio::task::spawn_blocking(move || db.read_blocking(f))
            .await
            .map_err(|e| MailError::Cache(format!("读任务失败：{e}")))?
    }

    /// Borrow a connection for a **synchronous** read, from the pool when there is one.
    ///
    /// This is what lets the storage API stay synchronous without either opening a connection per
    /// call or wrapping every read in a `Send + 'static` closure. The guard derefs to a `Connection`
    /// taken from one pooled slot, so concurrent callers still use different connections.
    ///
    /// Dropping the guard charges the **hold time** to the watchdog — see [`ReadGuard`].
    ///
    /// Do not do blocking work (network, big file I/O) while holding it, and never call a write
    /// through the same handle while a guard is alive: the guard is a live borrow of the pool slot.
    pub fn read_guard(&self) -> ReadGuard<'_> {
        let started = std::time::Instant::now();
        let conn = if let Some(direct) = &self.shared.direct {
            direct.lock().expect("direct connection")
        } else {
            let index = self.shared.next_reader.fetch_add(1, Ordering::Relaxed) as usize
                % self.shared.readers.len().max(1);
            self.shared.readers[index].lock().expect("reader lock")
        };
        ReadGuard {
            db: self,
            conn,
            started,
        }
    }

    /// Blocking read, for startup paths that run before the Tokio runtime exists.
    ///
    /// Never call this from inside a task on the runtime's worker threads — it blocks one.
    ///
    /// This is the one instrumented read path: [`Db::read`] delegates here, and every synchronous
    /// caller either comes through here or holds a [`ReadGuard`]. Timing it here rather than in each
    /// entry point is what keeps the two from drifting apart — which is how `slow_queries` spent a
    /// whole release at zero while the 40 call sites that actually answer the user ran unmeasured
    /// (`read_guard()` in `cache.rs` and `userdb.rs`, production code — recount before believing).
    pub fn read_blocking<T, F>(&self, f: F) -> Result<T>
    where
        F: FnOnce(&Connection) -> Result<T> + Send + 'static,
        T: Send + 'static,
    {
        let started = std::time::Instant::now();
        let out = self.read_untimed(f);
        self.record_read(started.elapsed());
        out
    }

    /// The pool slot lookup and the closure, with no timing in it.
    ///
    /// Split out because [`Db::read_guard`] needs to charge for the *hold* rather than a single call,
    /// and an instrumented `read_blocking` inside that hold would count the same wait twice.
    fn read_untimed<T, F>(&self, f: F) -> Result<T>
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

    /// Charge one read to the watchdog.
    ///
    /// Kept to the two things that can be done cheaply on a sub-millisecond path: one stats lock and
    /// (only for a slow read) one line on stderr. The user-visible copy of the same numbers is written
    /// by the watchdog loop in the app layer, because `eprintln!` from a released Windows GUI process
    /// goes nowhere — that is the other half of why the counter existing never helped anyone.
    fn record_read(&self, elapsed: std::time::Duration) {
        let elapsed_ms = elapsed.as_millis() as u64;
        let slow = elapsed_ms >= self.shared.slow_query_ms;
        self.bump(|s| {
            s.reads += 1;
            if slow {
                s.slow_queries += 1;
                s.slow_last_ms = elapsed_ms;
                if elapsed_ms > s.slow_max_ms {
                    s.slow_max_ms = elapsed_ms;
                }
            }
        });
        if slow {
            eprintln!("[db] 慢查询 {}：{elapsed_ms} ms", self.shared.label);
        }
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

    /// The hold time at or above which a read counts as slow. Exposed so a consumer that reports the
    /// counts can also report the bar they were measured against — a number without its threshold is
    /// not a claim anyone can check.
    pub fn slow_query_threshold_ms(&self) -> u64 {
        self.shared.slow_query_ms
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

fn open_connection(
    path: &Path,
    kind: DbKind,
    opts: &DbOptions,
    cache_kib: i64,
) -> Result<Connection> {
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
    //
    // `cache_size` is the opposite case: it is per-connection state that the file never remembers,
    // so it has to be applied to every connection this function opens. Setting it on the writer
    // alone leaves the read pool — the one that answers list and search — on SQLite's 2 MB default.
    //
    // There is deliberately no `mmap_size` here even though `design/15` §5 lists one. Measured
    // 2026-10-19 (100 000 封, same round, `cache_size` held equal in both arms): warm searches moved
    // by 0.4–3% with one shape getting 3.9% *worse*, i.e. nothing outside the run-to-run floor, while
    // the first query after a sync went 94.27 → 57.19 ms. The cost was +255.6 MB of peak working set
    // — 844.7 MB against 589.1 MB, the whole of `mmap_size` landing in the working set — which puts
    // `design/15` §4's P5 (常驻 ≤ 300 MB) out of reach. The three-arm table is
    // `reports/performance.md` §6.6; read it before re-adding this pragma.
    let pragmas = format!(
        "pragma auto_vacuum = incremental;
         pragma journal_mode = wal;
         pragma synchronous = {synchronous};
         pragma busy_timeout = {};
         pragma temp_store = memory;
         pragma cache_size = {cache_kib};",
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

    /// A WAL database is three files, and a test that leaves `-wal` behind turns `%TEMP%` into litter.
    fn remove_db_files(path: PathBuf) {
        let stem = path
            .file_name()
            .expect("name")
            .to_string_lossy()
            .to_string();
        for suffix in ["", "-wal", "-shm"] {
            let mut candidate = path.clone();
            candidate.set_file_name(format!("{stem}{suffix}"));
            let _ = std::fs::remove_file(candidate);
        }
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

    /// **The slow-query detector has to see the reads production actually performs.**
    ///
    /// `design/15` §4 names `DbStats.slow` as the measurement source for P4, and the counter has
    /// existed since then — but the only timed path was [`Db::read`], which two diagnostic pragmas
    /// call. Every mail read (list, search, detail) goes through [`Db::read_guard`] or
    /// [`Db::read_blocking`] instead, so the counter could only ever sit at zero: a drift in P4 would
    /// stay invisible, which is the failure mode this repo keeps hitting ("有测量不等于被测量").
    ///
    /// The second half is the negative control: with a threshold nothing reaches, a normal read must
    /// **not** be flagged — a detector that counts everything says nothing about drift either.
    #[tokio::test]
    async fn every_pooled_read_is_counted_and_only_a_slow_one_is_flagged() {
        let open = |threshold: u64, name: &str| -> (Db, PathBuf) {
            let path = temp_path(name);
            let db = Db::open(
                &path,
                DbKind::Cache,
                DbOptions {
                    label: "watchdog.db".to_string(),
                    slow_query_ms: threshold,
                    synchronous: Some("off".to_string()),
                    ..DbOptions::for_kind(DbKind::Cache)
                },
            )
            .expect("open");
            db.write_blocking(|conn| {
                conn.execute_batch(
                    "create table t (id integer primary key, v text);
                     insert into t (v) values ('x');",
                )?;
                Ok(())
            })
            .expect("seed");
            (db, path)
        };
        let count_with_guard = |db: &Db| {
            let conn = db.read_guard();
            conn.query_row("select count(*) from t", [], |r| r.get::<_, i64>(0))
                .expect("count")
        };

        // The path the storage API actually takes: borrow a pooled connection.
        let (db, path) = open(0, "guard-threshold-zero");
        assert_eq!(count_with_guard(&db), 1);
        let guarded = db.stats();
        assert!(
            guarded.reads == 1 && guarded.slow_queries == 1,
            "a guarded read must be counted, and with the threshold at 0 it must be flagged: {guarded:?}"
        );
        drop(db);
        remove_db_files(path);

        // A fresh handle, same statement, this time through the async pooled reader — it must be
        // counted once, not twice, now that the timing sits under both entry points.
        let (db, path) = open(10_000, "guard-threshold-high");
        assert_eq!(count_with_guard(&db), 1);
        let async_count: i64 = db
            .read(|conn| Ok(conn.query_row("select count(*) from t", [], |r| r.get(0))?))
            .await
            .expect("async read");
        assert_eq!(async_count, 1);
        let stats = db.stats();
        assert_eq!(stats.reads, 2, "guard + async read, one each: {stats:?}");
        assert_eq!(
            stats.slow_queries, 0,
            "a 10-second threshold must not flag a sub-millisecond read: {stats:?}"
        );
        drop(db);
        remove_db_files(path);
    }

    /// **What the watchdog costs, measured against the read it sits on.**
    ///
    /// Timed reads are on the path that answers the list and the search, so an instrument that shows
    /// up in the user's numbers is a bug in the instrument rather than a cost of doing the job. The
    /// shape measured here is the **pessimistic** one on purpose: a `select count(*)` over three rows
    /// is the fastest read the app performs, so it takes the largest possible *share* out of. If the
    /// overhead is inside the run-to-run jitter here, it is definitionally invisible on the 39 ms
    /// search that P4 is written for.
    ///
    /// Two things are compared, because they fail differently:
    ///   * **single-threaded** — the added `Instant` pair plus one stats lock per read;
    ///   * **eight readers at once** — the only mechanism by which a per-read lock could actually
    ///     hurt, since every pooled connection now queues on the same mutex.
    ///
    /// Arms are paired inside each round and the per-round ratio is printed, never a median divided
    /// by a median across rounds: the swing between rounds of this benchmark is larger than the
    /// effect being looked for.
    #[tokio::test]
    #[ignore]
    async fn the_watchdog_costs_less_than_the_read_it_measures() {
        const READS: usize = 20_000;
        const ROUNDS: usize = 3;

        let path = temp_path("watchdog-cost");
        // `slow_query_ms` is set beyond any reachable hold time so this measures the counting, not
        // the logging: a threshold that tripped on every read would measure an `eprintln!` instead.
        let db = Db::open(
            &path,
            DbKind::Cache,
            DbOptions {
                label: "cost.db".to_string(),
                synchronous: Some("off".to_string()),
                slow_query_ms: u64::MAX,
                ..DbOptions::for_kind(DbKind::Cache)
            },
        )
        .expect("open");
        db.write_blocking(|conn| {
            conn.execute_batch(
                "create table t (id integer primary key, v text);
                 insert into t (v) values ('x'), ('y'), ('z');",
            )?;
            Ok(())
        })
        .expect("seed");

        let count = |conn: &rusqlite::Connection| -> Result<i64> {
            Ok(conn.query_row("select count(*) from t", [], |r| r.get::<_, i64>(0))?)
        };
        let timed_ns = |db: &Db| {
            let started = std::time::Instant::now();
            for _ in 0..READS {
                db.read_blocking(count).expect("timed read");
            }
            started.elapsed().as_nanos() as f64 / READS as f64
        };
        let untimed_ns = |db: &Db| {
            let started = std::time::Instant::now();
            for _ in 0..READS {
                db.read_untimed(count).expect("untimed read");
            }
            started.elapsed().as_nanos() as f64 / READS as f64
        };
        let guard_ns = |db: &Db| {
            let started = std::time::Instant::now();
            for _ in 0..READS {
                let conn = db.read_guard();
                conn.query_row("select count(*) from t", [], |r| r.get::<_, i64>(0))
                    .expect("guarded read");
            }
            started.elapsed().as_nanos() as f64 / READS as f64
        };

        // Both arms' caches (rusqlite's statement cache per connection, SQLite's page cache, the
        // branch predictors) warm up on the first pass, and whichever arm leads a round pays for it.
        // Measured without this, the arm that happened to run first looked up to 25 % *slower* than
        // the one that followed it — an artefact large enough to hide, or invent, an overhead of the
        // size being looked for. So: one throwaway warm-up, then rotate which arm leads.
        let _ = (untimed_ns(&db), timed_ns(&db), guard_ns(&db));

        println!("== 单线程：每臂 {READS} 次读，逐轮配对（轮次换领跑臂）==");
        let mut ratios: Vec<(f64, f64)> = Vec::new();
        let mut baseline: Vec<f64> = Vec::new();
        for round in 0..ROUNDS {
            // `(step + round) % 3` rotates which arm leads: index 0 = untimed, 1 = timed,
            // 2 = guarded, and round 1 starts at the arm round 0 finished with.
            let mut ns = [0f64; 3];
            for step in 0..3 {
                let arm = (step + round) % 3;
                ns[arm] = match arm {
                    0 => untimed_ns(&db),
                    1 => timed_ns(&db),
                    _ => guard_ns(&db),
                };
            }
            let (a, b, c) = (ns[0], ns[1], ns[2]);
            println!(
                "  第 {} 轮  未计时 {a:>7.1} ns/读   计时 {b:>7.1} ns/读   借连接 {c:>7.1} ns/读   比值 {:>5.3} / {:>5.3}（领跑：{}）",
                round + 1,
                b / a,
                c / a,
                ["未计时", "计时", "借连接"][round % 3],
            );
            ratios.push((b / a, c / a));
            baseline.push(a);
        }
        // The noise floor: the same arm measured three separate times in these same rounds.
        let floor = (baseline.iter().copied().fold(f64::MIN, f64::max)
            / baseline.iter().copied().fold(f64::MAX, f64::min)
            - 1.0)
            * 100.0;
        println!("  未计时臂自身的逐轮抖动：{floor:.1}%（比值落在 ±这个范围内就不能被分辨）");
        println!(
            "  配对比值 计时÷未计时 {}，借连接÷未计时 {}",
            ratios
                .iter()
                .map(|(b, _)| format!("{b:.3}"))
                .collect::<Vec<_>>()
                .join(" / "),
            ratios
                .iter()
                .map(|(_, c)| format!("{c:.3}"))
                .collect::<Vec<_>>()
                .join(" / ")
        );

        println!(
            "== 8 个线程抢 3 条读连接：每线程 {} 次读（统计锁的唯一杀伤方式）==",
            READS / 8
        );
        let concurrent = |instrumented: bool| {
            let handles: Vec<_> = (0..8)
                .map(|_| {
                    let db = db.clone();
                    std::thread::spawn(move || {
                        let started = std::time::Instant::now();
                        for _ in 0..READS / 8 {
                            if instrumented {
                                db.read_blocking(count).expect("timed");
                            } else {
                                db.read_untimed(count).expect("untimed");
                            }
                        }
                        started.elapsed().as_nanos() as f64 / (READS / 8) as f64
                    })
                })
                .collect();
            // Per-read latency as each thread experienced it, averaged over the eight.
            handles
                .into_iter()
                .map(|h| h.join().expect("join"))
                .sum::<f64>()
                / 8.0
        };
        let _ = (concurrent(false), concurrent(true));
        for round in 0..ROUNDS {
            let (off, on) = if round % 2 == 0 {
                (concurrent(false), concurrent(true))
            } else {
                (concurrent(true), concurrent(false))
            };
            println!(
                "  第 {} 轮  未计时 {off:>8.1} ns/读   计时 {on:>8.1} ns/读   比值 {:.3}",
                round + 1,
                on / off
            );
        }

        drop(db);
        remove_db_files(path);
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

        // The writer parks **inside** its transaction until this test lets it go. That is what makes
        // the assertion below causal instead of a stopwatch: if the read were waiting on the writer,
        // it would not return at all, and the timeout would say so.
        //
        // The previous version slept 40 ms and then asserted the read finished within 120 ms. It was
        // flaky — two failures in three runs on a machine that was busy compiling — because it
        // measured the *machine*, not the behaviour: WAL keeps readers off the writer's lock, but it
        // cannot make a slow machine fast.
        let (entered_tx, entered_rx) = std::sync::mpsc::channel::<()>();
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let writer = {
            let db = db.clone();
            tokio::spawn(async move {
                // No explicit BEGIN: `write` wraps the closure in a transaction, which is what holds
                // the write lock for as long as this closure runs.
                db.write(move |conn| {
                    conn.execute("insert into m (v) values (2)", [])?;
                    let _ = entered_tx.send(());
                    // Released by the test once it has read. The timeout is a deadlock guard, not a
                    // schedule.
                    let _ = release_rx.recv_timeout(std::time::Duration::from_secs(10));
                    Ok(())
                })
                .await
            })
        };

        // Wait for the writer to be *inside* its transaction, so the read below happens while the
        // write lock is provably held. The wait runs on a blocking thread on purpose: this test is a
        // current-thread runtime, and parking its only thread here would stop `db.write` from ever
        // being polled — a deadlock of the test's own making.
        let entered = tokio::task::spawn_blocking(move || {
            entered_rx.recv_timeout(std::time::Duration::from_secs(10))
        })
        .await
        .expect("join");
        entered.expect("the writer must reach its transaction");

        let started = std::time::Instant::now();
        let count: i64 = db
            .read(|conn| Ok(conn.query_row("select count(*) from m", [], |r| r.get(0))?))
            .await
            .expect("read during write");
        let elapsed = started.elapsed();
        assert_eq!(count, 1, "the uncommitted row must not be visible");
        assert!(
            elapsed < std::time::Duration::from_secs(2),
            "reading during a write must not wait for it (took {elapsed:?})"
        );

        release_tx.send(()).expect("release the writer");
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

        // `cache_size` is per-connection state the file never remembers, so it is asserted on the
        // **read pool** — where the list and the search actually run — and on every connection in it,
        // because "applied to the writer only" reads back green while leaving each reader on
        // SQLite's 2 MB default. A brand-new connection would report the default and prove nothing.
        // Expected value is taken per kind, because the two databases are already allowed to differ.
        for (kind, db) in [(DbKind::Cache, &set.cache), (DbKind::User, &set.user)] {
            for _ in 0..db.shared.readers.len() {
                let seen: i64 = db
                    .read(|conn| Ok(conn.query_row("pragma cache_size", [], |r| r.get(0))?))
                    .await
                    .expect("cache_size pragma");
                assert_eq!(
                    seen,
                    DbOptions::for_kind(kind).read_cache_kib,
                    "{} 的每条读连接都要拿到页缓存设置，否则搜索仍在 SQLite 默认的 2 MB 上跑",
                    db.label()
                );
            }
            // And `mmap_size` is asserted to stay **off**, because the decision to leave it out is a
            // measured one (nothing outside the run-to-run floor on warm search, for +255.6 MB of peak
            // working set — see `open_connection`). Without this line a future "顺手 add mmap" reads as
            // a free win in a benchmark that does not sample memory.
            let mapped: i64 = db
                .read(|conn| Ok(conn.query_row("pragma mmap_size", [], |r| r.get(0))?))
                .await
                .expect("mmap_size pragma");
            assert_eq!(
                mapped,
                0,
                "{} 的 mmap 被打开了，但它只换来噪声级的搜索收益，贵 255.6 MB 峰值工作集；\
                 要留就得先按 design/15 §4 的 P5 实测常驻内存（数据见 reports/performance.md §6.6）",
                db.label()
            );
        }

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
