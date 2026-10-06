//! Local SQLite cache and the FTS5 search index.
//!
//! IMAP is a remote protocol. Listing a folder re-fetches from the server every time, reading a
//! message costs a round trip, and nothing is available offline. This module keeps a local copy of
//! what has been synced so the UI can answer from disk.
//!
//! Two things are worth knowing before touching this file:
//!
//! **1. UIDs are only meaningful together with `UIDVALIDITY`.** If a server changes
//! `UIDVALIDITY` for a folder it is telling you every UID you hold is now meaningless — a
//! different message may have the UID you cached. So the cursor is `(uidvalidity, last_uid)` and
//! a mismatch wipes the folder. This is the single easiest way to corrupt a mail cache.
//!
//! **2. FTS5 cannot search Chinese out of the box.** `unicode61` treats a run of Han characters as
//! one token, so searching 账单 finds nothing inside 这是您十月的账单. The FTS5 `trigram`
//! tokenizer is the usual suggestion and is also wrong here: it needs three characters, and a very
//! large share of Chinese words are two (账单, 发票, 银行, 提醒 …). Measured, both ways.
//!
//! The approach that works is to index and query **one character per token** — see [`to_tokens`]
//! and [`to_query`]. The two must stay in step; a test pins that.

use rusqlite::{Connection, OptionalExtension, params};

use crate::db::{Db, DbKind, DbOptions};
use crate::error::{MailError, Result};
use crate::model::{MailboxInfo, MessageDetail, MessageSummary, ThreadMember};

/// Bumped whenever the schema below changes; [`Cache::migrate`] reacts to it.
pub const SCHEMA_VERSION: i64 = 6;

/// Where a delivered message's Sent copy is (`design/07` §5.4).
///
/// Derived data: it lives in the rebuildable cache, and losing it costs the "查看已发送副本" link,
/// nothing more. The *intent* to archive is carried by the spool file's existence instead, which is
/// why that is in `user.db`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SentCopyRecord {
    pub account_id: String,
    pub message_id: String,
    /// The folder it is in, as the wire name.
    pub folder: String,
    /// Its UID, or `None` when the server would not tell us (`APPEND` does not return one; A1-F43).
    /// `None` means "the copy is there, its number is unknown", never "no copy".
    pub uid: Option<u32>,
    pub local_at: i64,
}

/// How far a folder has been synced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FolderState {
    /// Server-side generation counter. A change invalidates every UID below.
    pub uidvalidity: u32,
    /// Highest UID already stored, i.e. where the next incremental fetch starts.
    pub last_uid: u32,
}

/// One row of a local search result.
///
/// Serialisable because it crosses the Tauri IPC boundary: Tauri refuses to compile a command whose
/// return type is not `IpcResponse`, and that needs `Serialize` on everything inside.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SearchHit {
    /// Wire folder name, so the UI can `SELECT` it and open the message.
    pub folder: String,
    pub uid: u32,
    pub subject: String,
    pub from_name: Option<String>,
    pub from_addr: Option<String>,
    pub date: Option<i64>,
    /// Carried so the UI can show the unread dot in results, and so opening a hit only asks the
    /// server to mark it read when it actually needs marking.
    pub seen: bool,
}

/// The columns that feed one row of the search index.
///
/// The body is not a column of its own: it lives inside the serialised `MessageDetail`, so it is
/// pulled out of `detail_json` in Rust rather than by the query.
struct IndexSource {
    subject: String,
    from_name: Option<String>,
    from_addr: Option<String>,
    to_addrs: Option<String>,
    detail_json: Option<String>,
}

/// Turn text into FTS5 tokens, one character per token.
///
/// This is what makes Chinese searchable. The transformation must be applied to *both* the indexed
/// text and the user's query, or nothing matches.
pub fn to_tokens(text: &str) -> String {
    text.chars()
        .map(|c| c.to_string())
        .collect::<Vec<_>>()
        .join(" ")
}

/// Turn user input into an FTS5 `MATCH` expression.
///
/// Each whitespace-separated term becomes a phrase of single characters, and the terms are ANDed —
/// so `账单 发票` means "the adjacent characters 账单, and also 发票". Quotes are dropped: they are
/// FTS5 syntax, and letting them through turns a pasted string into a `MATCH` syntax error.
pub fn to_query(input: &str) -> String {
    let terms: Vec<String> = input
        .split_whitespace()
        .map(|term| term.replace(['"', '\''], ""))
        .filter(|term| !term.is_empty())
        .map(|term| format!("\"{}\"", to_tokens(&term)))
        .collect();

    if terms.is_empty() {
        // Nothing searchable. A bare `*` cannot match anything, so this is a safe no-op query.
        return String::new();
    }
    terms.join(" AND ")
}

#[derive(Clone)]
pub struct Cache {
    /// Shared handle: one writer thread plus a read pool (`db.rs`). Not a bare `Connection`, because
    /// the app must not open a connection per command — and because a batch has to be one
    /// transaction.
    db: Db,
}

impl Cache {
    /// Open (creating if needed) the cache database at `path`.
    ///
    /// Callers open a connection per operation rather than sharing one, because
    /// `rusqlite::Connection` is `!Sync`: holding it across an `.await` would make a Tauri command's
    /// future non-`Send`. A `busy_timeout` is set so a search running while a sync writes waits for
    /// the writer instead of failing outright with `SQLITE_BUSY`.
    pub fn open(path: &std::path::Path) -> Result<Self> {
        let cache = Self {
            db: Db::open(path, DbKind::Cache, DbOptions::for_kind(DbKind::Cache))?,
        };
        cache.migrate()?;
        Ok(cache)
    }

    /// Wrap an already-open handle (used when the app owns a [`DbSet`]).
    pub fn from_db(db: Db) -> Result<Self> {
        let cache = Self { db };
        cache.migrate()?;
        Ok(cache)
    }

    /// In-memory cache, for tests.
    pub fn open_in_memory() -> Result<Self> {
        let cache = Self {
            db: Db::open_in_memory("cache")?,
        };
        cache.migrate()?;
        Ok(cache)
    }

    /// What SQLite actually thinks the auto-vacuum mode is (`0` none, `1` full, `2` incremental).
    ///
    /// Read back rather than assumed: an existing database keeps whatever it was created with, and
    /// that difference is the difference between "cleanup frees disk" and "cleanup does nothing".
    pub fn auto_vacuum_state(&self) -> Result<i64> {
        self.db
            .write_blocking(|conn| Ok(conn.query_row("pragma auto_vacuum", [], |row| row.get(0))?))
    }

    /// Bytes sitting on the freelist, i.e. pages that are free but still part of the file.
    pub fn freelist_bytes(&self) -> Result<i64> {
        self.db.read_blocking(|conn| {
            let pages: i64 = conn.query_row("pragma freelist_count", [], |row| row.get(0))?;
            let page_size: i64 = conn.query_row("pragma page_size", [], |row| row.get(0))?;
            Ok(pages * page_size)
        })
    }

    /// Run an incremental vacuum, which is a no-op unless auto-vacuum is incremental.
    ///
    /// Returns the number of freelist pages that remain (0 on an auto-vacuum database means the
    /// space really did come back).
    pub fn vacuum_into_freelist(&self) -> Result<i64> {
        self.db.write_blocking(|conn| {
            conn.execute_batch("pragma incremental_vacuum;")?;
            Ok(conn.query_row("pragma freelist_count", [], |row| row.get(0))?)
        })
    }

    /// Whether this database predates the auto-vacuum pragma and therefore needs the one-time
    /// rebuild before "cleanup freed N MB" can be true on disk.
    ///
    /// Real instances of this exist: any cache created by v0.2 reports `auto_vacuum = 0`, because
    /// SQLite only accepts the setting on an empty database (or after a full `VACUUM`).
    pub fn needs_auto_vacuum_rebuild(&self) -> Result<bool> {
        Ok(self.auto_vacuum_state()? != 2)
    }

    /// One-off, **blocking** full `VACUUM`, the only way to turn on auto-vacuum for a database that
    /// already has tables (SQLite rejects the pragma change otherwise).
    ///
    /// Deliberately not called during startup: it rewrites the whole file, so the caller decides
    /// when to pay for it (design/03 §8: an explicit, interruptible, user-visible step).
    pub fn rebuild_with_auto_vacuum(&self) -> Result<()> {
        // `exec_blocking`, not `write_blocking`: `VACUUM` refuses to run inside a transaction, and
        // this is the one operation in the storage API whose whole purpose is to rewrite the file.
        self.db.exec_blocking(|conn| {
            let current: i64 = conn.query_row("pragma auto_vacuum", [], |row| row.get(0))?;
            if current == 2 {
                return Ok(());
            }
            conn.execute_batch(
                "pragma auto_vacuum = incremental;
                 vacuum;
                 pragma auto_vacuum = incremental;",
            )?;
            Ok(())
        })
    }

    fn migrate(&self) -> Result<()> {
        // One transaction: the whole schema appears or none of it does, which is what makes
        // "the version says v3" a statement about a complete schema.
        self.db.write_blocking(move |conn: &Connection| {


            let version: i64 = conn
                .query_row("pragma user_version", [], |row| row.get(0))?;
            if version == SCHEMA_VERSION {
                return Ok(());
            }

            // v1 is the first schema, so there is nothing to migrate from — a database from the future
            // is the only case worth refusing.
            if version > SCHEMA_VERSION {
                return Err(MailError::Cache(format!(
                    "缓存数据库版本 {version} 比程序支持的 {SCHEMA_VERSION} 更新，请升级程序"
                )));
            }
            // Some steps below are only needed for databases that predate a version, and they cannot
            // be inferred from the schema after the fact.
            let previous_version = version;

            conn.execute_batch(
                "create table if not exists folders (
                     account_id   text not null,
                     name         text not null,
                     display_name text not null,
                     delimiter    text,
                     selectable   integer not null default 1,
                     uidvalidity  integer not null default 0,
                     last_uid     integer not null default 0,
                     synced_at    integer,
                     primary key (account_id, name)
                 );

                 create table if not exists messages (
                     account_id text not null,
                     folder     text not null,
                     uid        integer not null,
                     subject    text not null default '',
                     from_name  text,
                     from_addr  text,
                     to_addrs   text,
                     date       integer,
                     size       integer,
                     seen       integer not null default 0,
                     flagged    integer not null default 0,
                     primary key (account_id, folder, uid)
                 );

                 create index if not exists messages_by_date
                     on messages(account_id, folder, date desc, uid desc);

                 create table if not exists bodies (
                     account_id text not null,
                     folder     text not null,
                     uid        integer not null,
                     detail     text not null,
                     fetched_at integer not null,
                     primary key (account_id, folder, uid)
                 );

                 -- Standalone (not external-content) on purpose: it stores the tokenised text, which is
                 -- not the same text as in `messages`. Independent of `unicode61`'s defaults.
                 create virtual table if not exists search using fts5(
                     subject, from_name, from_addr, to_addrs, body,
                     account_id unindexed, folder unindexed, uid unindexed,
                     tokenize = 'unicode61'
                 );

                 -- Where each delivered message's Sent copy ended up (design/07 §5.4/§7). Derived
                 -- data, so it belongs in the cache that can be rebuilt — the *intent* to archive is
                 -- carried by the spool file's existence, not by this row.
                 create table if not exists sent_index (
                     account_id  text not null,
                     message_id  text not null,
                     folder      text not null,
                     uid         integer,
                     local_at    integer not null,
                     primary key (account_id, message_id)
                 );",
            )?;

            conn
                .pragma_update(None, "user_version", SCHEMA_VERSION)?;

            // A column cannot arrive through `create table if not exists`: for a file that already
            // has the table, SQLite reads the existing definition and ignores the new one — so the
            // statement would succeed while the column silently never appeared. This is the first
            // *column* this schema has added, which is why it is spelled out rather than assumed.
            add_column_if_missing(conn, "messages", "thread_key", "text")?;

            // **After** the column, not with the other index above it. The conversation query filters
            // on `thread_key`, and without this it is a scan of the account's rows; but a v2 file
            // reaches this function with a `messages` table that has no such column yet, so creating
            // the index in the same batch would fail with "no such column" and the whole upgrade with
            // it. Index creation is idempotent, and it runs inside the version-guarded block, so an
            // already-current file never repeats the work.
            conn.execute(
                "create index if not exists messages_by_thread on messages(account_id, thread_key)",
                [],
            )?;

            // v5: the second, heuristic conversation signal (`threads::subject_key`), for mail with no
            // `References` chain. Same column-not-in-create-table rule as above.
            add_column_if_missing(conn, "messages", "subject_key", "text")?;
            conn.execute(
                "create index if not exists messages_by_subject_key
                     on messages(account_id, subject_key)",
                [],
            )?;

            // Fill it for the rows that already exist. **From the rows themselves** — subject, sender
            // and the account's address are all in `messages`, so this migration needs no network: an
            // upgrade that quietly required a sync before old mail could be grouped would leave the
            // user looking at rows that never merge.
            // v6: the FTS row that belongs to this message, so the index can be addressed by rowid.
            // Before this the delete scanned the entire index on every single message — see the
            // comment in `refresh_search_row`; the benchmark measured the difference.
            add_column_if_missing(conn, "messages", "search_rowid", "integer")?;
            // The search query joins `messages.search_rowid` to the FTS rowid, and without this index
            // that join is a scan of `messages` per match — which is the second half of the same
            // performance bug and was measured at 769 ms P50 on a 20 000-message fixture.
            conn.execute(
                "create index if not exists messages_by_search_rowid
                     on messages(search_rowid)",
                [],
            )?;
            if previous_version < 6 {
                // Nothing else can fix rows indexed under the old scheme: their rowids are unrelated
                // to their messages, so there is no way to find them except by scanning. Rebuilding
                // is cheap (it is derived data) and this is the only moment it can be done in one
                // pass.
                rebuild_search_index(conn)?;
            }
            backfill_subject_keys(conn)?;
            Ok(())

        })
    }

    // ------------------------------------------------------------------ folders

    /// Remember a folder and its `UIDVALIDITY`, leaving the sync cursor alone.
    pub fn upsert_folder(
        &self,
        account_id: &str,
        mailbox: &MailboxInfo,
        uidvalidity: u32,
    ) -> Result<()> {
        let account_id = account_id.to_string();
        let mailbox = mailbox.clone();
        self.db.write_blocking(move |conn: &Connection| {

            conn.execute(
                "insert into folders (account_id, name, display_name, delimiter, selectable, uidvalidity)
                 values (?1, ?2, ?3, ?4, ?5, ?6)
                 on conflict(account_id, name) do update set
                     display_name = excluded.display_name,
                     delimiter    = excluded.delimiter,
                     selectable   = excluded.selectable,
                     uidvalidity  = excluded.uidvalidity",
                params![
                    account_id,
                    mailbox.name,
                    mailbox.display_name,
                    mailbox.delimiter,
                    i64::from(mailbox.selectable),
                    i64::from(uidvalidity),
                ],
            )?;
            Ok(())

        })
    }

    /// The folder list as last seen, for offline startup.
    pub fn list_folders(&self, account_id: &str) -> Result<Vec<MailboxInfo>> {
        let guard = self.db.read_guard();
        let conn: &Connection = &guard;

        let mut stmt = conn.prepare(
            "select name, display_name, delimiter, selectable
             from folders where account_id = ?1 order by name",
        )?;
        let rows = stmt.query_map(params![account_id], |row| {
            Ok(MailboxInfo {
                name: row.get(0)?,
                display_name: row.get(1)?,
                delimiter: row.get(2)?,
                selectable: row.get::<_, i64>(3)? != 0,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// One folder as last seen, if it has ever been synced.
    ///
    /// Lets a sync reuse the display name and delimiter already known instead of issuing `LIST`
    /// again on every single message-list refresh.
    pub fn get_folder(&self, account_id: &str, name: &str) -> Result<Option<MailboxInfo>> {
        let guard = self.db.read_guard();
        let conn: &Connection = &guard;

        Ok(conn
            .query_row(
                "select name, display_name, delimiter, selectable
                 from folders where account_id = ?1 and name = ?2",
                params![account_id, name],
                |row| {
                    Ok(MailboxInfo {
                        name: row.get(0)?,
                        display_name: row.get(1)?,
                        delimiter: row.get(2)?,
                        selectable: row.get::<_, i64>(3)? != 0,
                    })
                },
            )
            .optional()?)
    }

    pub fn folder_state(&self, account_id: &str, folder: &str) -> Result<Option<FolderState>> {
        let guard = self.db.read_guard();
        let conn: &Connection = &guard;

        Ok(conn
            .query_row(
                "select uidvalidity, last_uid from folders where account_id = ?1 and name = ?2",
                params![account_id, folder],
                |row| {
                    Ok(FolderState {
                        uidvalidity: row.get::<_, i64>(0)? as u32,
                        last_uid: row.get::<_, i64>(1)? as u32,
                    })
                },
            )
            .optional()?)
    }

    /// Move the incremental cursor forward.
    pub fn set_cursor(
        &self,
        account_id: &str,
        folder: &str,
        uidvalidity: u32,
        last_uid: u32,
    ) -> Result<()> {
        let account_id = account_id.to_string();
        let folder = folder.to_string();
        self.db.write_blocking(move |conn: &Connection| {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);
            conn.execute(
                "update folders set uidvalidity = ?3, last_uid = ?4, synced_at = ?5
                 where account_id = ?1 and name = ?2",
                params![
                    account_id,
                    folder,
                    i64::from(uidvalidity),
                    i64::from(last_uid),
                    now
                ],
            )?;
            Ok(())
        })
    }

    /// Drop everything cached for one folder and reset its cursor.
    ///
    /// Called when `UIDVALIDITY` changes: at that point every stored UID may refer to a different
    /// message, so keeping any of it would show the wrong mail under the wrong headers.
    ///
    /// The stored `uidvalidity` is zeroed as well, which is what tells the sync driver that the
    /// next pass must start from a fresh window rather than trusting a cursor it no longer has.
    pub fn invalidate_folder(&self, account_id: &str, folder: &str) -> Result<()> {
        let account_id = account_id.to_string();
        let folder = folder.to_string();
        self.db.write_blocking(move |conn: &Connection| {

            conn.execute(
                "delete from search where account_id = ?1 and folder = ?2",
                params![account_id, folder],
            )?;
            conn.execute(
                "delete from bodies where account_id = ?1 and folder = ?2",
                params![account_id, folder],
            )?;
            conn.execute(
                "delete from messages where account_id = ?1 and folder = ?2",
                params![account_id, folder],
            )?;
            conn.execute(
                "update folders set uidvalidity = 0, last_uid = 0 where account_id = ?1 and name = ?2",
                params![account_id, folder],
            )?;
            Ok(())

        })
    }

    /// Forget an account entirely (uninstalled / removed account).
    pub fn forget_account(&self, account_id: &str) -> Result<()> {
        let account_id = account_id.to_string();
        self.db.write_blocking(move |conn: &Connection| {
            for table in ["search", "bodies", "messages", "folders"] {
                conn.execute(
                    &format!("delete from {table} where account_id = ?1"),
                    params![account_id],
                )?;
            }
            Ok(())
        })
    }

    // ------------------------------------------------------------------ messages

    /// Insert or update header rows. Idempotent: re-syncing the same window changes nothing.
    pub fn upsert_summaries(
        &self,
        account_id: &str,
        folder: &str,
        summaries: &[MessageSummary],
    ) -> Result<()> {
        let account_id = account_id.to_string();
        let folder = folder.to_string();
        let summaries = summaries.to_vec();
        self.db.write_blocking(move |conn: &Connection| {
            for summary in summaries {
                // Computed here rather than in the parser: the heuristic key needs to know whether the
                // sender is the account itself, and that fact lives with the account, not the headers.
                let subject_key = crate::threads::subject_key(
                    &summary.subject,
                    summary.from_addr.as_deref().unwrap_or_default(),
                    &account_id,
                );
                let subject_key = (!subject_key.is_empty()).then_some(subject_key);
                conn.execute(
                    "insert into messages
                         (account_id, folder, uid, subject, from_name, from_addr, date, size, seen,
                          flagged, thread_key, subject_key)
                     values (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
                     on conflict(account_id, folder, uid) do update set
                         subject   = excluded.subject,
                         from_name = excluded.from_name,
                         from_addr = excluded.from_addr,
                         date      = excluded.date,
                         size      = excluded.size,
                         seen      = excluded.seen,
                         flagged   = excluded.flagged,
                         thread_key = excluded.thread_key,
                         subject_key = excluded.subject_key",
                    params![
                        account_id,
                        folder,
                        i64::from(summary.uid),
                        summary.subject,
                        summary.from_name,
                        summary.from_addr,
                        summary.date,
                        summary.size.map(i64::from),
                        i64::from(summary.seen),
                        i64::from(summary.flagged),
                        summary.thread_key,
                        subject_key,
                    ],
                )?;
                Self::refresh_search_row(conn, &account_id, &folder, summary.uid)?;
            }
            Ok(())
        })
    }

    /// Newest first, matching what the UI shows.
    pub fn list_summaries(
        &self,
        account_id: &str,
        folder: &str,
        limit: u32,
        offset: u32,
    ) -> Result<Vec<MessageSummary>> {
        let guard = self.db.read_guard();
        let conn: &Connection = &guard;

        let mut stmt = conn.prepare(
            "select uid, subject, from_name, from_addr, date, size, seen, flagged, thread_key,
                    subject_key
             from messages
             where account_id = ?1 and folder = ?2
             order by date desc, uid desc
             limit ?3 offset ?4",
        )?;
        let rows = stmt.query_map(
            params![account_id, folder, i64::from(limit), i64::from(offset)],
            |row| {
                Ok(MessageSummary {
                    uid: row.get::<_, i64>(0)? as u32,
                    subject: row.get(1)?,
                    from_name: row.get(2)?,
                    from_addr: row.get(3)?,
                    date: row.get(4)?,
                    size: row.get::<_, Option<i64>>(5)?.map(|v| v as u32),
                    seen: row.get::<_, i64>(6)? != 0,
                    flagged: row.get::<_, i64>(7)? != 0,
                    // `None` for rows written before the column existed: the renderer falls back to
                    // grouping by subject and sender, which is a guess but a visible one.
                    thread_key: row.get(8)?,
                    subject_key: row.get(9)?,
                })
            },
        )?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Every cached message that belongs to one conversation, oldest first.
    ///
    /// Across folders on purpose (see [`crate::model::ThreadMember`]): half of a conversation is
    /// usually the user's own replies, and those live in the Sent folder. Reading only the folder on
    /// screen would answer "what did they say" while silently dropping "what did I say" — a thread
    /// view that is worse than no thread view, because it looks complete.
    ///
    /// Only rows carrying the exact `thread_key` are returned. Rows written before the column existed
    /// have `None`, and the renderer groups those from the list it already holds (subject plus
    /// counterpart). That is a guess, and it is deliberately not made here, where guessing wrong would
    /// silently merge two unrelated conversations.
    ///
    /// The two conversation signals are **unioned**, not ranked (see [`crate::threads`]): the
    /// `References` chain is a fact, and the subject-and-sender key is a heuristic for mail that has
    /// no chain at all. Starting from whichever key the caller has in hand, each round adds the
    /// signals of everything found so far, so a heuristic group that contains a chain (or a chain
    /// whose root shares a subject with unrelated mail) comes back whole. The loop is bounded: it
    /// stops as soon as a round adds nothing, and four rounds is more than any real group needs.
    ///
    /// Local only — no network — so opening a conversation is a disk read (`design/08` §8). `limit`
    /// exists because "a conversation" has no upper bound in the data, and the caller is a UI.
    pub fn thread_summaries(
        &self,
        account_id: &str,
        thread_key: &str,
        limit: u32,
    ) -> Result<Vec<ThreadMember>> {
        let guard = self.db.read_guard();
        let conn: &Connection = &guard;

        let mut members: Vec<ThreadMember> = Vec::new();
        let mut seen: std::collections::HashSet<(String, u32)> = std::collections::HashSet::new();
        // Each signal is followed once, which is also what makes the loop terminate: a round that
        // finds no *new* signal is the end of the group.
        let mut tried: std::collections::HashSet<Signal> = std::collections::HashSet::new();
        tried.insert(Signal::Chain(thread_key.to_string()));
        let mut pending: Vec<Signal> = vec![Signal::Chain(thread_key.to_string())];

        for _ in 0..4 {
            let mut next: Vec<Signal> = Vec::new();
            for signal in &pending {
                let found = match signal {
                    Signal::Chain(key) => Self::chain_members(conn, account_id, key, limit)?,
                    // The heuristic is bounded in time: only the session that already contains one of
                    // our messages joins, not every message that ever used this subject.
                    Signal::Subject(key) => {
                        Self::subject_session(conn, account_id, key, limit, &seen)?
                    }
                };
                for member in found {
                    let id = (member.folder.clone(), member.summary.uid);
                    if !seen.insert(id) {
                        continue;
                    }
                    for candidate in [
                        member.summary.thread_key.clone().map(Signal::Chain),
                        member.summary.subject_key.clone().map(Signal::Subject),
                    ]
                    .into_iter()
                    .flatten()
                    {
                        if !candidate.is_empty() && tried.insert(candidate.clone()) {
                            next.push(candidate);
                        }
                    }
                    members.push(member);
                }
            }
            if next.is_empty() {
                break;
            }
            pending = next;
        }

        // Oldest first: a conversation is read in the order it happened. The UI is what decides which
        // end to focus, and it needs the whole thing either way.
        members.sort_by_key(|member| (member.summary.date.unwrap_or(0), member.summary.uid));
        members.truncate(limit as usize);
        Ok(members)
    }

    /// Every cached row sharing one `References` root: the exact half of the grouping.
    fn chain_members(
        conn: &Connection,
        account_id: &str,
        thread_key: &str,
        limit: u32,
    ) -> Result<Vec<ThreadMember>> {
        Self::query_members(
            conn,
            "select folder, uid, subject, from_name, from_addr, date, size, seen, flagged,
                    thread_key, subject_key
             from messages
             where account_id = ?1 and thread_key = ?2
             order by date asc, uid asc
             limit ?3",
            params![account_id, thread_key, i64::from(limit)],
        )
    }

    /// The **session** of mail that used one `subject_key` — the heuristic half of the grouping.
    ///
    /// Split by [`THREAD_SESSION_GAP_SECS`]: consecutive messages chain while the gap stays inside the
    /// window, and only the session that already contains one of `seen` is returned (that is how the
    /// caller learned the key, so this is "the conversation this message is part of" rather than
    /// "everything that ever had this subject").
    fn subject_session(
        conn: &Connection,
        account_id: &str,
        subject_key: &str,
        limit: u32,
        seen: &std::collections::HashSet<(String, u32)>,
    ) -> Result<Vec<ThreadMember>> {
        let rows = Self::query_members(
            conn,
            "select folder, uid, subject, from_name, from_addr, date, size, seen, flagged,
                    thread_key, subject_key
             from messages
             where account_id = ?1 and subject_key = ?2
             order by date asc, uid asc
             limit ?3",
            params![account_id, subject_key, i64::from(limit)],
        )?;

        let mut out: Vec<ThreadMember> = Vec::new();
        let mut session: Vec<ThreadMember> = Vec::new();
        let mut previous: Option<i64> = None;
        for member in rows {
            let date = member.summary.date.unwrap_or(0);
            if let Some(last) = previous
                && date.saturating_sub(last) > THREAD_SESSION_GAP_SECS
            {
                flush_session(&mut session, &mut out, seen);
            }
            previous = Some(date);
            session.push(member);
        }
        flush_session(&mut session, &mut out, seen);
        Ok(out)
    }

    /// Read `ThreadMember`s with one prepared query. Shared so the two searches above cannot drift in
    /// the columns they read.
    fn query_members(
        conn: &Connection,
        sql: &str,
        params: &[&dyn rusqlite::ToSql],
    ) -> Result<Vec<ThreadMember>> {
        let mut stmt = conn.prepare(sql)?;
        let rows = stmt.query_map(params, |row| {
            Ok(ThreadMember {
                folder: row.get(0)?,
                summary: MessageSummary {
                    uid: row.get::<_, i64>(1)? as u32,
                    subject: row.get(2)?,
                    from_name: row.get(3)?,
                    from_addr: row.get(4)?,
                    date: row.get(5)?,
                    size: row.get::<_, Option<i64>>(6)?.map(|v| v as u32),
                    seen: row.get::<_, i64>(7)? != 0,
                    flagged: row.get::<_, i64>(8)? != 0,
                    thread_key: row.get(9)?,
                    subject_key: row.get(10)?,
                },
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// The date of the oldest message cached for a folder, if any.
    ///
    /// Used for one thing: when the walk stops and the provider is one that caps how far back a
    /// third-party client may read (A1-F2), this is the **observed** floor — the oldest mail the
    /// account can actually see. Recording it is not a guess about the server; it is what we asked
    /// for and what came back.
    pub fn oldest_date(&self, account_id: &str, folder: &str) -> Result<Option<i64>> {
        let guard = self.db.read_guard();
        let conn: &Connection = &guard;
        Ok(conn.query_row(
            "select min(date) from messages where account_id = ?1 and folder = ?2",
            params![account_id, folder],
            |row| row.get(0),
        )?)
    }
    pub fn summary_count(&self, account_id: &str, folder: &str) -> Result<u32> {
        let guard = self.db.read_guard();
        let conn: &Connection = &guard;

        let count: i64 = conn.query_row(
            "select count(*) from messages where account_id = ?1 and folder = ?2",
            params![account_id, folder],
            |row| row.get(0),
        )?;
        Ok(count as u32)
    }

    /// The lowest UID cached for a folder, i.e. where the next history page starts.
    ///
    /// `None` means the folder has nothing cached. This is what the "load older mail" path asks for
    /// before requesting `ImapClient::fetch_older_headers`.
    pub fn oldest_uid(&self, account_id: &str, folder: &str) -> Result<Option<u32>> {
        let guard = self.db.read_guard();
        let conn: &Connection = &guard;

        let oldest: Option<i64> = conn.query_row(
            "select min(uid) from messages where account_id = ?1 and folder = ?2",
            params![account_id, folder],
            |row| row.get(0),
        )?;
        Ok(oldest.map(|uid| uid as u32))
    }

    /// Messages still marked unread anywhere in this account.
    ///
    /// Read from the cache rather than the server because the tray tooltip and the notification both
    /// need a number immediately, and a round trip per refresh would make them lag the event they
    /// are reporting.
    pub fn unread_count(&self, account_id: &str) -> Result<u32> {
        let guard = self.db.read_guard();
        let conn: &Connection = &guard;

        let count: i64 = conn.query_row(
            "select count(*) from messages where account_id = ?1 and seen = 0",
            params![account_id],
            |row| row.get(0),
        )?;
        Ok(count as u32)
    }

    // ------------------------------------------------------------------ sent copies

    /// Schema version recorded in the file.
    pub fn schema_version(&self) -> Result<i64> {
        let guard = self.db.read_guard();
        let conn: &Connection = &guard;
        Ok(conn.query_row("pragma user_version", [], |row| row.get(0))?)
    }

    /// Remember where a delivered message's Sent copy ended up (`design/07` §5.4).
    ///
    /// Upsert on `(account_id, message_id)`: the archive step refuses to write a second copy, so a
    /// second *record* for the same message can only mean the same copy found again — replacing it is
    /// right, and two rows would make the link ambiguous.
    pub fn record_sent_copy(
        &self,
        account_id: &str,
        message_id: &str,
        folder: &str,
        uid: Option<u32>,
        now: i64,
    ) -> Result<()> {
        let account_id = account_id.to_string();
        let message_id = message_id.to_string();
        let folder = folder.to_string();
        self.db.write_blocking(move |conn: &Connection| {
            conn.execute(
                "insert into sent_index (account_id, message_id, folder, uid, local_at)
                 values (?1, ?2, ?3, ?4, ?5)
                 on conflict(account_id, message_id) do update set
                     folder = excluded.folder, uid = excluded.uid, local_at = excluded.local_at",
                params![account_id, message_id, folder, uid.map(i64::from), now],
            )?;
            Ok(())
        })
    }

    /// Where this message's Sent copy is, if it was ever filed.
    pub fn sent_copy(&self, account_id: &str, message_id: &str) -> Result<Option<SentCopyRecord>> {
        let guard = self.db.read_guard();
        let conn: &Connection = &guard;
        Ok(conn
            .query_row(
                "select account_id, message_id, folder, uid, local_at
                 from sent_index where account_id = ?1 and message_id = ?2",
                params![account_id, message_id],
                |row| {
                    Ok(SentCopyRecord {
                        account_id: row.get(0)?,
                        message_id: row.get(1)?,
                        folder: row.get(2)?,
                        // A `NULL` uid is the documented APPEND outcome (A1-F43), not a missing copy.
                        uid: row.get::<_, Option<i64>>(3)?.map(|uid| uid as u32),
                        local_at: row.get(4)?,
                    })
                },
            )
            .optional()?)
    }

    /// Reflect a flag change locally so the list does not disagree with the server.
    pub fn set_seen(&self, account_id: &str, folder: &str, uid: u32, seen: bool) -> Result<()> {
        let account_id = account_id.to_string();
        let folder = folder.to_string();
        self.db.write_blocking(move |conn: &Connection| {
            conn.execute(
                "update messages set seen = ?4 where account_id = ?1 and folder = ?2 and uid = ?3",
                params![account_id, folder, i64::from(uid), i64::from(seen)],
            )?;
            Ok(())
        })
    }

    /// Remove a message (deleted or moved) from the cache.
    pub fn delete_message(&self, account_id: &str, folder: &str, uid: u32) -> Result<()> {
        let account_id = account_id.to_string();
        let folder = folder.to_string();
        self.db.write_blocking(move |conn: &Connection| {
            // The index row goes **first**, while its id is still readable from `messages`: once the
            // message row is gone there is nothing left to look the index row up by, and finding it
            // any other way means scanning the index (which is the bug this ordering exists to avoid).
            conn.execute(
                "delete from search where rowid =
                     (select search_rowid from messages
                       where account_id = ?1 and folder = ?2 and uid = ?3)",
                params![account_id, folder, i64::from(uid)],
            )?;
            conn.execute(
                "delete from messages where account_id = ?1 and folder = ?2 and uid = ?3",
                params![account_id, folder, i64::from(uid)],
            )?;
            conn.execute(
                "delete from bodies where account_id = ?1 and folder = ?2 and uid = ?3",
                params![account_id, folder, i64::from(uid)],
            )?;
            Ok(())
        })
    }

    // ------------------------------------------------------------------ bodies

    /// Store a parsed message so it can be read again without touching the network.
    pub fn put_detail(&self, account_id: &str, folder: &str, detail: &MessageDetail) -> Result<()> {
        let account_id = account_id.to_string();
        let folder = folder.to_string();
        let detail = detail.clone();
        self.db.write_blocking(move |conn: &Connection| {
            let detail_json = serde_json::to_string(&detail)
                .map_err(|e| MailError::Cache(format!("序列化正文失败：{e}")))?;
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);

            conn.execute(
                "insert into bodies (account_id, folder, uid, detail, fetched_at)
                 values (?1, ?2, ?3, ?4, ?5)
                 on conflict(account_id, folder, uid) do update set
                     detail = excluded.detail, fetched_at = excluded.fetched_at",
                params![account_id, folder, i64::from(detail.uid), detail_json, now],
            )?;

            // The summary table may not carry `to_addrs` yet (headers alone do not include it), and
            // the body is where we learn it. Write it back so recipient search works.
            if !detail.to.is_empty() {
                let to_json = serde_json::to_string(&detail.to)
                    .map_err(|e| MailError::Cache(format!("序列化收件人失败：{e}")))?;
                conn.execute(
                    "update messages set to_addrs = ?4
                     where account_id = ?1 and folder = ?2 and uid = ?3",
                    params![account_id, folder, i64::from(detail.uid), to_json],
                )?;
            }

            Self::refresh_search_row(conn, &account_id, &folder, detail.uid)?;
            Ok(())
        })
    }

    pub fn get_detail(
        &self,
        account_id: &str,
        folder: &str,
        uid: u32,
    ) -> Result<Option<MessageDetail>> {
        let guard = self.db.read_guard();
        let conn: &Connection = &guard;

        let raw: Option<String> = conn
            .query_row(
                "select detail from bodies where account_id = ?1 and folder = ?2 and uid = ?3",
                params![account_id, folder, i64::from(uid)],
                |row| row.get(0),
            )
            .optional()?;

        match raw {
            Some(json) => {
                let mut detail: MessageDetail = serde_json::from_str(&json)
                    .map_err(|e| MailError::Cache(format!("缓存里的正文已损坏：{e}")))?;
                // Rows written before the sanitiser existed still hold the sender's raw HTML, so the
                // guard runs on the way out as well as on the way in. It is idempotent, which is what
                // makes doing it in both places safe (`crate::sanitize`).
                if let Some(html) = detail.body_html.take() {
                    detail.body_html = Some(crate::sanitize::sanitize_mail_html(&html));
                }
                Ok(Some(detail))
            }
            None => Ok(None),
        }
    }

    // ------------------------------------------------------------------ search

    /// Local full-text search over headers (always) and bodies (for messages already read).
    ///
    /// **Driven from the index, joined by rowid.** The obvious shape — `from search join messages m
    /// using (account_id, folder, uid) where search match ?` — is the one that was here, and it is
    /// quadratic: the three join columns are `unindexed` in the FTS5 table, so the planner cannot use
    /// them for a join and the whole thing degrades into a scan. Measured on the 20 000-message
    /// fixture: P50 769 ms, P95 2 380 ms, against a `design/15` budget of 100 ms at P95 on ten times
    /// that data. The subquery takes the matching FTS rowids first (an index lookup) and
    /// `messages_by_search_rowid` turns each into the message row.
    pub fn search(&self, account_id: &str, query: &str, limit: u32) -> Result<Vec<SearchHit>> {
        let guard = self.db.read_guard();
        let conn: &Connection = &guard;

        let expression = to_query(query);
        if expression.is_empty() {
            return Ok(Vec::new());
        }

        let mut stmt = conn.prepare(
            "select m.folder, m.uid, m.subject, m.from_name, m.from_addr, m.date, m.seen
             from (select rowid from search where search match ?1) f
             join messages m on m.search_rowid = f.rowid
             where m.account_id = ?2
             order by m.date desc, m.uid desc
             limit ?3",
        )?;
        let rows = stmt.query_map(params![expression, account_id, i64::from(limit)], |row| {
            Ok(SearchHit {
                folder: row.get(0)?,
                uid: row.get::<_, i64>(1)? as u32,
                subject: row.get(2)?,
                from_name: row.get(3)?,
                from_addr: row.get(4)?,
                date: row.get(5)?,
                seen: row.get::<_, i64>(6)? != 0,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Rebuild one FTS row from whatever is currently cached for that message.
    ///
    /// FTS5 has no upsert, so this deletes then inserts. Done inside the caller's flow rather than
    /// with triggers because the indexed text has to be transformed in Rust first.
    /// Rebuild one FTS row **inside the caller's transaction**.
    ///
    /// An associated function taking `conn` rather than a `&self` method: a `&self` call from inside
    /// a `write_blocking` closure would capture the handle and turn "one transaction per batch" back
    /// into one per row (and would not compile, since the closure must be `'static`).
    fn refresh_search_row(
        conn: &Connection,
        account_id: &str,
        folder: &str,
        uid: u32,
    ) -> Result<()> {
        // `m.rowid` and `m.search_rowid` come back too: the index is keyed **by rowid** now.
        //
        // This is the fix for a real performance bug, found on 2026-10-06 by the fixture benchmark.
        // The delete used to be `delete from search where account_id = ? and folder = ? and uid = ?`,
        // and those three columns are `unindexed` in the FTS5 table — `unindexed` means "not
        // searchable", not "cheap to filter", so every delete scanned the whole index. A sync of N
        // messages therefore did O(N²) row visits: measured 1.4 s for 2 000 messages and, by that
        // curve, about an hour for 100 000. `design/A2` §2.1 had already flagged the risk ("987 ms
        // per row on delete"); the implementation had kept the slow form, and nothing measured it
        // until the fixture existed.
        let row: Option<(Option<i64>, IndexSource)> = conn
            .query_row(
                "select m.search_rowid, m.subject, m.from_name, m.from_addr, m.to_addrs, b.detail
                     from messages m
                     left join bodies b
                       on b.account_id = m.account_id and b.folder = m.folder and b.uid = m.uid
                     where m.account_id = ?1 and m.folder = ?2 and m.uid = ?3",
                params![account_id, folder, i64::from(uid)],
                |row| {
                    Ok((
                        row.get(0)?,
                        IndexSource {
                            subject: row.get(1)?,
                            from_name: row.get(2)?,
                            from_addr: row.get(3)?,
                            to_addrs: row.get(4)?,
                            detail_json: row.get(5)?,
                        },
                    ))
                },
            )
            .optional()?;

        // A row with no `messages` entry never reaches here: the callers that delete a message
        // (`delete_message`, `invalidate_folder`, `forget_account`) remove the index row in the same
        // transaction, and they do it first, while the `search_rowid` is still readable.
        let Some((existing_rowid, source)) = row else {
            return Ok(());
        };

        if let Some(existing) = existing_rowid {
            conn.execute("delete from search where rowid = ?1", params![existing])?;
        }

        // Reuse the row's own id when it has one, so the index stays stable across re-indexing
        // (a body arriving later re-runs this whole function). A brand new row takes the next free
        // id: `max(rowid)` is a b-tree lookup, not a scan.
        let rowid: i64 = match existing_rowid {
            Some(existing) => existing,
            None => conn.query_row("select ifnull(max(rowid), 0) + 1 from search", [], |row| {
                row.get(0)
            })?,
        };

        // The body lives inside the serialised detail; pull just the text out of it.
        let body = source
            .detail_json
            .as_deref()
            .and_then(|json| serde_json::from_str::<MessageDetail>(json).ok())
            .and_then(|detail| detail.body_text)
            .unwrap_or_default();

        conn.execute(
            "insert into search
                 (rowid, subject, from_name, from_addr, to_addrs, body, account_id, folder, uid)
             values (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                rowid,
                to_tokens(&source.subject),
                to_tokens(source.from_name.as_deref().unwrap_or_default()),
                to_tokens(source.from_addr.as_deref().unwrap_or_default()),
                to_tokens(source.to_addrs.as_deref().unwrap_or_default()),
                to_tokens(&body),
                account_id,
                folder,
                i64::from(uid),
            ],
        )?;

        // Remember which index row belongs to this message, so the *next* refresh (and every
        // deletion) can address it by rowid instead of scanning the index.
        conn.execute(
            "update messages set search_rowid = ?4
             where account_id = ?1 and folder = ?2 and uid = ?3",
            params![account_id, folder, i64::from(uid), rowid],
        )?;
        Ok(())
    }
}

/// Throw the index away and build it again from `messages` (and `bodies`).
///
/// Used by the v6 migration, whose whole point is that old rows had no `search_rowid` to delete by.
/// One pass, one transaction: the identities are collected first because the loop writes to
/// `messages` while a statement over it would still be open.
fn rebuild_search_index(conn: &Connection) -> Result<()> {
    conn.execute("delete from search", [])?;
    conn.execute("update messages set search_rowid = null", [])?;

    let rows: Vec<(String, String, u32)> = {
        let mut stmt = conn.prepare("select account_id, folder, uid from messages")?;
        let mapped = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)? as u32,
            ))
        })?;
        mapped.collect::<rusqlite::Result<Vec<_>>>()?
    };

    for (account_id, folder, uid) in rows {
        Cache::refresh_search_row(conn, &account_id, &folder, uid)?;
    }
    Ok(())
}

/// Add a column to an existing table, once.
///
/// How far apart two messages under the same subject may be and still read as one conversation.
///
/// The heuristic needs a bound, and a real mailbox says why: the case that asked for it is four
/// messages inside one day, while recurring notifications reuse one subject for years — Google's
/// 安全提醒 appeared 24 times over **4.7 years**, an "unusual sign-in" alert 11 times over 52 days.
/// Unbounded, the guess folds all of those into single rows.
///
/// A week is the threshold mail clients converge on for subject-based threading: long enough for a
/// real burst of mail (four messages over two days, a bank's five transaction notices inside a day),
/// short enough that "the same subject next month" is a new conversation.
pub const THREAD_SESSION_GAP_SECS: i64 = 7 * 24 * 60 * 60;

/// One of the two things that can join two messages into one conversation.
///
/// A type rather than a string with a prefix, because the two are followed by **different rules**: a
/// chain is followed to the end, a subject is followed only inside its session. Keeping them apart is
/// also what stops a chain key and a subject key from ever being compared to each other.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum Signal {
    Chain(String),
    Subject(String),
}

impl Signal {
    fn is_empty(&self) -> bool {
        match self {
            Signal::Chain(key) | Signal::Subject(key) => key.is_empty(),
        }
    }
}

/// Keep a finished session only if it holds a message we already have.
///
/// That is the whole "which session is mine" rule: the caller learned the subject key *from* a message
/// it had already found, so the session containing that message is the one the user opened, and a
/// session months earlier that happens to share the subject is a different conversation.
fn flush_session(
    session: &mut Vec<ThreadMember>,
    out: &mut Vec<ThreadMember>,
    seen: &std::collections::HashSet<(String, u32)>,
) {
    if session
        .iter()
        .any(|member| seen.contains(&(member.folder.clone(), member.summary.uid)))
    {
        out.append(session);
    }
    session.clear();
}

/// Fill `subject_key` for every row that has none, reading nothing but the row itself.
///
/// The v5 migration's one-time pass. It is deliberately not a sync: subject, sender and the account's
/// address are all in `messages`, so already-downloaded mail becomes groupable immediately. Rows that
/// legitimately get no key (the user's own mail, a subject-less message) stay `NULL` and simply do not
/// participate in the heuristic.
fn backfill_subject_keys(conn: &Connection) -> Result<()> {
    let rows: Vec<(String, String, i64, String, Option<String>)> = {
        let mut stmt = conn.prepare(
            "select account_id, folder, uid, subject, from_addr
             from messages where subject_key is null",
        )?;
        stmt.query_map([], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?
    };

    for (account_id, folder, uid, subject, from_addr) in rows {
        let key = crate::threads::subject_key(
            &subject,
            from_addr.as_deref().unwrap_or_default(),
            &account_id,
        );
        if key.is_empty() {
            continue;
        }
        conn.execute(
            "update messages set subject_key = ?4
             where account_id = ?1 and folder = ?2 and uid = ?3",
            params![account_id, folder, uid, key],
        )?;
    }
    Ok(())
}

/// SQLite has no `alter table ... add column if not exists`, so the check has to be made explicitly.
/// Doing it by asking the schema rather than by trusting the recorded version is deliberate: the two
/// can disagree (an interrupted migration, a file copied between versions), and the schema is what
/// the query will actually run against.
fn add_column_if_missing(conn: &Connection, table: &str, column: &str, kind: &str) -> Result<()> {
    let mut stmt = conn.prepare(&format!("pragma table_info({table})"))?;
    let existing = stmt
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    if !existing.iter().any(|name| name == column) {
        conn.execute_batch(&format!("alter table {table} add column {column} {kind};"))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::InlineImage;

    fn summary(uid: u32, subject: &str, from_addr: &str, date: i64) -> MessageSummary {
        MessageSummary {
            uid,
            subject: subject.to_string(),
            from_name: Some("发件人".to_string()),
            from_addr: Some(from_addr.to_string()),
            date: Some(date),
            size: Some(1024),
            seen: false,
            flagged: false,
            thread_key: None,
            subject_key: None,
        }
    }

    fn detail(uid: u32, subject: &str, body_text: &str) -> MessageDetail {
        MessageDetail {
            uid,
            subject: subject.to_string(),
            from_name: Some("发件人".to_string()),
            from_addr: Some("a@example.com".to_string()),
            to: vec!["me@example.com".to_string()],
            cc: Vec::new(),
            reply_to: None,
            date: Some(1_700_000_000),
            message_id: Some("m1@example.com".to_string()),
            in_reply_to: None,
            references: Vec::new(),
            body_text: Some(body_text.to_string()),
            body_html: Some("<p>html</p>".to_string()),
            attachments: Vec::new(),
            inline_images: Vec::<InlineImage>::new(),
        }
    }

    #[test]
    fn schema_is_created_with_fts5_available() {
        // If FTS5 were missing from the bundled SQLite build, `migrate` would fail here rather than
        // in some later query.
        Cache::open_in_memory().expect("FTS5 must be available");
    }

    #[test]
    fn two_character_chinese_words_are_searchable() {
        // The case that rules out the trigram tokenizer: 账单 is two characters.
        let cache = Cache::open_in_memory().unwrap();
        cache
            .upsert_summaries(
                "acc",
                "INBOX",
                &[summary(1, "十月的账单提醒", "bank@example.com", 100)],
            )
            .unwrap();

        let hits = cache.search("acc", "账单", 10).unwrap();
        assert_eq!(hits.len(), 1, "2-char CJK search must work");
        assert_eq!(hits[0].uid, 1);
        assert_eq!(hits[0].folder, "INBOX");
    }

    #[test]
    fn search_covers_sender_subject_and_body_and_is_ordered_newest_first() {
        let cache = Cache::open_in_memory().unwrap();
        cache
            .upsert_summaries(
                "acc",
                "INBOX",
                &[
                    summary(1, "月度对账单", "bank@example.com", 100),
                    summary(2, "开会通知", "boss@example.com", 200),
                    summary(3, "发票已开具", "invoice@example.com", 300),
                ],
            )
            .unwrap();
        // Only message 2 has a body cached, i.e. only it has been read.
        cache
            .put_detail("acc", "INBOX", &detail(2, "开会通知", "记得带季度报表"))
            .unwrap();

        assert_eq!(cache.search("acc", "boss", 10).unwrap()[0].uid, 2, "sender");
        assert_eq!(
            cache.search("acc", "发票", 10).unwrap()[0].uid,
            3,
            "subject"
        );
        assert_eq!(
            cache.search("acc", "季度报表", 10).unwrap()[0].uid,
            2,
            "body, and it must be the body text we cached"
        );

        // Two matches, newest first.
        let both = cache.search("acc", "example.com", 10).unwrap();
        assert!(both.len() >= 2);
        assert!(both[0].date >= both[1].date, "newest first");
    }

    #[test]
    fn the_index_and_the_query_use_the_same_transformation() {
        // Guards the single most likely way to break search: changing one side only. If
        // `to_tokens` were ever replaced by something word-based while `to_query` stayed
        // character-based, this query would stop matching.
        let cache = Cache::open_in_memory().unwrap();
        cache
            .upsert_summaries("acc", "INBOX", &[summary(1, "账单", "a@b.c", 1)])
            .unwrap();
        assert_eq!(cache.search("acc", "账单", 5).unwrap().len(), 1);

        // And the transformation is applied to the query as well.
        assert_eq!(to_query("账单"), "\"账 单\"");
        assert_eq!(to_tokens("账单"), "账 单");
    }

    #[test]
    fn multiple_terms_are_anded_and_quotes_are_stripped() {
        assert_eq!(to_query("账单 发票"), "\"账 单\" AND \"发 票\"");
        // A quote would otherwise be FTS5 syntax and blow up the whole MATCH expression.
        assert_eq!(to_query("\"账单\""), "\"账 单\"");
        assert_eq!(to_query("   "), "");
    }

    // ============================================================== pragmas / vacuum (M0-7)

    /// A database this code creates must come back with auto-vacuum actually on.
    ///
    /// The failure this pins is silent: `pragma auto_vacuum = incremental` on a database that
    /// already has tables is **ignored** (SQLite only honours it on an empty file or after a full
    /// VACUUM), so the setting looks applied while `incremental_vacuum` keeps doing nothing.
    #[test]
    fn a_new_database_really_gets_incremental_auto_vacuum() {
        // A **file**, not `open_in_memory`: auto-vacuum lives in the database header, and an
        // in-memory database has no header — it always reports 0, so testing it there would be
        // testing nothing.
        let (cache, path) = temp_cache("auto-vacuum");
        assert_eq!(
            cache.auto_vacuum_state().expect("state"),
            2,
            "auto_vacuum must read back as incremental (2), not 0 — the pragma has to precede DDL"
        );
        drop(cache);
        let _ = std::fs::remove_file(&path);
    }

    /// A cache in a temporary file, plus its path. File-backed because half of the storage
    /// behaviour under test (auto-vacuum, freelist reclamation) does not exist in memory.
    fn temp_cache(name: &str) -> (Cache, std::path::PathBuf) {
        let path = std::env::temp_dir().join(format!(
            "easy-email-{name}-{}-{}.sqlite",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        (Cache::open(&path).expect("open"), path)
    }

    /// Deleting rows must hand pages back to the freelist, and the incremental vacuum must then
    /// release them. This is what makes "清理缓存释放了 N MB" true on disk rather than only in the
    /// UI's arithmetic.
    #[test]
    fn cleanup_frees_pages_and_incremental_vacuum_releases_them() {
        let (cache, path) = temp_cache("freelist");
        let chunky = "x".repeat(4096);
        for uid in 1..=400u32 {
            cache
                .upsert_summaries("acc", "INBOX", &[mail_core_model_summary(uid, &chunky)])
                .expect("insert");
        }
        let before = cache.freelist_bytes().expect("freelist");

        // Through the public API: the test is about what cleanup does to the file, not about the
        // storage internals.
        cache.forget_account("acc").expect("delete the rows");
        let freed = cache.freelist_bytes().expect("freelist after delete");
        assert!(
            freed > before,
            "deleting rows must put pages on the freelist ({before} -> {freed})"
        );

        let remaining = cache.vacuum_into_freelist().expect("incremental vacuum");
        assert!(
            remaining < freed.max(1),
            "an incremental vacuum must consume freelist pages on an auto-vacuum database \
             (freelist went {freed} pages -> {remaining})"
        );
        drop(cache);
        let _ = std::fs::remove_file(&path);
    }

    /// A cache from v0.2 must be *detected*, not silently left unable to reclaim space.
    #[test]
    fn an_old_cache_is_flagged_for_the_one_time_rebuild() {
        let path = std::env::temp_dir().join(format!(
            "easy-email-needs-rebuild-{}-{}.sqlite",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        {
            let conn = Connection::open(&path).expect("legacy open");
            conn.execute_batch(&format!(
                "pragma auto_vacuum = none;
                 pragma user_version = {SCHEMA_VERSION};
                 create table messages (
                     account_id text not null, folder text not null, uid integer not null,
                     date integer, primary key (account_id, folder, uid)
                 );"
            ))
            .expect("legacy schema");
        }

        let cache = Cache::open(&path).expect("open");
        assert!(
            cache.needs_auto_vacuum_rebuild().expect("check"),
            "a v0.2 file must be flagged so the app can rebuild it once"
        );
        cache.rebuild_with_auto_vacuum().expect("rebuild");
        assert!(!cache.needs_auto_vacuum_rebuild().expect("check"));
        drop(cache);
        let _ = std::fs::remove_file(&path);
    }

    /// `design/07` §5.4: the link from a delivered message to its copy in 已发送.
    #[test]
    fn a_sent_copy_is_recorded_and_found_again() {
        let cache = Cache::open_in_memory().expect("open");
        assert_eq!(
            cache.sent_copy("acc", "<a@x>").expect("read"),
            None,
            "nothing recorded yet"
        );

        cache
            .record_sent_copy("acc", "<a@x>", "Sent", Some(4242), 100)
            .expect("record");
        let found = cache
            .sent_copy("acc", "<a@x>")
            .expect("read")
            .expect("present");
        assert_eq!(found.folder, "Sent");
        assert_eq!(found.uid, Some(4242));

        // Another account's message with the same id is a different row: the index is per account.
        assert_eq!(cache.sent_copy("other", "<a@x>").expect("read"), None);

        // Recording the same message twice must not produce two rows — the archive step refuses to
        // write a second copy, so a second record is the same copy found again.
        cache
            .record_sent_copy("acc", "<a@x>", "Sent", None, 200)
            .expect("record again");
        let again = cache
            .sent_copy("acc", "<a@x>")
            .expect("read")
            .expect("present");
        assert_eq!(again.uid, None, "the later observation wins");
        assert_eq!(again.local_at, 200);
    }

    #[test]
    fn a_sent_copy_without_a_known_uid_is_still_a_copy() {
        // APPEND does not return a UID (A1-F43), so "we filed it but cannot number it" has to be
        // representable — otherwise the UI would offer to look up a message that is already there.
        let cache = Cache::open_in_memory().expect("open");
        cache
            .record_sent_copy("acc", "<b@x>", "Sent Messages", None, 1)
            .expect("record");
        let found = cache
            .sent_copy("acc", "<b@x>")
            .expect("read")
            .expect("present");
        assert_eq!(found.uid, None);
        assert_eq!(found.folder, "Sent Messages");
    }

    /// A cache written by the previous schema gains `sent_index` on open.
    ///
    /// The table is additive, so the same rule as `user.db` applies: `create table if not exists` is
    /// the migration, and this test is what keeps somebody from turning that into a version check that
    /// refuses to open old files (which would throw away every user's cached mail).
    #[test]
    fn an_older_cache_gains_the_sent_index_on_open() {
        let path = std::env::temp_dir().join(format!(
            "easy-email-sent-index-{}-{}.sqlite",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));

        // A file at the previous schema version, without the new table.
        {
            let cache = Cache::open(&path).expect("open");
            assert_eq!(cache.schema_version().expect("version"), SCHEMA_VERSION);
            cache
                .record_sent_copy("acc", "<c@x>", "Sent", Some(7), 1)
                .expect("record");
        }
        {
            let db = Db::open(
                &path,
                crate::db::DbKind::Cache,
                crate::db::DbOptions::for_kind(crate::db::DbKind::Cache),
            )
            .expect("db");
            db.write_blocking(|conn: &Connection| {
                conn.execute_batch("drop table sent_index; pragma user_version = 1;")?;
                Ok(())
            })
            .expect("downgrade");
        }

        let upgraded = Cache::open(&path).expect("reopen");
        assert_eq!(
            upgraded.schema_version().expect("version"),
            SCHEMA_VERSION,
            "opening must bring the schema up, not refuse the file"
        );
        assert!(
            upgraded.sent_copy("acc", "<c@x>").expect("read").is_none(),
            "the dropped rows are gone, which is fine: this is derived data"
        );
        upgraded
            .record_sent_copy("acc", "<d@x>", "Sent", Some(8), 2)
            .expect("the new table must be usable");
        assert!(upgraded.sent_copy("acc", "<d@x>").expect("read").is_some());
        drop(upgraded);
        let _ = std::fs::remove_file(&path);
    }

    /// The first *column* this schema has ever added.
    ///
    /// A new table arrives through `create table if not exists`, but a new column cannot: against an
    /// existing table SQLite keeps the old definition and ignores the new one, so the statement
    /// succeeds and the column never appears. This test is what keeps that from being discovered by
    /// a user whose cache was written by the previous version.
    #[test]
    fn an_older_cache_gains_the_thread_key_column_on_open() {
        let path = std::env::temp_dir().join(format!(
            "easy-email-thread-key-{}-{}.sqlite",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));

        // A v2 file: the `messages` table without `thread_key`, and a row already in it.
        {
            let conn = Connection::open(&path).expect("legacy open");
            conn.execute_batch(
                "pragma user_version = 2;
                 create table messages (
                     account_id text not null, folder text not null, uid integer not null,
                     subject text not null default '', from_name text, from_addr text,
                     to_addrs text, date integer, size integer, seen integer not null default 0,
                     flagged integer not null default 0,
                     primary key (account_id, folder, uid)
                 );
                 -- Present in v2 as well: the search index is what `upsert_summaries` writes to, and
                 -- a fixture that omits it would fail on the first write for the wrong reason.
                 create virtual table search using fts5(
                     subject, from_name, from_addr, to_addrs, body,
                     account_id unindexed, folder unindexed, uid unindexed,
                     tokenize = 'unicode61'
                 );
                 insert into messages (account_id, folder, uid, subject, seen, flagged)
                 values ('acc', 'INBOX', 7, 'OLD', 0, 0);",
            )
            .expect("legacy schema");
        }

        let cache = Cache::open(&path).expect("opening must migrate, not refuse");
        assert_eq!(cache.schema_version().expect("version"), SCHEMA_VERSION);
        let rows = cache.list_summaries("acc", "INBOX", 10, 0).expect("read");
        assert_eq!(
            rows.len(),
            1,
            "the row written before the migration survives"
        );
        assert_eq!(rows[0].subject, "OLD");
        assert_eq!(
            rows[0].thread_key, None,
            "a row that predates the column has no key, and the renderer falls back for it"
        );

        // The conversation index exists, and it was created **after** the column it indexes. That
        // ordering is the whole reason this assertion is here: a v2 file arrives at the migration with
        // a `messages` table that has no `thread_key`, so indexing it alongside the other schema would
        // fail with "no such column" and take the entire upgrade down with it.
        {
            let guard = cache.db.read_guard();
            let indexed: i64 = guard
                .query_row(
                    "select count(*) from sqlite_master where type = 'index' \
                     and name = 'messages_by_thread'",
                    [],
                    |row| row.get(0),
                )
                .expect("schema");
            assert_eq!(
                indexed, 1,
                "the conversation index must exist after upgrade"
            );
        }

        // And the column is usable from here on.
        let mut fresh = rows[0].clone();
        fresh.uid = 8;
        fresh.thread_key = Some("<root@x>".to_string());
        cache
            .upsert_summaries("acc", "INBOX", &[fresh])
            .expect("write");
        let after = cache.list_summaries("acc", "INBOX", 10, 0).expect("read");
        assert!(
            after
                .iter()
                .any(|row| row.thread_key.as_deref() == Some("<root@x>"))
        );
        drop(cache);
        let _ = std::fs::remove_file(&path);
    }

    /// A conversation is assembled across folders, and never across accounts.
    ///
    /// The cross-folder half is the feature: the user's own replies are in the Sent folder, so a
    /// thread read from one folder would show an argument in which only the other side speaks. The
    /// account half is the safety rail, because a thread key is only unique within an account.
    #[test]
    fn a_conversation_crosses_folders_and_stops_at_the_account() {
        let (cache, path) = temp_cache("thread-members");
        let key = "<root@example.com>";

        let mut root = summary(10, "项目进度", "boss@example.com", 1_000);
        root.thread_key = Some(key.to_string());
        let mut reply = summary(2, "Re: 项目进度", "me@example.com", 2_000);
        reply.thread_key = Some(key.to_string());
        let mut unrelated = summary(3, "别的会话", "someone@example.com", 3_000);
        unrelated.thread_key = Some("<other@example.com>".to_string());
        let mut foreign = summary(4, "Re: 项目进度", "me@example.com", 4_000);
        foreign.thread_key = Some(key.to_string());

        cache
            .upsert_summaries("acc", "INBOX", &[root, unrelated])
            .expect("write");
        // The same UID space, a different folder: this is where the reply actually lives.
        cache
            .upsert_summaries("acc", "Sent Messages", &[reply])
            .expect("write");
        cache
            .upsert_summaries("other", "INBOX", &[foreign])
            .expect("write");

        let members = cache.thread_summaries("acc", key, 100).expect("read");
        assert_eq!(members.len(), 2, "{members:?}");
        // Oldest first — a conversation reads in the order it happened.
        assert_eq!(members[0].folder, "INBOX");
        assert_eq!(members[0].summary.uid, 10);
        assert_eq!(members[1].folder, "Sent Messages");
        assert_eq!(members[1].summary.uid, 2);

        assert_eq!(
            cache
                .thread_summaries("acc", "<other@example.com>", 100)
                .expect("read")
                .len(),
            1,
            "a different conversation is a different list"
        );
        assert!(
            cache
                .thread_summaries("acc", "<nobody@example.com>", 100)
                .expect("read")
                .is_empty(),
            "an unknown key is an empty conversation, not an error"
        );
        assert_eq!(
            cache.thread_summaries("acc", key, 1).expect("read").len(),
            1,
            "the limit bounds the read"
        );

        drop(cache);
        let _ = std::fs::remove_file(&path);
    }

    /// The heuristic half of the grouping, on real rows: mail with **no** `References` chain that came
    /// from the same person with the same subject is one conversation.
    ///
    /// The reported case (2026-10-06): four separate messages from one correspondent, one subject,
    /// none of them a reply to anything — four rows in the list. They are grouped now, and the reply
    /// that *does* carry a chain still joins its own thread.
    #[test]
    fn mail_with_no_chain_is_grouped_by_sender_and_subject() {
        let cache = Cache::open_in_memory().unwrap();
        let me = "acc";

        // Four independent messages, each its own root, all from the same person with one subject.
        let mut first = summary(1, "王芳 您好", "wangfang@example.com", 1_000);
        first.thread_key = Some("<a@outlook>".to_string());
        let mut second = summary(2, "王芳 您好", "wangfang@example.com", 2_000);
        second.thread_key = Some("<b@outlook>".to_string());
        // A reply-shaped subject is the same conversation.
        let mut third = summary(3, "Re: 王芳 您好", "wangfang@example.com", 3_000);
        third.thread_key = Some("<c@outlook>".to_string());
        // Same subject, a different person: a different conversation.
        let mut stranger = summary(4, "王芳 您好", "someone@else.com", 4_000);
        stranger.thread_key = Some("<d@else>".to_string());
        // The user's own mail never joins by subject, even with the same subject and no chain.
        let mut mine = summary(5, "王芳 您好", me, 5_000);
        mine.thread_key = Some("<e@qq>".to_string());

        cache
            .upsert_summaries(me, "INBOX", &[first, second, third, stranger, mine])
            .expect("write");

        let members = cache
            .thread_summaries(me, "<a@outlook>", 100)
            .expect("read");
        assert_eq!(
            members.iter().map(|m| m.summary.uid).collect::<Vec<_>>(),
            vec![1, 2, 3],
            "the three from that sender are one conversation, oldest first: {members:?}"
        );

        assert_eq!(
            cache
                .thread_summaries(me, "<d@else>", 100)
                .expect("read")
                .len(),
            1,
            "a different sender is a different conversation"
        );
        assert_eq!(
            cache
                .thread_summaries(me, "<e@qq>", 100)
                .expect("read")
                .len(),
            1,
            "the user's own mail is not folded in by subject"
        );

        // And the write path stores the signal, so the renderer can group the same way it does here.
        let listed = cache.list_summaries(me, "INBOX", 10, 0).expect("read");
        let stored: Vec<Option<String>> = listed
            .iter()
            .map(|summary| summary.subject_key.clone())
            .collect();
        assert!(
            stored.iter().filter(|key| key.is_some()).count() == 4,
            "four rows carry the heuristic key, the user's own does not: {stored:?}"
        );
    }

    /// The heuristic is bounded in time, and a real mailbox is why: recurring notifications reuse one
    /// subject for years (Google's 安全提醒, 24 times over 4.7 years). Only a burst counts.
    #[test]
    fn the_subject_heuristic_stops_at_the_session_gap() {
        let cache = Cache::open_in_memory().unwrap();
        let day = 86_400;
        let mut burst_a = summary(1, "发票", "billing@example.com", day * 10);
        burst_a.thread_key = Some("<a@x>".to_string());
        let mut burst_b = summary(2, "发票", "billing@example.com", day * 10 + 3_600);
        burst_b.thread_key = Some("<b@x>".to_string());
        // Two days later: still the same session.
        let mut burst_c = summary(3, "发票", "billing@example.com", day * 12);
        burst_c.thread_key = Some("<c@x>".to_string());
        // More than a week after that: the same subject, a different conversation.
        let mut much_later = summary(4, "发票", "billing@example.com", day * 40);
        much_later.thread_key = Some("<d@x>".to_string());

        cache
            .upsert_summaries("acc", "INBOX", &[burst_a, burst_b, burst_c, much_later])
            .expect("write");

        let members = cache.thread_summaries("acc", "<a@x>", 100).expect("read");
        assert_eq!(
            members.iter().map(|m| m.summary.uid).collect::<Vec<_>>(),
            vec![1, 2, 3],
            "the burst is one conversation: {members:?}"
        );
        assert_eq!(
            cache
                .thread_summaries("acc", "<d@x>", 100)
                .expect("read")
                .iter()
                .map(|m| m.summary.uid)
                .collect::<Vec<_>>(),
            vec![4],
            "mail six weeks later shares the subject but not the conversation"
        );
    }

    /// The upgrade path for databases created before M0-7: the pragma cannot be enabled in place,
    /// so the only correct answer is a full rebuild — and the helper must be idempotent.
    #[test]
    fn an_old_database_can_be_rebuilt_into_auto_vacuum() {
        // Simulate a v0.2 database **as a file**: tables first, auto-vacuum left at its default.
        let path = std::env::temp_dir().join(format!(
            "easy-email-legacy-{}-{}.sqlite",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        {
            let conn = Connection::open(&path).expect("legacy open");
            // A realistic v0.2 shape: `pragma user_version` at the current value (so the migration
            // is a no-op) and a `messages` table with the columns the schema's indexes need —
            // otherwise `migrate` would fail on an index over missing columns, which is a different
            // test than the one intended here.
            conn.execute_batch(&format!(
                "pragma auto_vacuum = none;
                 pragma user_version = {SCHEMA_VERSION};
                 create table messages (
                     account_id text not null,
                     folder     text not null,
                     uid        integer not null,
                     date       integer,
                     primary key (account_id, folder, uid)
                 );"
            ))
            .expect("legacy schema");
        }

        let cache = Cache::open(&path).expect("open the legacy file");
        assert_eq!(cache.auto_vacuum_state().expect("state"), 0);

        // Turning it on directly is silently ignored — that is the trap, so assert it rather than
        // trusting the documentation.
        cache
            .db
            .exec_blocking(|conn| {
                conn.execute_batch("pragma auto_vacuum = incremental;")?;
                Ok(())
            })
            .expect("pragma");
        assert_eq!(
            cache.auto_vacuum_state().expect("state"),
            0,
            "SQLite must ignore the pragma on a non-empty database; if this ever passes, the \
             rebuild step below is no longer needed"
        );

        cache.rebuild_with_auto_vacuum().expect("rebuild");
        assert_eq!(cache.auto_vacuum_state().expect("state"), 2);
        cache
            .rebuild_with_auto_vacuum()
            .expect("rebuild is idempotent");
        assert_eq!(cache.auto_vacuum_state().expect("state"), 2);
        drop(cache);
        let _ = std::fs::remove_file(&path);
    }

    fn mail_core_model_summary(uid: u32, subject: &str) -> crate::model::MessageSummary {
        crate::model::MessageSummary {
            uid,
            subject: subject.to_string(),
            from_name: None,
            from_addr: Some("a@example.com".to_string()),
            date: Some(1_700_000_000 + uid as i64),
            size: Some(4096),
            seen: false,
            flagged: false,
            thread_key: None,
            subject_key: None,
        }
    }

    #[test]
    fn a_query_of_only_quotes_returns_nothing_instead_of_failing() {
        let cache = Cache::open_in_memory().unwrap();
        cache
            .upsert_summaries("acc", "INBOX", &[summary(1, "账单", "a@b.c", 1)])
            .unwrap();
        assert!(cache.search("acc", "\"", 5).unwrap().is_empty());
        assert!(cache.search("acc", "   ", 5).unwrap().is_empty());
    }

    #[test]
    fn upserting_the_same_batch_twice_changes_nothing() {
        let cache = Cache::open_in_memory().unwrap();
        let batch = [
            summary(1, "账单", "a@b.c", 100),
            summary(2, "发票", "d@e.f", 200),
        ];
        cache.upsert_summaries("acc", "INBOX", &batch).unwrap();
        cache.upsert_summaries("acc", "INBOX", &batch).unwrap();

        assert_eq!(cache.summary_count("acc", "INBOX").unwrap(), 2);
        // The FTS index must not have doubled up either: a duplicated row would return the hit twice.
        assert_eq!(cache.search("acc", "账单", 10).unwrap().len(), 1);
        assert_eq!(
            cache.list_summaries("acc", "INBOX", 10, 0).unwrap().len(),
            2
        );
    }

    #[test]
    fn the_oldest_date_is_what_the_floor_is_derived_from() {
        let cache = Cache::open_in_memory().expect("open");
        assert_eq!(cache.oldest_date("acc", "INBOX").expect("read"), None);

        cache
            .upsert_summaries(
                "acc",
                "INBOX",
                &[
                    summary(1, "newer", "a@example.com", 2_000),
                    summary(2, "oldest", "a@example.com", 900),
                    summary(3, "middle", "a@example.com", 1_500),
                ],
            )
            .expect("store");
        assert_eq!(cache.oldest_date("acc", "INBOX").expect("read"), Some(900));
        assert_eq!(
            cache.oldest_date("acc", "OTHER").expect("read"),
            None,
            "and it is per folder"
        );
    }
    #[test]
    fn summaries_come_back_newest_first_with_paging() {
        let cache = Cache::open_in_memory().unwrap();
        let batch: Vec<MessageSummary> = (1..=5)
            .map(|uid| summary(uid, "主题", "a@b.c", i64::from(uid) * 100))
            .collect();
        cache.upsert_summaries("acc", "INBOX", &batch).unwrap();

        let first = cache.list_summaries("acc", "INBOX", 2, 0).unwrap();
        assert_eq!(first.iter().map(|m| m.uid).collect::<Vec<_>>(), vec![5, 4]);
        let second = cache.list_summaries("acc", "INBOX", 2, 2).unwrap();
        assert_eq!(second.iter().map(|m| m.uid).collect::<Vec<_>>(), vec![3, 2]);
    }

    #[test]
    fn a_body_round_trips_including_an_inline_image() {
        let cache = Cache::open_in_memory().unwrap();
        cache
            .upsert_summaries("acc", "INBOX", &[summary(7, "带图", "a@b.c", 1)])
            .unwrap();

        let mut body = detail(7, "带图", "正文");
        body.inline_images = vec![InlineImage {
            content_id: "logo@x".to_string(),
            mime: "image/png".to_string(),
            data_base64: "AAAA".to_string(),
        }];
        cache.put_detail("acc", "INBOX", &body).unwrap();

        let read = cache
            .get_detail("acc", "INBOX", 7)
            .unwrap()
            .expect("cached");
        assert_eq!(read.body_html.as_deref(), Some("<p>html</p>"));
        assert_eq!(read.inline_images.len(), 1);
        assert_eq!(read.inline_images[0].content_id, "logo@x");
        assert!(cache.get_detail("acc", "INBOX", 999).unwrap().is_none());
    }

    #[test]
    fn recipient_search_works_once_a_body_has_been_read() {
        // Headers alone do not carry To:, so this only works after `put_detail` writes it back.
        let cache = Cache::open_in_memory().unwrap();
        cache
            .upsert_summaries("acc", "Sent", &[summary(1, "汇报", "me@example.com", 1)])
            .unwrap();
        assert!(cache.search("acc", "colleague", 5).unwrap().is_empty());

        let mut body = detail(1, "汇报", "正文");
        body.to = vec!["colleague@example.com".to_string()];
        cache.put_detail("acc", "Sent", &body).unwrap();

        assert_eq!(cache.search("acc", "colleague", 5).unwrap()[0].uid, 1);
    }

    #[test]
    fn cursor_round_trips_and_uidvalidity_change_wipe_the_folder() {
        let cache = Cache::open_in_memory().unwrap();
        let mailbox = MailboxInfo {
            name: "INBOX".to_string(),
            display_name: "收件箱".to_string(),
            delimiter: Some("/".to_string()),
            selectable: true,
        };
        cache.upsert_folder("acc", &mailbox, 111).unwrap();
        cache
            .upsert_summaries("acc", "INBOX", &[summary(1, "账单", "a@b.c", 1)])
            .unwrap();
        cache
            .put_detail("acc", "INBOX", &detail(1, "账单", "正文"))
            .unwrap();
        cache.set_cursor("acc", "INBOX", 111, 1).unwrap();

        assert_eq!(
            cache.folder_state("acc", "INBOX").unwrap(),
            Some(FolderState {
                uidvalidity: 111,
                last_uid: 1
            })
        );

        // The server changed UIDVALIDITY: every stored UID may now mean a different message.
        cache.invalidate_folder("acc", "INBOX").unwrap();
        cache.upsert_folder("acc", &mailbox, 222).unwrap();

        assert_eq!(cache.summary_count("acc", "INBOX").unwrap(), 0);
        assert!(cache.get_detail("acc", "INBOX", 1).unwrap().is_none());
        assert!(
            cache.search("acc", "账单", 5).unwrap().is_empty(),
            "the search index must be emptied too, or results point at rows that are gone"
        );
        assert_eq!(
            cache.folder_state("acc", "INBOX").unwrap(),
            Some(FolderState {
                uidvalidity: 222,
                last_uid: 0
            }),
            "the cursor must restart from zero"
        );
    }

    #[test]
    fn folders_are_remembered_for_offline_startup() {
        let cache = Cache::open_in_memory().unwrap();
        for (name, display) in [("INBOX", "收件箱"), ("Sent", "已发送")] {
            let mailbox = MailboxInfo {
                name: name.to_string(),
                display_name: display.to_string(),
                delimiter: Some("/".to_string()),
                selectable: true,
            };
            cache.upsert_folder("acc", &mailbox, 9).unwrap();
        }

        let folders = cache.list_folders("acc").unwrap();
        assert_eq!(folders.len(), 2);
        assert!(folders.iter().any(|f| f.display_name == "收件箱"));
    }

    #[test]
    fn deleting_a_message_clears_its_body_and_index_entry() {
        let cache = Cache::open_in_memory().unwrap();
        cache
            .upsert_summaries("acc", "INBOX", &[summary(1, "账单", "a@b.c", 1)])
            .unwrap();
        cache
            .put_detail("acc", "INBOX", &detail(1, "账单", "正文"))
            .unwrap();

        cache.delete_message("acc", "INBOX", 1).unwrap();

        assert_eq!(cache.summary_count("acc", "INBOX").unwrap(), 0);
        assert!(cache.get_detail("acc", "INBOX", 1).unwrap().is_none());
        assert!(cache.search("acc", "账单", 5).unwrap().is_empty());
    }

    #[test]
    fn forgetting_an_account_leaves_other_accounts_alone() {
        let cache = Cache::open_in_memory().unwrap();
        cache
            .upsert_summaries("a", "INBOX", &[summary(1, "账单", "x@y.z", 1)])
            .unwrap();
        cache
            .upsert_summaries("b", "INBOX", &[summary(1, "账单", "x@y.z", 1)])
            .unwrap();

        cache.forget_account("a").unwrap();

        assert_eq!(cache.summary_count("a", "INBOX").unwrap(), 0);
        assert_eq!(cache.summary_count("b", "INBOX").unwrap(), 1);
    }

    #[test]
    fn a_cache_from_a_newer_schema_is_refused_rather_than_misread() {
        let path = std::env::temp_dir().join(format!(
            "easy-email-future-{}-{}.sqlite",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        {
            let conn = Connection::open(&path).unwrap();
            conn.pragma_update(None, "user_version", SCHEMA_VERSION + 1)
                .unwrap();
        }
        let error = match Cache::open(&path) {
            Ok(_) => panic!("a newer schema must not be opened"),
            Err(error) => error,
        };
        assert!(matches!(error, MailError::Cache(_)), "got {error:?}");
        let _ = std::fs::remove_file(&path);
    }
}
