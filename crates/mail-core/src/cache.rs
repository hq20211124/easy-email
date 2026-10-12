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
//! The approach that works is to index and query **one character per token for the characters that
//! need it, and one token per word for the ones that do not** — see [`to_tokens`] and [`to_query`].
//! The two must stay in step; tests pin both the strings and the behaviour.

use rusqlite::{Connection, OptionalExtension, params};

use crate::db::{Db, DbKind, DbOptions};
use crate::error::{MailError, Result};
use crate::model::{MailboxInfo, MessageDetail, MessageSummary, ThreadMember};

/// Bumped whenever the schema below changes; [`Cache::migrate`] reacts to it.
pub const SCHEMA_VERSION: i64 = 7;

/// Which index shape a `bodies` row was last written by.
///
/// The FTS row is derived from the stored message, so a change to what goes into it (attachment
/// names arrived in round 71) has to be detectable per row. A row that predates the change reads
/// back as `0`, which puts it in [`Cache::bodies_pending`] again — and because `bodies.detail`
/// holds the whole message, the pass re-indexes those rows locally, with no connection. Without
/// this marker, the already-indexed mail of an existing user would never learn about the newer
/// index content at all.
pub const INDEX_VERSION: i64 = 1;

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

/// How much of an account has a body behind it — what search can actually see.
///
/// Serialisable for the same reason [`SearchHit`] is. The two numbers exist so the UI can say
/// "正文已索引 120/500" instead of implying that every cached message is searchable: before the
/// backfill pass (`crate::index`), a body arrives only when the user opens the message, and the
/// difference between those two facts is invisible on screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct BodyCoverage {
    /// Messages whose body is in `bodies` (and therefore in the index) and still have a row in
    /// `messages`.
    pub indexed: u32,
    /// Messages known to the cache at all.
    pub total: u32,
}

/// How far a rebuild of the search index has got.
///
/// The counters are **read back from the database** rather than accumulated. A rebuild is supposed
/// to survive being interrupted — that is what makes it resumable and cancellable — so its progress
/// cannot live in a variable somewhere; `done` is just the number of messages that have an index row
/// again, which is the same set the next step walks over.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RebuildProgress {
    /// Messages that have been rebuilt so far.
    pub done: u32,
    /// Messages known to the cache at all — the target, not an estimate.
    pub total: u32,
    /// `total - done`, reported as its own field because the UI shows it and arithmetic in two
    /// places is how "0 remaining" appears while rows are still unindexed.
    pub remaining: u32,
}

/// What the health check found (`design/09` goal 5 / `design/16` §4.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct IndexHealth {
    /// Messages this account has cached.
    pub messages: u32,
    /// Rows this account has in the search index — asked of the index itself, not computed.
    pub indexed: u32,
    /// Whether FTS5's own integrity check passes.
    pub fts_ok: bool,
    /// Bodies whose message row is gone. They inflate the coverage number and nothing else.
    pub orphan_bodies: u32,
}

/// What one heal step did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct HealReport {
    /// Messages re-indexed in this step.
    pub rebuilt: u32,
    /// Bodies rows deleted because their message no longer exists.
    pub orphans_deleted: u32,
    /// Still to rebuild, when a rebuild is in progress.
    pub remaining: u32,
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

/// Turn text into FTS5 tokens.
///
/// Two shapes, because two scripts need opposite things:
///
/// * a run of ASCII letters/digits becomes **one token** (`invoice`), so an English word is a word;
/// * every non-ASCII character becomes a token of its own (`账 单`), which is what makes a
///   two-character Chinese word findable — `unicode61` cannot segment Chinese, and the `trigram`
///   tokenizer every guide recommends needs three characters while most Chinese words are two.
///
/// Spaces and ASCII punctuation are separators.
///
/// **The ASCII half is not cosmetic** (changed 2026-10-06). Splitting `invoice` into `i n v o i c e`
/// made the query a seven-token phrase, and FTS5 has to intersect seven position lists to answer it:
/// the benchmark measured **P50 944 ms** on a 50 000-message fixture against a `design/15` budget of
/// 100 ms at P95. Whole-word ASCII tokens took it to single-digit milliseconds. The cost is that
/// *substring* search inside an English word no longer matches (`invoi` will not find `invoice`),
/// which is what every other mail client does anyway. Chinese is untouched: it still matches on any
/// adjacent two characters.
///
/// The transformation must be applied to **both** the indexed text and the user's query, or nothing
/// matches; [`to_query`] builds its clauses from the same run splitting for exactly that reason.
pub fn to_tokens(text: &str) -> String {
    let mut tokens: Vec<String> = Vec::new();
    let mut word = String::new();

    for c in text.chars() {
        if c.is_ascii_alphanumeric() {
            word.push(c.to_ascii_lowercase());
            continue;
        }
        if !word.is_empty() {
            tokens.push(std::mem::take(&mut word));
        }
        // Non-ASCII means a character-driven script (Chinese, Japanese, Korean) or an accented
        // letter — either way it is worth its own token. ASCII punctuation and whitespace separate.
        if !c.is_ascii() {
            tokens.push(c.to_string());
        }
    }
    if !word.is_empty() {
        tokens.push(word);
    }

    tokens.join(" ")
}

/// Cut to `limit` *characters*, never to bytes.
///
/// A byte cut would split a multi-byte character in half and panic on the slice boundary — and the
/// mail this limit exists for is Chinese, where that is the normal case rather than the exception.
fn truncate_chars(text: &str, limit: usize) -> &str {
    match text.char_indices().nth(limit) {
        Some((byte, _)) => &text[..byte],
        None => text,
    }
}

/// One clause of a `MATCH` expression: an ASCII word, or a run of characters that must be adjacent.
/// Crate-visible because `crate::query` builds the FTS expression from it: the index and the
/// query must tokenise with the *same* function, and that is only true while there is one of it.
pub(crate) enum Clause {
    Word(String),
    Phrase(Vec<char>),
}

/// Split a term the same way [`to_tokens`] splits indexed text.
///
/// Keeping this in one place is the whole point: the previous version of `to_query` called
/// `to_tokens` and wrapped the result in quotes, so the two could not disagree. They still cannot —
/// both walk the input and cut it at the same boundaries.
pub(crate) fn clauses_of(term: &str) -> Vec<Clause> {
    let mut clauses = Vec::new();
    let mut word = String::new();
    let mut run: Vec<char> = Vec::new();

    let flush = |clauses: &mut Vec<Clause>, word: &mut String, run: &mut Vec<char>| {
        if !word.is_empty() {
            clauses.push(Clause::Word(std::mem::take(word)));
        }
        if !run.is_empty() {
            clauses.push(Clause::Phrase(std::mem::take(run)));
        }
    };

    for c in term.chars() {
        if c.is_ascii_alphanumeric() {
            if !run.is_empty() {
                clauses.push(Clause::Phrase(std::mem::take(&mut run)));
            }
            word.push(c.to_ascii_lowercase());
        } else if c.is_ascii() {
            // ASCII punctuation separates, exactly as it does in the index.
            flush(&mut clauses, &mut word, &mut run);
        } else {
            if !word.is_empty() {
                clauses.push(Clause::Word(std::mem::take(&mut word)));
            }
            run.push(c);
        }
    }
    flush(&mut clauses, &mut word, &mut run);
    clauses
}

/// Turn user input into an FTS5 `MATCH` expression.
///
/// Each whitespace-separated term becomes one or more clauses, and everything is ANDed — so
/// `账单 发票` means "the adjacent characters 账单, and also 发票", and `invoice 账单` means "the word
/// invoice, and also 账单". Quotes are dropped: they are FTS5 syntax, and letting them through turns
/// a pasted string into a `MATCH` syntax error.
use crate::query::{QueryTerm, match_expression, parse_query};

/// The FTS5 MATCH expression for a whole query string — the historical entry point, kept because
/// the tokenisation symmetry test pins it. Parsing lives in [`crate::query`]; this is the bridge the
/// existing callers and that test still use.
pub fn to_query(input: &str) -> String {
    match_expression(&parse_query(input))
}

/// Build the WHERE conditions for one search, in a fixed parameter order.
///
/// Each condition is a fragment with a bare `?` plus the value to bind. The numbering advances only
/// for clauses that bind something: the `has:attachment` clause is an EXISTS with no `?`, and
/// counting it would shift every predicate after it by one — a query that returns the wrong rows
/// instead of an error. That is the whole reason predicates travel as a structure rather than as
/// `format!` sprinkled through `search`: string-built SQL with foreign text in it is both an
/// injection surface and a quoting bug waiting for a name with an apostrophe.
struct Predicates {
    clauses: Vec<String>,
    values: Vec<rusqlite::types::Value>,
    next: usize,
}

impl Predicates {
    /// `first` is the number the first bound value takes: 2 when the query carries an FTS MATCH (that
    /// expression holds ?1) and 1 otherwise. Getting it wrong binds the account id where the search
    /// expression was meant to go, which SQLite answers with silence rather than an error.
    fn new(first: usize) -> Self {
        Self {
            clauses: Vec::new(),
            values: Vec::new(),
            next: first,
        }
    }

    fn add<T: Into<rusqlite::types::Value>>(&mut self, clause: &str, value: T) {
        self.clauses
            .push(clause.replacen('?', &format!("?{}", self.next), 1));
        self.next += 1;
        self.values.push(value.into());
    }

    /// A clause that binds nothing (`exists (...)`), so it must not consume a placeholder.
    fn add_raw(&mut self, clause: &str) {
        self.clauses.push(clause.to_string());
    }

    fn sql(&self) -> String {
        self.clauses.join(" and ")
    }
}

#[cfg(test)]
mod predicate_tests {
    use super::*;

    #[test]
    fn a_new_predicate_cannot_shift_anothers_placeholder() {
        let mut p = Predicates::new(1);
        p.add("m.account_id = ?", "acc".to_string());
        p.add("m.date >= ?", 1_767_225_600_i64);
        p.add("m.seen = ?", 0_i64);
        assert_eq!(
            p.sql(),
            "m.account_id = ?1 and m.date >= ?2 and m.seen = ?3"
        );
        assert_eq!(p.values.len(), 3);
    }
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
                     index_version integer not null default 0,
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

            // v7: which index shape each stored body was last written by (`INDEX_VERSION`). Old rows
            // default to 0, which is what puts them back into `bodies_pending` — the pass re-indexes
            // them from `bodies.detail` itself, so no network and no rebuild at open time
            // (`design/09` §5.4: rebuilding belongs in bounded background batches, never in the
            // startup transaction).
            add_column_if_missing(conn, "bodies", "index_version", "integer not null default 0")?;
            if previous_version < 7 {
                // v7 changed the tokenisation (ASCII words are whole tokens now), so an index built
                // by an older version is not just stale, it is built on different rules and can never
                // match a query from this version. Rebuilding is the only repair, and it is cheap:
                // the index is derived data, rebuilt from `messages` + `bodies` in one pass.
                //
                // The same rebuild covers v6's rowid change, so one condition serves both.
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
                Self::refresh_search_row(
                    conn,
                    &account_id,
                    &folder,
                    summary.uid,
                    crate::index::DEFAULT_BODY_INDEX_CHARS,
                )?;
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

    /// Unread messages **per folder**, for the sidebar badges.
    ///
    /// A folder with no unread mail is absent from the map rather than present with `0`: the renderer
    /// shows nothing for it, which is the difference between a badge that carries information and one
    /// that is decoration. Read from the cache for the same reason [`Cache::unread_count`] is — the
    /// badges must appear on an offline start, where there is no server to ask.
    ///
    /// Folders the cache has never synced are absent too, so a freshly added account shows no badges
    /// instead of a row of zeroes.
    pub fn unread_counts(
        &self,
        account_id: &str,
    ) -> Result<std::collections::HashMap<String, u32>> {
        let guard = self.db.read_guard();
        let conn: &Connection = &guard;

        let mut statement = conn.prepare(
            "select folder, count(*) from messages
              where account_id = ?1 and seen = 0
              group by folder",
        )?;
        let rows = statement.query_map(params![account_id], |row| {
            let folder: String = row.get(0)?;
            let count: i64 = row.get(1)?;
            Ok((folder, count as u32))
        })?;

        let mut counts = std::collections::HashMap::new();
        for row in rows {
            let (folder, count) = row?;
            if count > 0 {
                counts.insert(folder, count);
            }
        }
        Ok(counts)
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

    /// Messages in this account that have no body in the cache yet, newest first.
    ///
    /// Driven from `messages` with a `left join`, not by pulling every uid into Rust and filtering:
    /// at 100k rows the join is one query and the alternative is one query plus a filter over 100k
    /// values. The `b.uid is null` side is what makes a message "not read yet" — `put_detail` is the
    /// only writer of that table, so this is exactly "search cannot see this one yet".
    pub fn bodies_pending(
        &self,
        account_id: &str,
        limit: usize,
    ) -> Result<Vec<crate::index::PendingBody>> {
        let guard = self.db.read_guard();
        let conn: &Connection = &guard;
        let mut stmt = conn.prepare(
            "select m.folder, m.uid, (b.uid is not null) as has_body
             from messages m
             left join bodies b
               on b.account_id = m.account_id and b.folder = m.folder and b.uid = m.uid
             where m.account_id = ?1
               and (b.uid is null or b.index_version < ?3)
             order by m.date desc, m.uid desc
             limit ?2",
        )?;
        let cap = i64::try_from(limit).unwrap_or(i64::MAX);
        let rows = stmt.query_map(params![account_id, cap, INDEX_VERSION], |row| {
            Ok(crate::index::PendingBody {
                folder: row.get(0)?,
                uid: row.get::<_, i64>(1)? as u32,
                has_body: row.get::<_, i64>(2)? != 0,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// How many messages in this account have no body in the cache yet.
    ///
    /// A count rather than a `bodies_pending(…, 1).len()` probe: the probe can only answer "none" or
    /// "one", so it would report the smaller number — and the UI repeats whatever it is told.
    pub fn bodies_pending_count(&self, account_id: &str) -> Result<u32> {
        let guard = self.db.read_guard();
        let conn: &Connection = &guard;
        Ok(conn.query_row(
            "select count(*) from messages m
             left join bodies b
               on b.account_id = m.account_id and b.folder = m.folder and b.uid = m.uid
             where m.account_id = ?1
               and (b.uid is null or b.index_version < ?2)",
            params![account_id, INDEX_VERSION],
            |row| row.get::<_, i64>(0).map(|n| n as u32),
        )?)
    }

    /// How much of this account has a body behind it — what search can actually see today.
    ///
    /// `indexed` counts bodies that still have a message row (`exists`, not a bare count): a body
    /// whose message was deleted is not searchable, and counting it would let the UI claim 105/100.
    ///
    /// **A row behind `INDEX_VERSION` still counts as indexed** — its body is searchable, it just
    /// does not carry attachment names yet. It *is* pending work (`bodies_pending_count`), because
    /// the pass still has to rebuild it, but subtracting it from this number would understate what
    /// search can do: an upgrading user would see "已索引 0/300" while their 300 bodies answer
    /// queries perfectly well. The honest place for that shortfall is the pending count, which is
    /// what the index button works through.
    pub fn body_coverage(&self, account_id: &str) -> Result<BodyCoverage> {
        let guard = self.db.read_guard();
        let conn: &Connection = &guard;
        let total: u32 = conn.query_row(
            "select count(*) from messages where account_id = ?1",
            params![account_id],
            |row| row.get::<_, i64>(0).map(|n| n as u32),
        )?;
        let indexed: u32 = conn.query_row(
            "select count(*) from bodies b
             where b.account_id = ?1
               and exists (select 1 from messages m
                           where m.account_id = b.account_id
                             and m.folder = b.folder and m.uid = b.uid)",
            params![account_id],
            |row| row.get::<_, i64>(0).map(|n| n as u32),
        )?;
        Ok(BodyCoverage { indexed, total })
    }

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

            Self::refresh_search_row(
                conn,
                &account_id,
                &folder,
                detail.uid,
                crate::index::DEFAULT_BODY_INDEX_CHARS,
            )?;
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

    /// Rebuild this message's index row from what is already stored — no network.
    ///
    /// What a row whose `index_version` is behind needs: `bodies.detail` holds the whole message,
    /// so the only thing missing is the newer index shape (attachment names, in round 71). The
    /// bounded pass calls this for such rows instead of fetching them again, which is what keeps
    /// the upgrade free for the user. Goes through the write thread because
    /// [`Self::refresh_search_row`] writes both FTS tables.
    pub fn reindex_row(&self, account_id: &str, folder: &str, uid: u32) -> Result<()> {
        let account_id = account_id.to_string();
        let folder = folder.to_string();
        self.db.write_blocking(move |conn: &Connection| {
            Self::refresh_search_row(
                conn,
                &account_id,
                &folder,
                uid,
                crate::index::DEFAULT_BODY_INDEX_CHARS,
            )
        })
    }

    /// Test seam: put a row back to an older index shape, which is exactly what an upgrade looks
    /// like from the database's side. `#[cfg(test)]` because no production path needs it — the
    /// whole point of the tests that use it is that the normal code cannot produce a stale row by
    /// accident, only the migration of an older file can.
    #[cfg(test)]
    pub fn force_index_version(
        &self,
        account_id: &str,
        folder: &str,
        uid: u32,
        version: i64,
    ) -> Result<()> {
        let account_id = account_id.to_string();
        let folder = folder.to_string();
        self.db.write_blocking(move |conn: &Connection| {
            conn.execute(
                "update bodies set index_version = ?4
                 where account_id = ?1 and folder = ?2 and uid = ?3",
                params![account_id, folder, i64::from(uid), version],
            )?;
            Ok(())
        })
    }

    /// Throw the index away so it can be built again from what this machine already has.
    ///
    /// The FTS table holds tokenised text derived from `messages` and `bodies`, and both survive
    /// untouched — this deletes **nothing the user owns**, which is what makes it safe to expose as
    /// a button (`design/09` §5.4: the index is derived data, droppable and rebuildable).
    ///
    /// One transaction, and the rebuild itself then proceeds in bounded steps
    /// ([`Self::rebuild_search_step`]). §5.4 forbids rebuilding inside a startup transaction, and for
    /// the same reason it must not happen inside one long transaction here: every other command
    /// would queue behind it on the single write thread.
    ///
    /// Not to be confused with the migration's own one-shot `rebuild_search_index`, which runs once
    /// inside `migrate` for a v5 file that had no `search_rowid` to delete by. The two look alike and
    /// are deliberately separate — do not "merge" them.
    pub fn rebuild_search_begin(&self) -> Result<()> {
        self.db.write_blocking(|conn: &Connection| {
            conn.execute("delete from search", [])?;
            conn.execute("update messages set search_rowid = null", [])?;
            Ok(())
        })
    }

    /// Rebuild up to `limit` messages' index rows. The next call continues where this one stopped.
    ///
    /// Ordered by `messages.rowid`: stable, so a resumed rebuild walks the same rows in the same
    /// order. A message with no stored body is rebuilt as a **header-only** row — search has always
    /// covered headers, and going to the network for bodies is the pending pass's job
    /// ([`crate::index`]), not the rebuild's. That is also why the UI has to say so: a rebuilt
    /// message the user never opened is searchable by subject but not by body, which is the truth
    /// and not a fault.
    pub fn rebuild_search_step(&self, limit: usize) -> Result<RebuildProgress> {
        let cap = i64::try_from(limit).unwrap_or(i64::MAX);
        self.db.write_blocking(move |conn: &Connection| {
            let rows: Vec<(String, String, u32)> = {
                let mut stmt = conn.prepare(
                    "select account_id, folder, uid from messages
                     where search_rowid is null
                     order by rowid
                     limit ?1",
                )?;
                let mapped = stmt.query_map(params![cap], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, i64>(2)? as u32,
                    ))
                })?;
                mapped.collect::<rusqlite::Result<Vec<_>>>()?
            };

            for (account_id, folder, uid) in rows {
                Self::refresh_search_row(
                    conn,
                    &account_id,
                    &folder,
                    uid,
                    crate::index::DEFAULT_BODY_INDEX_CHARS,
                )?;
            }
            // Counted, never accumulated: a step cut short by a crash or a closed window still has
            // to report the truth, and the caller drives its loop off these numbers. Accumulating
            // would make the progress line the one thing a restart silently rewrites.
            let total: i64 =
                conn.query_row("select count(*) from messages", [], |row| row.get(0))?;
            let done: i64 = conn.query_row(
                "select count(*) from messages where search_rowid is not null",
                [],
                |row| row.get(0),
            )?;
            let done = done.clamp(0, total);
            Ok(RebuildProgress {
                done: done as u32,
                total: total as u32,
                remaining: (total - done) as u32,
            })
        })
    }

    // ------------------------------------------------------------------ search

    /// Run one closure on the cache's writer connection.
    ///
    /// For the modules whose work is "a few SQL statements over this database" — quota, the
    /// diagnostics bundle — so they can be written against a `&Connection` and tested against an
    /// in-memory one, instead of each taking a `Cache` and reaching into it. Routed through
    /// `write_blocking` like every other write here, so it holds the writer for exactly as long as
    /// the closure and no longer.
    ///
    /// Read-only closures belong on [`Self::db`]'s readers; this is the write door.
    pub fn with_connection<T>(
        &self,
        f: impl FnOnce(&Connection) -> rusqlite::Result<T> + Send + 'static,
    ) -> Result<T>
    where
        T: Send + 'static,
    {
        self.db.write_blocking(move |conn| {
            f(conn).map_err(|e| MailError::Cache(format!("数据库读写失败：{e}")))
        })
    }

    /// Clean the cache: what it would do, what it did, and the index told about both.
    ///
    /// The two-step shape (`plan` then `apply`) is the design's, and the whole entry point exists so
    /// the caller can show the plan before it is run — a cleanup whose numbers only appear afterwards
    /// is a cleanup the user cannot consent to.
    ///
    /// The index refresh after the delete is why this is a `Cache` method and not a call straight
    /// into `quota`: the bodies that went took their words with them, and a search row that still
    /// carries them would keep answering for mail the cache no longer has.
    pub fn cleanup(
        &self,
        policy: crate::quota::CleanupPolicy,
        now: i64,
    ) -> Result<(crate::quota::CleanupPlan, crate::quota::CleanupReport)> {
        self.with_connection(move |conn| {
            let plan = crate::quota::plan(conn, &policy, now)?;
            let report = crate::quota::apply(conn, &policy, &plan, now)?;
            // Header-only rows for the messages whose body is gone, in the same write as the delete:
            // the whole cleanup is one transaction, so a crash mid-way cannot leave an index that
            // answers for a body that is still there.
            let stale: Vec<(String, String, u32)> = {
                let mut stmt = conn.prepare(
                    "select m.account_id, m.folder, m.uid from messages m
                     where m.search_rowid is not null
                       and not exists (select 1 from bodies b
                                        where b.account_id = m.account_id
                                          and b.folder = m.folder
                                          and b.uid = m.uid)",
                )?;
                let rows = stmt.query_map([], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, i64>(2)? as u32,
                    ))
                })?;
                rows.collect::<rusqlite::Result<Vec<_>>>()?
            };
            for (account_id, folder, uid) in stale {
                Self::refresh_search_row(
                    conn,
                    &account_id,
                    &folder,
                    uid,
                    crate::index::DEFAULT_BODY_INDEX_CHARS,
                )
                .map_err(|e| {
                    rusqlite::Error::ToSqlConversionFailure(Box::new(std::io::Error::other(e)))
                })?;
            }
            Ok((plan, report))
        })
    }

    /// Whether FTS5 considers itself consistent.
    ///
    /// FTS5's own `integrity-check` is the authority here, and that is the point: a count comparison
    /// between the FTS table and `messages` would catch "a row went missing" and nothing else, while
    /// reporting a perfectly normal mid-write state as broken. The one thing a heuristic like that
    /// achieves is a heal the user did not need.
    ///
    /// Returns `false` rather than an error when the check fails — a caller asking "is the index
    /// healthy" has an answer either way, and a corrupt index is a finding, not an exception.
    pub fn fts_integrity_ok(&self) -> bool {
        self.db
            .write_blocking(|conn: &Connection| {
                // The command is a special INSERT into the table's own shadow table; a corrupt index
                // raises SQLITE_CORRUPT_VTAB from here rather than at query time.
                conn.execute(
                    "insert into search(search, rank) values('integrity-check', 1)",
                    [],
                )
                .map(|_| ())
                .map_err(Into::into)
            })
            .is_ok()
    }

    /// What the search index looks like right now, for one account.
    ///
    /// `indexed` counts this account's rows in the FTS table, and is asked of the FTS table rather
    /// than computed from `messages` — a detector that cannot see the thing it is checking for is
    /// worse than none, because it reports healthy while rows are gone.
    pub fn index_health(&self, account_id: &str) -> Result<IndexHealth> {
        let account_id = account_id.to_string();
        let guard = self.db.read_guard();
        let conn: &Connection = &guard;
        let messages: u32 = conn.query_row(
            "select count(*) from messages where account_id = ?1",
            params![account_id],
            |row| row.get::<_, i64>(0).map(|n| n as u32),
        )?;
        let indexed: u32 = conn.query_row(
            "select count(*) from search where account_id = ?1",
            params![account_id],
            |row| row.get::<_, i64>(0).map(|n| n as u32),
        )?;
        let orphan_bodies: u32 = conn.query_row(
            "select count(*) from bodies b
             where b.account_id = ?1
               and not exists (select 1 from messages m
                               where m.account_id = b.account_id
                                 and m.folder = b.folder
                                 and m.uid = b.uid)",
            params![account_id],
            |row| row.get::<_, i64>(0).map(|n| n as u32),
        )?;
        drop(guard);
        Ok(IndexHealth {
            messages,
            indexed,
            fts_ok: self.fts_integrity_ok(),
            orphan_bodies,
        })
    }

    /// Repair what the health check can name.
    ///
    /// One bounded step, so it can be called from a tick: an FTS that failed its integrity check is
    /// rebuilt through the same begin/step pair the round-72 UI drives — a second rebuild path is a
    /// second thing that can disagree with the first. Orphan bodies are deleted in the same call,
    /// because a count that includes rows for messages that no longer exist is a count that lies
    /// about coverage.
    ///
    /// A healthy index is left alone and reports zeros. A healer that "fixes" a healthy index is a
    /// healer that cannot tell, and the user pays for the rebuild either way.
    pub fn heal_index(&self, account_id: &str, step: usize) -> Result<HealReport> {
        let mut report = HealReport::default();
        let health = self.index_health(account_id)?;

        // Orphans first, and scoped by account: the FTS rebuild walks every message in the cache,
        // but the bodies table only needs its own rows cleaned.
        if health.orphan_bodies > 0 {
            let removed = self.db.write_blocking({
                let account_id = account_id.to_string();
                move |conn: &Connection| {
                    conn.execute(
                        "delete from bodies
                         where account_id = ?1
                           and not exists (select 1 from messages m
                                           where m.account_id = bodies.account_id
                                             and m.folder = bodies.folder
                                             and m.uid = bodies.uid)",
                        params![account_id],
                    )
                    .map_err(Into::into)
                }
            })?;
            report.orphans_deleted = removed as u32;
        }

        // Either signal is enough, and the count one matters more: a row that went missing is the
        // damage a user actually meets (the mail stops being findable) while a structurally corrupt
        // index usually announces itself. The count is a trustworthy signal **because** every write
        // path puts the message row and its index row in one transaction
        // (`upsert_summaries`/`put_detail`), so a reader can never see the half that is missing.
        // That is the fact this relies on; if a write path ever splits in two, this becomes a
        // detector that repairs a race, which is why the comment is here.
        if !health.fts_ok || health.indexed < health.messages {
            self.rebuild_search_begin()?;
            let progress = self.rebuild_search_step(step)?;
            report.rebuilt = progress.done;
            report.remaining = progress.remaining;
        }
        Ok(report)
    }

    /// Local full-text search over headers (always) and bodies (for messages already read).
    ///
    /// **Driven from the index, joined by rowid.** The obvious shape — `from search join messages m
    /// using (account_id, folder, uid) where search match ?` — is the one that was here, and it is
    /// quadratic: the three join columns are `unindexed` in the FTS5 table, so the planner cannot use
    /// them for a join and the whole thing degrades into a scan. Measured on the 20 000-message
    /// fixture: P50 769 ms, P95 2 380 ms, against a `design/15` budget of 100 ms at P95 on ten times
    /// that data. The subquery takes the matching FTS rowids first (an index lookup) and
    /// `messages_by_search_rowid` turns each into the message row.
    ///
    /// **Three alternatives were measured on 2026-10-06 and rejected**, each in the same run as the
    /// query above (the fixture harness prints the comparison):
    ///
    /// * driving from `messages` and testing membership (`search_rowid in (…)`) — P50 279 ms against
    ///   43 ms, because SQLite picks `messages_by_subject_key` plus a bloom filter and never gets the
    ///   early stop the shape was supposed to buy;
    /// * taking the best 500 by `rank` before joining — P50 317 ms, *and* it answers a different
    ///   question ("the best matches" rather than "the newest"), so it also returns different rows;
    /// * splitting into "sort narrow, then fetch the page wide" — P50 40 ms against 44 ms for the
    ///   single query, i.e. inside the noise. The shape is tempting (SQLite's sorter stores the whole
    ///   result row, so it *looks* like the seven columns cost something) but the measurement says
    ///   otherwise, and the extra statement is not worth an unproven claim.
    ///
    /// **One thing this file cannot explain, recorded so it is not lost:** in the benchmark the query
    /// above measures ~175 ms while the *identical* SQL with the same plan, same fixture and same run
    /// measures ~44 ms (`bench_fixture`'s shapes A/F). A 4× gap between the statement and the
    /// application's own measurement is a property of the measurement or of the connection the
    /// application uses — not of the SQL — and until it is explained, the P4 numbers should be read as
    /// an upper bound rather than as the cost of the query.
    pub fn search(&self, account_id: &str, query: &str, limit: u32) -> Result<Vec<SearchHit>> {
        let guard = self.db.read_guard();
        let conn: &Connection = &guard;

        let terms = parse_query(query);
        let expression = match_expression(&terms);

        // Predicates are collected first, in a fixed order, because each one adds a bound value and a
        // numbered placeholder — and a predicate that silently shifts another's index is a query that
        // returns the wrong rows rather than an error. `first` leaves ?1 free for the MATCH
        // expression when there is one.
        // An explicit `account:` **replaces** the caller's scope rather than adding to it: two
        // conditions on the same column can only contradict each other, and "0 results" is the worst
        // way to say so. The caller's account is the default; `account:` is the override, and it is
        // what a unified inbox would use.
        let explicit_account = terms.iter().find_map(|term| match term {
            QueryTerm::Account { id } => Some(id.clone()),
            _ => None,
        });
        let mut pred = Predicates::new(if expression.is_empty() { 1 } else { 2 });
        pred.add(
            "m.account_id = ?",
            explicit_account.unwrap_or_else(|| account_id.to_string()),
        );
        for term in &terms {
            match term {
                QueryTerm::HasAttachment => pred.add_raw(
                    // A fact about the stored row, not about indexed text. EXISTS over `bodies` reads
                    // the saved JSON and counts the array: an empty array is 0, and a row whose body
                    // was never fetched has no row at all — which is the same answer.
                    "exists (select 1 from bodies b
                        where b.account_id = m.account_id
                          and b.folder = m.folder
                          and b.uid = m.uid
                          and json_array_length(json_extract(b.detail, '$.attachments')) > 0)",
                ),
                QueryTerm::Date { after, ts } => {
                    let clause = if *after { "m.date >= ?" } else { "m.date <= ?" };
                    pred.add(clause, *ts);
                }
                QueryTerm::Unread => pred.add("m.seen = ?", 0_i64),
                QueryTerm::Flagged => pred.add("m.flagged = ?", 1_i64),
                // A message whose size the server never said is NULL, and a comparison with NULL is
                // not true in SQL — an unknown size is neither a small message nor a large one, and
                // treating it as 0 would quietly claim "smaller than 1KB".
                QueryTerm::Size { larger, bytes } => {
                    let clause = if *larger {
                        "m.size >= ?"
                    } else {
                        "m.size <= ?"
                    };
                    pred.add(clause, i64::try_from(*bytes).unwrap_or(i64::MAX));
                }
                // Case-insensitive: the wire spells `INBOX` and a user types `inbox`.
                QueryTerm::InFolder { folder } => {
                    pred.add("lower(m.folder) = lower(?)", folder.clone())
                }
                // Handled above, as an override of the caller's scope.
                QueryTerm::Account { .. } => {}
                QueryTerm::Text { .. } => {}
            }
        }
        let has_text = !expression.is_empty();
        // Asked of the **terms**, never of the predicate list: `pred` always carries the account
        // condition, so "is anything left to narrow by" is a question about the query, not about the
        // clause count. Getting that wrong is exactly how a bare `has:attachment` came back empty in
        // round 77, and again in the first version of this function.
        let has_predicate = terms
            .iter()
            .any(|term| !matches!(term, QueryTerm::Text { .. }));
        if !has_text && !has_predicate {
            // Nothing to answer: no words to match and no conditions to narrow by.
            return Ok(Vec::new());
        }

        let where_clause = pred.sql();
        let sql = if has_text {
            format!(
                "select m.folder, m.uid, m.subject, m.from_name, m.from_addr, m.date, m.seen
                  from (select rowid from search where search match ?1) f
                  join messages m on m.search_rowid = f.rowid
                  where {where_clause}
                  order by m.date desc, m.uid desc
                  limit ?{}",
                pred.next
            )
        } else {
            format!(
                "select m.folder, m.uid, m.subject, m.from_name, m.from_addr, m.date, m.seen
                  from messages m
                  where {where_clause}
                  order by m.date desc, m.uid desc
                  limit ?{}",
                pred.next
            )
        };

        // One row-mapping for both shapes: they differ only in which query drove them.
        fn hit_from(row: &rusqlite::Row<'_>) -> rusqlite::Result<SearchHit> {
            Ok(SearchHit {
                folder: row.get(0)?,
                uid: row.get::<_, i64>(1)? as u32,
                subject: row.get(2)?,
                from_name: row.get(3)?,
                from_addr: row.get(4)?,
                date: row.get(5)?,
                seen: row.get::<_, i64>(6)? != 0,
            })
        }

        // The MATCH expression first (when there is one), then the predicates in the same order they
        // were numbered, then the limit — the order is the contract, which is why the values travel
        // together with the clauses that produced them.
        let mut params: Vec<rusqlite::types::Value> = Vec::new();
        if has_text {
            params.push(expression.into());
        }
        params.extend(pred.values);
        params.push(i64::from(limit).into());

        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt
            .query_map(rusqlite::params_from_iter(params), hit_from)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
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
    pub(crate) fn refresh_search_row(
        conn: &Connection,
        account_id: &str,
        folder: &str,
        uid: u32,
        body_limit: usize,
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

        // The body and the attachment names both live inside the serialised detail; one parse gives
        // both. Names only, not the attachment contents: `design/09` §11/Q3 keeps text extraction
        // off by default (M2), and the M1b slice is "search for 报告.pdf", not "search inside it".
        let detail = source
            .detail_json
            .as_deref()
            .and_then(|json| serde_json::from_str::<MessageDetail>(json).ok());
        let body = detail
            .as_ref()
            .and_then(|d| d.body_text.clone())
            .unwrap_or_default();

        // Only the *index copy* is truncated. `bodies.detail` keeps the whole message: that is what
        // the user reads, and cutting it here would lose mail (`design/20-v0.2-plan.md` C9). The
        // default is `design/09`'s 64 KB, and it is a setting on the roadmap so "I want everything
        // searchable" has somewhere to live.
        let body = truncate_chars(&body, body_limit);

        // Attachment names are appended AFTER the truncation, never before it: the cap belongs to
        // the body alone, and a 64 KB message would otherwise squeeze the filename out of the index
        // — the name is short and is exactly what the user types on a long thread. `to_tokens` does
        // the per-character spacing, so a Chinese filename matches the same way a Chinese body
        // does; an ASCII extension like `pdf` is already a single token.
        let names = detail
            .as_ref()
            .map(|d| {
                d.attachments
                    .iter()
                    .map(|a| a.name.as_str())
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .unwrap_or_default();
        let indexed_body = if names.is_empty() {
            to_tokens(body)
        } else {
            format!("{} {}", to_tokens(body), to_tokens(&names))
        };

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
                indexed_body,
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

        // Mark the body as carrying the current index shape. A row written before attachment names
        // existed reads back as 0, which is what sends it through `bodies_pending` again — the pass
        // re-indexes it locally (see `Cache::reindex_row`), so an existing user's already-indexed
        // mail learns about attachment names without a single fetch.
        conn.execute(
            "update bodies set index_version = ?4
             where account_id = ?1 and folder = ?2 and uid = ?3",
            params![account_id, folder, i64::from(uid), INDEX_VERSION],
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
        Cache::refresh_search_row(
            conn,
            &account_id,
            &folder,
            uid,
            crate::index::DEFAULT_BODY_INDEX_CHARS,
        )?;
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

    /// Same message, with attachment names — the round-71 slice. Names only, no contents: the
    /// point is that "报告.pdf" is searchable, not that the PDF is.
    fn detail_with_attachments(
        uid: u32,
        subject: &str,
        body_text: &str,
        names: &[&str],
    ) -> MessageDetail {
        let mut d = detail(uid, subject, body_text);
        d.attachments = names
            .iter()
            .map(|name| crate::model::AttachmentMeta {
                name: (*name).to_string(),
                mime: "application/octet-stream".to_string(),
                size: 1024,
                part: 1,
                inline: false,
                content_id: None,
            })
            .collect();
        d
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
    fn attachment_names_are_searchable_after_put_detail() {
        // `design/09` §2: the indexed row covers subject/sender/recipients + body + attachment
        // *names* — names only, because text extraction is the M2 switch (§11/Q3). The user's
        // query is "报告.pdf", not the body the file rode in; both the Chinese name and the ASCII
        // extension have to find it.
        let cache = Cache::open_in_memory().unwrap();
        cache
            .upsert_summaries(
                "acc",
                "INBOX",
                &[summary(1, "季度报表", "boss@example.com", 100)],
            )
            .unwrap();
        cache
            .put_detail(
                "acc",
                "INBOX",
                &detail_with_attachments(
                    1,
                    "季度报表",
                    "正文里没有你要找的词",
                    &["报告.pdf", "截图.png"],
                ),
            )
            .unwrap();

        assert_eq!(
            cache.search("acc", "报告", 10).unwrap().len(),
            1,
            "Chinese name"
        );
        assert_eq!(
            cache.search("acc", "pdf", 10).unwrap().len(),
            1,
            "extension"
        );
    }

    #[test]
    fn a_row_indexed_before_attachment_names_existed_is_reindexed_without_the_network() {
        // What an upgrade looks like from the outside: the user read this mail, so the body is in
        // the cache, but its index row predates attachment names. It must come back as pending
        // *with the body in hand* (`has_body`) and be re-indexed by the local path. The coverage
        // number deliberately does **not** move: see the assertion below.
        let cache = Cache::open_in_memory().unwrap();
        cache
            .upsert_summaries(
                "acc",
                "INBOX",
                &[summary(1, "发票", "shop@example.com", 100)],
            )
            .unwrap();
        cache
            .put_detail(
                "acc",
                "INBOX",
                &detail_with_attachments(1, "发票", "正文", &["报告.pdf"]),
            )
            .unwrap();
        cache
            .force_index_version("acc", "INBOX", 1, 0)
            .expect("rewind the row to the pre-attachment shape");

        let pending = cache.bodies_pending("acc", 20).unwrap();
        assert_eq!(pending.len(), 1, "a stale row is pending again");
        assert!(
            pending[0].has_body,
            "…and the pass must know the body is already stored"
        );
        let coverage = cache.body_coverage("acc").unwrap();
        assert_eq!(
            (coverage.indexed, coverage.total),
            (1, 1),
            "pending work is not lost search: the body is indexed and answers queries today"
        );
        assert_eq!(
            cache.search("acc", "正文", 10).unwrap().len(),
            1,
            "…and the rewind did not take the body with it"
        );

        cache
            .reindex_row("acc", "INBOX", 1)
            .expect("local re-index");
        assert_eq!(cache.search("acc", "报告", 10).unwrap().len(), 1);
        assert_eq!(cache.bodies_pending_count("acc").unwrap(), 0);
        let coverage = cache.body_coverage("acc").unwrap();
        assert_eq!((coverage.indexed, coverage.total), (1, 1));
    }

    // ---------------------------------------------------------------- rebuild

    /// A temp file for the tests that need a second process-worth of state: "resume after a
    /// restart" cannot be shown on an in-memory cache, because reopening one gives an empty database
    /// rather than the file the first handle left behind.
    fn temp_cache_path(tag: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "easy-email-{tag}-{}-{}.sqlite",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ))
    }

    /// Three messages covering the three shapes search has to serve: headers only, a body, and a
    /// body whose mail is found by its attachment name.
    fn seed_rebuild_fixture(cache: &Cache) {
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
            .expect("summaries");
        // 1 never gets a body: the rebuild must still make it searchable by subject, and must not
        // invent a body for it.
        cache
            .put_detail(
                "acc",
                "INBOX",
                &detail_with_attachments(2, "开会通知", "记得带季度报表", &["议程.pdf"]),
            )
            .expect("body 2");
        cache
            .put_detail("acc", "INBOX", &detail(3, "发票已开具", "发票号 INV-8848"))
            .expect("body 3");
    }

    fn uids(cache: &Cache, query: &str) -> Vec<u32> {
        cache
            .search("acc", query, 20)
            .expect("search")
            .iter()
            .map(|hit| hit.uid)
            .collect()
    }

    #[test]
    fn a_rebuild_reproduces_the_incremental_index() {
        // The load-bearing invariant of the whole block: a rebuilt index must answer **the same
        // questions with the same answers** as one that was never deleted. If this fails, the button
        // is not a repair — it is a second, subtly different search engine.
        let cache = Cache::open_in_memory().unwrap();
        seed_rebuild_fixture(&cache);

        let queries = ["对账单", "季度报表", "议程", "INV-8848", "example.com"];
        let before: Vec<Vec<u32>> = queries.iter().map(|q| uids(&cache, q)).collect::<Vec<_>>();

        cache.rebuild_search_begin().expect("begin");
        let mut progress = cache.rebuild_search_step(2).expect("step 1");
        while progress.remaining > 0 {
            progress = cache.rebuild_search_step(2).expect("step");
        }

        let after: Vec<Vec<u32>> = queries.iter().map(|q| uids(&cache, q)).collect();
        for (i, query) in queries.iter().enumerate() {
            assert_eq!(
                before[i], after[i],
                "rebuild changed the answer to {query:?}"
            );
        }
        // Sanity: the fixture actually distinguishes the shapes, or the comparison above proves
        // nothing. A term from nowhere must find nothing, or "the answers match" would be vacuous.
        assert_eq!(uids(&cache, "对账单"), vec![1], "headers still searchable");
        assert_eq!(
            uids(&cache, "议程"),
            vec![2],
            "attachment name still searchable"
        );
        assert_eq!(uids(&cache, "INV-8848"), vec![3], "body still searchable");
        assert!(
            uids(&cache, "查无此词").is_empty(),
            "control: nothing matches nothing"
        );
    }

    #[test]
    fn a_rebuild_step_is_bounded_and_its_progress_survives_a_restart() {
        // Two claims in one test because they are the same claim seen from two sides: the step does
        // bounded work, and the progress that says so is **in the database**, not in a variable the
        // process is carrying.
        let path = temp_cache_path("rebuild-resume");
        let cache = Cache::open(&path).expect("open");
        cache
            .upsert_summaries(
                "acc",
                "INBOX",
                &(1..=5)
                    .map(|uid| {
                        summary(
                            uid,
                            &format!("主题{uid}"),
                            "boss@example.com",
                            i64::from(uid),
                        )
                    })
                    .collect::<Vec<_>>(),
            )
            .expect("summaries");
        drop(cache);

        let cache = Cache::open(&path).expect("reopen");
        cache.rebuild_search_begin().expect("begin");
        drop(cache);

        // Every step opens the file, does bounded work and closes it again, so "the app was closed
        // and reopened between steps" is structural here rather than a `drop` in the middle of a
        // loop — and the numbers each step reports can only have come from the file.
        let mut steps = Vec::new();
        for _ in 0..10 {
            let cache = Cache::open(&path).expect("open for one step");
            let progress = cache.rebuild_search_step(2).expect("step");
            steps.push(progress);
            if progress.remaining == 0 {
                break;
            }
        }

        assert_eq!(
            steps[0].done, 2,
            "the first step did bounded work, not all five"
        );
        assert_eq!(steps[0].total, 5);
        assert!(
            steps[0].remaining > 0,
            "there is more to do, and the UI is told so"
        );
        // Monotone and never over-reported: the two claims always add up to the target.
        for (i, step) in steps.iter().enumerate() {
            assert_eq!(
                step.done + step.remaining,
                step.total,
                "step {i} arithmetic"
            );
            if i > 0 {
                assert!(
                    step.done >= steps[i - 1].done,
                    "progress must not go backwards across a restart"
                );
            }
        }
        let cache = Cache::open(&path).expect("final reopen");
        assert_eq!(uids(&cache, "主题3"), vec![3], "the finished index answers");
    }

    // ---------------------------------------------------------------- query syntax

    #[test]
    fn parse_query_understands_field_prefixes_and_falls_back_to_literal_text() {
        use crate::cache::QueryTerm::*;
        use crate::query::QueryColumn::*;

        assert_eq!(
            parse_query("账单"),
            vec![Text {
                column: None,
                text: "账单".to_string()
            }]
        );
        assert_eq!(
            parse_query("subject:账单"),
            vec![Text {
                column: Some(Subject),
                text: "账单".to_string()
            }]
        );
        assert_eq!(
            parse_query("from:boss@example.com"),
            vec![Text {
                column: Some(From),
                text: "boss@example.com".to_string()
            }]
        );
        assert_eq!(parse_query("has:attachment"), vec![HasAttachment]);
        assert_eq!(
            parse_query("frm:boss"),
            vec![Text {
                column: None,
                text: "frm:boss".to_string()
            }],
            "a prefix we do not know is literal text: a typo must still be searchable"
        );
        assert_eq!(
            parse_query("filename:报告.pdf"),
            vec![Text {
                column: Some(Body),
                text: "报告.pdf".to_string()
            }],
            "attachment names are indexed in the body column (round 71), so filename: maps there"
        );
        assert_eq!(
            parse_query("From:boss"),
            vec![Text {
                column: Some(From),
                text: "boss".to_string()
            }],
            "prefixes are case-insensitive the way users type them"
        );
        assert_eq!(parse_query("  账单   has:attachment  ").len(), 2);
        assert_eq!(parse_query("").len(), 0);
        assert_eq!(
            parse_query("subject:"),
            vec![Text {
                column: None,
                text: "subject:".to_string()
            }],
            "a prefix with nothing after it is not a prefix"
        );
    }

    #[test]
    fn a_prefixed_query_scopes_to_its_column_and_still_matches_chinese() {
        let cache = Cache::open_in_memory().unwrap();
        cache
            .upsert_summaries(
                "acc",
                "INBOX",
                &[
                    summary(1, "月度对账单", "bank@example.com", 100),
                    summary(2, "开会通知", "boss@example.com", 200),
                ],
            )
            .unwrap();
        cache
            .put_detail(
                "acc",
                "INBOX",
                &detail(1, "月度对账单", "正文里有账单两个字"),
            )
            .unwrap();

        // Scoped to the subject: the body of message 1 also says 账单, and must not be found.
        assert_eq!(uids(&cache, "subject:对账单"), vec![1]);
        assert_eq!(uids(&cache, "from:bank"), vec![1]);
        assert_eq!(uids(&cache, "from:boss"), vec![2]);
        assert_eq!(
            uids(&cache, "body:正文"),
            vec![1],
            "body: is the column that means the text, with or without a prefix"
        );
        // Chinese inside a scope: the tokenisation must not change with the prefix.
        assert_eq!(uids(&cache, "subject:对账"), vec![1]);
        // An ASCII term through a scoped column behaves like the unscoped one.
        assert_eq!(uids(&cache, "from:example.com").len(), 2);
    }

    #[test]
    fn has_attachment_only_finds_mail_that_has_one() {
        let cache = Cache::open_in_memory().unwrap();
        cache
            .upsert_summaries(
                "acc",
                "INBOX",
                &[
                    summary(1, "季度报表", "boss@example.com", 100),
                    summary(2, "发票已开具", "shop@example.com", 200),
                    summary(3, "纯正文", "plain@example.com", 300),
                ],
            )
            .unwrap();
        cache
            .put_detail(
                "acc",
                "INBOX",
                &detail_with_attachments(1, "季度报表", "看附件", &["报告.pdf"]),
            )
            .unwrap();
        cache
            .put_detail(
                "acc",
                "INBOX",
                &detail_with_attachments(2, "发票已开具", "看附件", &["invoice.pdf"]),
            )
            .unwrap();
        // Message 3 has no body stored at all: no row in `bodies`, so it cannot prove an attachment.
        assert_eq!(
            uids(&cache, "has:attachment"),
            vec![2, 1],
            "newest first, both attachments"
        );

        // Combined with a field: only the attachment mail that also matches.
        cache
            .put_detail("acc", "INBOX", &detail(1, "季度报表", "看附件"))
            .expect("re-store 1 as body-only");
        assert_eq!(
            uids(&cache, "has:attachment from:shop"),
            vec![2],
            "the EXISTS clause must narrow the FTS hits, not replace them"
        );
        assert_eq!(
            uids(&cache, "has:attachment invoice"),
            vec![2],
            "a bare word plus the attachment predicate"
        );
    }

    #[test]
    fn an_unbounded_query_still_finds_a_literal_colon_word() {
        // The regression that a "silently return nothing" prefix parser would produce: the user
        // searches for text that happens to contain a colon.
        let cache = Cache::open_in_memory().unwrap();
        cache
            .upsert_summaries(
                "acc",
                "INBOX",
                &[summary(1, "会议: 十点", "boss@example.com", 100)],
            )
            .unwrap();
        assert_eq!(uids(&cache, "会议: 十点").len(), 1);
        assert_eq!(uids(&cache, "subject:会议").len(), 1);
    }

    // ---------------------------------------------------------------- structured predicates

    /// Four messages that differ only in the things predicates test: date, folder, flags,
    /// attachments. Dates are chosen around 2026-01-01 (1_767_225_600) so the boundaries bite.
    fn seed_predicates(cache: &Cache) {
        cache
            .upsert_summaries(
                "acc",
                "INBOX",
                &[
                    summary(1, "一", "a@x.com", 1_000_000_000),
                    summary(2, "二", "b@x.com", 1_767_225_600),
                    summary(3, "三", "c@x.com", 1_767_300_000),
                    summary(4, "四", "d@x.com", 1_800_000_000),
                ],
            )
            .unwrap();
        cache
            .upsert_summaries("acc", "Sent", &[summary(5, "五", "e@x.com", 1_700_000_000)])
            .unwrap();
        cache
            .put_detail(
                "acc",
                "INBOX",
                &detail_with_attachments(4, "四", "正文四", &["附件四.pdf"]),
            )
            .unwrap();
    }

    #[test]
    fn structured_predicates_narrow_the_same_hits_the_text_would_find() {
        let cache = Cache::open_in_memory().unwrap();
        seed_predicates(&cache);

        // Boundaries are inclusive on both sides: "inclusive" is one rule, not two, and a user
        // typing a day means the whole day.
        assert_eq!(
            uids(&cache, "after:2026-01-01"),
            vec![4, 3, 2],
            "a cut-off date includes mail from that day (newest first)"
        );
        assert_eq!(uids(&cache, "before:2026-01-01"), vec![2, 5, 1]);

        // Every row in the fixture is unread — including the one in `Sent` — which is what makes
        // `in:` worth testing next to it.
        assert_eq!(uids(&cache, "is:unread").len(), 5);
        assert_eq!(uids(&cache, "in:sent"), vec![5]);
        assert_eq!(
            uids(&cache, "in:INBOX is:unread").len(),
            4,
            "two predicates narrow together"
        );

        // Attachments alone and mixed with a date — the combination that exercises both query
        // shapes at once (text from the index, predicate from the table).
        assert_eq!(uids(&cache, "has:attachment"), vec![4]);
        assert_eq!(uids(&cache, "has:attachment after:2026-01-01"), vec![4]);
        assert!(uids(&cache, "has:attachment before:2026-01-01").is_empty());

        // Another account's mail must never answer this account's query, even through `account:`.
        cache
            .upsert_summaries(
                "other",
                "INBOX",
                &[summary(9, "九", "z@x.com", 1_900_000_000)],
            )
            .unwrap();
        assert!(
            uids(&cache, "after:2026-01-01").iter().all(|uid| *uid != 9),
            "the account condition is always present, with or without `account:`"
        );
        // With the override in place, `account:other` really does answer for the other account.
        assert_eq!(
            uids(&cache, "account:other"),
            vec![9],
            "an explicit account replaces the caller's scope"
        );
        assert_eq!(
            uids(&cache, "account:other in:INBOX").len(),
            1,
            "…and combines with the other predicates"
        );
        assert_eq!(
            uids(&cache, "account:other after:2026-01-01"),
            vec![9],
            "the other account's mail is in 2030, so the date keeps it"
        );
    }

    // ---------------------------------------------------------------- size predicates

    #[test]
    fn size_predicates_use_the_size_the_server_actually_said() {
        let cache = Cache::open_in_memory().unwrap();
        // The fixture helper writes size: Some(1024) for every row, so sizes come from the rows
        // themselves here: 1 KB, 1 MB, and one with no size at all.
        cache
            .upsert_summaries(
                "acc",
                "INBOX",
                &[
                    summary(1, "小", "a@x.com", 100),
                    summary(2, "大", "b@x.com", 200),
                    summary(3, "未知", "c@x.com", 300),
                ],
            )
            .unwrap();
        {
            // Give 2 a real one-megabyte size and leave 3 without one.
            let mut two = summary(2, "大", "b@x.com", 200);
            two.size = Some(1_000_000);
            cache
                .upsert_summaries("acc", "INBOX", &[two])
                .expect("re-write row 2 with a size");
            let mut three = summary(3, "未知", "c@x.com", 300);
            three.size = None;
            cache
                .upsert_summaries("acc", "INBOX", &[three])
                .expect("re-write row 3 with no size");
        }

        // The unit decision, end to end: a KB is a thousand bytes here (`size.rs`), so a 1024-byte
        // message is not "smaller than 1KB" — it is "smaller than 1 KiB". That surprise is exactly
        // why the choice is documented and pinned rather than left to whichever library was handy.
        assert!(
            uids(&cache, "smaller:1KB").is_empty(),
            "1KB means 1000 bytes, and 1024 is more"
        );
        assert_eq!(
            uids(&cache, "smaller:1KiB"),
            vec![1],
            "…and 1 KiB means 1024"
        );
        assert_eq!(uids(&cache, "larger:1MB"), vec![2], "one megabyte or more");
        assert_eq!(
            uids(&cache, "larger:100KB"),
            vec![2],
            "the unknown-size row is not reported as large: NULL is not a comparison"
        );
        assert_eq!(
            uids(&cache, "smaller:100KB"),
            vec![1],
            "…and not as small either. An unknown size is its own state, not zero."
        );
        assert_eq!(
            uids(&cache, "larger:1KB").len(),
            2,
            "both known sizes are above 1000 bytes"
        );
        // A unit is not a word: `10MB` as a bare search must not silently become a size filter.
        assert_eq!(uids(&cache, "larger:900KB").len(), 1);
        assert!(
            uids(&cache, "larger:big").is_empty(),
            "an unreadable size is dropped, and the hint says so"
        );
    }

    // ---------------------------------------------------------------- paging

    #[test]
    fn paging_by_offset_neither_repeats_nor_skips_a_row() {
        // Paging is the model the list moved to (round 75: `limit` used to grow and every press
        // re-read the whole visible window). The property that makes it safe is this one: the
        // pages, concatenated, are exactly the rows a single unbounded read would have returned —
        // no row twice, none missing. An overlap hides mail (a row that shifts between pages) and a
        // gap hides it just as permanently, so both directions are asserted here rather than left to
        // the SQL being obviously right.
        let cache = Cache::open_in_memory().unwrap();
        cache
            .upsert_summaries(
                "acc",
                "INBOX",
                &(1..=10)
                    .map(|uid| {
                        summary(
                            uid,
                            &format!("主题{uid}"),
                            "boss@example.com",
                            i64::from(uid),
                        )
                    })
                    .collect::<Vec<_>>(),
            )
            .unwrap();

        let mut seen: Vec<u32> = Vec::new();
        for offset in (0..12).step_by(4) {
            let page = cache
                .list_summaries("acc", "INBOX", 4, offset)
                .expect("page");
            for row in &page {
                assert!(
                    !seen.contains(&row.uid),
                    "uid {} appeared on two pages (offset {offset})",
                    row.uid
                );
            }
            // Newest first, per page: the renderer appends without re-sorting, so this is load-bearing.
            let uids: Vec<u32> = page.iter().map(|m| m.uid).collect();
            assert!(
                uids.windows(2).all(|w| w[0] > w[1]),
                "page at offset {offset} is not ordered newest-first: {uids:?}"
            );
            seen.extend(uids);
        }
        assert_eq!(
            seen.len(),
            10,
            "three pages of four rows must cover exactly the ten rows there are"
        );
        // Past the end: empty, not an error, and not a repeat of the last page.
        assert!(
            cache
                .list_summaries("acc", "INBOX", 4, 10)
                .expect("past end")
                .is_empty()
        );
        assert!(
            cache
                .list_summaries("acc", "INBOX", 4, 99)
                .expect("far past")
                .is_empty()
        );
    }

    // ---------------------------------------------------------------- rebuild

    #[test]
    fn a_rebuild_clears_the_index_before_it_starts() {
        // The half of the operation users can get wrong: if `begin` quietly rebuilt as it went, a
        // rebuild would be un-cancellable in the only sense that matters — there would be no moment
        // where "stop" means the index is *partly* rebuilt and the user can see that.
        let cache = Cache::open_in_memory().unwrap();
        seed_rebuild_fixture(&cache);
        assert_eq!(uids(&cache, "对账单"), vec![1], "indexed before");

        cache.rebuild_search_begin().expect("begin");
        assert!(
            uids(&cache, "对账单").is_empty(),
            "the index is empty right after begin"
        );
        let progress = cache.rebuild_search_step(1).expect("step");
        assert_eq!(progress.done, 1);
        assert_eq!(
            progress.remaining, 2,
            "one row at a time, so progress is visible"
        );
    }

    #[test]
    fn a_cancelled_rebuild_still_answers_for_what_was_rebuilt() {
        // Cancelling means "no next step". The index is then partly rebuilt, and a partly rebuilt
        // index must be **useful and explicable**: the rebuilt rows answer, the rest do not, and
        // nothing is left in a state that says "rebuilding" forever.
        let cache = Cache::open_in_memory().unwrap();
        seed_rebuild_fixture(&cache);
        cache.rebuild_search_begin().expect("begin");
        cache.rebuild_search_step(1).expect("one step, then stop");

        let progress = cache.rebuild_search_step(0).expect("ask where we are");
        assert_eq!(
            progress.done, 1,
            "a zero-limit step still reports the truth"
        );
        assert_eq!(progress.remaining, 2);

        // Whatever row the single step rebuilt is searchable; nothing pretends otherwise about the
        // rest, and re-entering the loop later picks up exactly where it stopped.
        let rebuilt = progress.done;
        let total = progress.total;
        assert!(
            rebuilt < total,
            "the fixture is partly rebuilt, which is the state under test"
        );
        let mut guard = 0;
        while cache.rebuild_search_step(2).expect("resume").remaining > 0 && guard < 5 {
            guard += 1;
        }
        assert_eq!(uids(&cache, "对账单"), vec![1], "fully rebuilt at the end");
    }

    #[test]
    fn a_truncated_body_does_not_take_the_attachment_names_with_it() {
        // The cap belongs to the body alone (C9: `bodies.detail` keeps the whole message; only the
        // index copy is cut). Appending the names *after* the truncation is what keeps a 64 KB body
        // from pushing the filename out of the index — the name is short and is the thing the user
        // types on a long thread.
        let cache = Cache::open_in_memory().unwrap();
        cache
            .upsert_summaries(
                "acc",
                "INBOX",
                &[summary(1, "大附件", "boss@example.com", 100)],
            )
            .unwrap();
        let long_body = format!(
            "{}唯一标记",
            "长".repeat(crate::index::DEFAULT_BODY_INDEX_CHARS + 1024)
        );
        cache
            .put_detail(
                "acc",
                "INBOX",
                &detail_with_attachments(1, "大附件", &long_body, &["年度总结.docx"]),
            )
            .unwrap();

        assert_eq!(
            cache.search("acc", "唯一标记", 10).unwrap().len(),
            0,
            "the cap still applies to the body"
        );
        assert_eq!(
            cache.search("acc", "年度总结", 10).unwrap().len(),
            1,
            "the name survives a body longer than the cap"
        );
    }

    #[test]
    fn a_message_without_attachments_indexes_unchanged() {
        // Regression guard for the row-shape change: a body-only message searches exactly as
        // before, and its row is current the moment it is written — no pending work, full coverage.
        let cache = Cache::open_in_memory().unwrap();
        cache
            .upsert_summaries(
                "acc",
                "INBOX",
                &[summary(1, "对账单", "bank@example.com", 100)],
            )
            .unwrap();
        cache
            .put_detail("acc", "INBOX", &detail(1, "对账单", "流水号 8848"))
            .unwrap();

        assert_eq!(cache.bodies_pending_count("acc").unwrap(), 0);
        let coverage = cache.body_coverage("acc").unwrap();
        assert_eq!((coverage.indexed, coverage.total), (1, 1));
        assert_eq!(cache.search("acc", "流水号", 10).unwrap().len(), 1);
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
    fn an_ascii_word_is_indexed_as_a_word_not_as_letters() {
        // The performance fix of 2026-10-06: `invoice` used to become `i n v o i c e`, which made
        // every English query a seven-token phrase (measured P50 944 ms on 50 000 messages).
        assert_eq!(to_tokens("invoice"), "invoice");
        assert_eq!(to_query("invoice"), "\"invoice\"");
        assert_eq!(to_tokens("Hello World"), "hello world");

        // Chinese is untouched: still one token per character, still a phrase on query.
        assert_eq!(to_tokens("账单 invoice"), "账 单 invoice");
        assert_eq!(to_query("invoice 账单"), "\"invoice\" AND \"账 单\"");

        // Punctuation separates on both sides, so an address still matches.
        assert_eq!(to_tokens("user@example.com"), "user example com");
        assert_eq!(to_query("example.com"), "\"example\" AND \"com\"");

        // A term whose characters are not all ASCII keeps the phrase form, character by character.
        assert_eq!(to_query("账单-发票"), "\"账 单\" AND \"发 票\"");
    }

    #[test]
    fn bodies_pending_returns_newest_first_and_respects_the_limit() {
        // The index pass only ever sees this list, so its order *is* the order mail becomes
        // searchable in — and the limit is what keeps one pass from becoming a whole-mailbox fetch.
        let cache = Cache::open_in_memory().unwrap();
        cache
            .upsert_summaries(
                "acc",
                "INBOX",
                &[
                    summary(10, "old", "a@example.com", 100),
                    summary(30, "new", "c@example.com", 300),
                    summary(20, "mid", "b@example.com", 200),
                ],
            )
            .unwrap();

        let pending = cache.bodies_pending("acc", 2).unwrap();
        assert_eq!(
            pending.iter().map(|p| p.uid).collect::<Vec<_>>(),
            vec![30, 20],
            "newest first, and no more than the limit"
        );

        // Reading one body removes it from the pending list: `put_detail` is the only writer.
        cache
            .put_detail("acc", "INBOX", &detail(30, "new", "正文"))
            .unwrap();
        let pending = cache.bodies_pending("acc", 10).unwrap();
        assert_eq!(
            pending.iter().map(|p| p.uid).collect::<Vec<_>>(),
            vec![20, 10]
        );
    }

    #[test]
    fn body_coverage_counts_only_messages_that_still_have_a_body() {
        // The orphan case is the one that would make the UI lie: a body whose message row is gone
        // is not searchable, so counting it would let the status line claim 105 out of 100.
        let cache = Cache::open_in_memory().unwrap();
        cache
            .upsert_summaries(
                "acc",
                "INBOX",
                &[
                    summary(1, "one", "a@example.com", 1),
                    summary(2, "two", "b@example.com", 2),
                    summary(3, "three", "c@example.com", 3),
                ],
            )
            .unwrap();
        cache
            .put_detail("acc", "INBOX", &detail(1, "one", "正文一"))
            .unwrap();
        cache
            .put_detail("acc", "INBOX", &detail(2, "two", "正文二"))
            .unwrap();
        cache.delete_message("acc", "INBOX", 2).unwrap();

        let coverage = cache.body_coverage("acc").unwrap();
        assert_eq!(
            coverage.total, 2,
            "the deleted message is gone from both counts"
        );
        assert_eq!(coverage.indexed, 1, "its body must not be counted either");
    }

    #[test]
    fn a_long_body_is_truncated_for_the_index_but_kept_whole_for_reading() {
        // C9: the truncation is the index's budget, not the user's copy. Cutting `bodies.detail`
        // would silently lose mail, so this test pins which side the cut lands on.
        let long: String = "账".repeat(crate::index::DEFAULT_BODY_INDEX_CHARS + 10);
        assert!(
            long.chars().count() > crate::index::DEFAULT_BODY_INDEX_CHARS,
            "the fixture must be longer than the cap"
        );

        let cache = Cache::open_in_memory().unwrap();
        cache
            .upsert_summaries("acc", "INBOX", &[summary(1, "s", "a@example.com", 1)])
            .unwrap();
        cache
            .put_detail("acc", "INBOX", &detail(1, "s", &long))
            .unwrap();

        // The FTS row is capped...
        let indexed = cache
            .db
            .read_guard()
            .query_row(
                "select length(search.body) from search where folder = 'INBOX' and uid = 1",
                [],
                |row| row.get::<_, i64>(0),
            )
            .unwrap();
        // The column holds the *tokenised* copy (`to_tokens` separates every CJK character with a
        // space), so the expected length is that of the truncated text, not of the raw one.
        let expected = to_tokens(truncate_chars(
            &long,
            crate::index::DEFAULT_BODY_INDEX_CHARS,
        ))
        .chars()
        .count();
        assert_eq!(
            indexed as usize, expected,
            "the index copy stops at the cap"
        );
        assert!(
            expected < to_tokens(&long).len(),
            "the cap must actually bite on this fixture"
        );

        // ...while what the user reads is the whole message.
        let stored = cache.get_detail("acc", "INBOX", 1).unwrap().unwrap();
        assert_eq!(stored.body_text.as_deref(), Some(long.as_str()));
    }

    #[test]
    fn truncate_chars_cuts_on_character_boundaries_and_never_panics() {
        // A byte cut on 账 would split it and panic on the slice; the mail this matters for is Chinese.
        assert_eq!(truncate_chars("abc", 10), "abc");
        assert_eq!(truncate_chars("abc", 2), "ab");
        assert_eq!(truncate_chars("账abc", 1), "账");
        assert_eq!(truncate_chars("账abc", 2), "账a");
        // Every boundary the function produces must be a valid split point.
        for text in ["账单", "a账b单", "𠮷野家"] {
            for limit in 0..=text.chars().count() + 1 {
                let cut = truncate_chars(text, limit);
                assert!(cut.len() <= text.len());
                assert!(text.starts_with(cut));
            }
        }
    }

    #[test]
    fn the_sidebar_badges_count_unread_per_folder() {
        let cache = Cache::open_in_memory().unwrap();
        cache
            .upsert_summaries(
                "acc",
                "INBOX",
                &[
                    summary(1, "one", "a@example.com", 1),
                    summary(2, "two", "b@example.com", 2),
                ],
            )
            .unwrap();
        cache
            .upsert_summaries(
                "acc",
                "[Gmail]/Spam",
                &[summary(3, "spam", "c@example.com", 3)],
            )
            .unwrap();

        // Everything starts unread (the schema default), and the folder with two leads.
        let counts = cache.unread_counts("acc").unwrap();
        assert_eq!(counts.get("INBOX").copied(), Some(2));
        assert_eq!(counts.get("[Gmail]/Spam").copied(), Some(1));

        // Reading one drops that folder's badge one step at a time — this is what a badge that
        // lies looks like, so it is asserted rather than assumed.
        cache.set_seen("acc", "INBOX", 1, true).unwrap();
        assert_eq!(
            cache.unread_counts("acc").unwrap().get("INBOX").copied(),
            Some(1)
        );
        cache.set_seen("acc", "INBOX", 2, true).unwrap();
        assert_eq!(cache.unread_counts("acc").unwrap().get("INBOX"), None);
        // ...and the other folder's badge is untouched by it.
        assert_eq!(
            cache
                .unread_counts("acc")
                .unwrap()
                .get("[Gmail]/Spam")
                .copied(),
            Some(1)
        );

        // A folder with no unread mail is absent, not zero: the renderer draws nothing for it.
        assert_eq!(cache.unread_count("acc").unwrap(), 1);
    }

    #[test]
    fn an_ascii_query_reaches_the_index_in_both_directions() {
        // The transformation test above pins the strings; this one pins that FTS5 actually agrees
        // with them, which is the part that silently stops being true.
        let cache = Cache::open_in_memory().unwrap();
        cache
            .upsert_summaries(
                "acc",
                "INBOX",
                &[
                    summary(1, "Quarterly invoice", "billing@example.com", 1),
                    summary(2, "账单", "a@b.c", 2),
                ],
            )
            .unwrap();
        assert_eq!(cache.search("acc", "invoice", 5).unwrap()[0].uid, 1);
        assert_eq!(
            cache.search("acc", "INVOICE", 5).unwrap()[0].uid,
            1,
            "case folds"
        );
        assert_eq!(
            cache.search("acc", "billing", 5).unwrap()[0].uid,
            1,
            "sender"
        );
        // Substring matching inside an English word is the deliberate casualty of the fix.
        assert!(cache.search("acc", "invoi", 5).unwrap().is_empty());
        // A word that is not there still finds nothing.
        assert!(cache.search("acc", "remittance", 5).unwrap().is_empty());
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
    // ------------------------------------------------- index health and heal

    /// Put one message in the cache the way the sync path does, so the index has a row to keep.
    ///
    /// The summary alone leaves the FTS row header-only, which is fine for the tests below — the
    /// damage they do is to the row, not to what is in it.
    fn seed_one(cache: &Cache, account_id: &str, subject: &str) -> u32 {
        let uid = 11;
        cache
            .upsert_summaries(
                account_id,
                "INBOX",
                &[summary(uid, subject, "shop@example.com", 1_700_000_000)],
            )
            .expect("summary");
        uid
    }

    #[test]
    fn a_healthy_index_says_so_and_a_heal_does_nothing() {
        // The boring half, and the half that keeps heal honest: a healer that cannot tell healthy from
        // broken rebuilds every time somebody asks, and the user pays for the rebuild either way.
        let cache = Cache::open_in_memory().unwrap();
        seed_one(&cache, "acc", "订单");

        let health = cache.index_health("acc").expect("health");
        assert!(health.fts_ok);
        assert_eq!(health.messages, 1);
        assert_eq!(health.indexed, 1);
        assert_eq!(health.orphan_bodies, 0);

        let healed = cache.heal_index("acc", 100).expect("heal");
        assert_eq!(
            (healed.rebuilt, healed.orphans_deleted),
            (0, 0),
            "nothing to do means nothing to do"
        );
    }

    #[test]
    fn a_row_the_index_lost_is_found_again_by_a_heal() {
        // The whole reason this round exists. The index is a second copy of the mail, and a row can
        // vanish from it without anything crashing: search simply answers with fewer results, and
        // the user concludes the mail never arrived. Nothing was asking "is the index still right".
        let cache = Cache::open_in_memory().unwrap();
        let uid = seed_one(&cache, "acc", "订单");

        // Break it behind the index's back — the same shape a crash between writes produces.
        cache
            .db
            .write_blocking(move |conn: &Connection| {
                conn.execute("delete from search where uid = ?1", params![uid])
                    .map(|_| ())
                    .map_err(Into::into)
            })
            .expect("corrupt the index");

        // And the failure is silent: the message still opens, the list still shows it.
        assert_eq!(
            cache.search("acc", "订单", 10).expect("search").len(),
            0,
            "search answers fewer results, which is what a user sees as \"that mail never came\""
        );
        let health = cache.index_health("acc").expect("health");
        assert!(
            health.indexed < health.messages,
            "a missing row is visible in the count — and it is a trustworthy signal because every write
             path puts the message row and its index row in one transaction"
        );

        cache.heal_index("acc", 100).expect("heal");
        let health = cache.index_health("acc").expect("health after");
        assert_eq!(health.indexed, health.messages, "the row is back");
        assert!(health.fts_ok, "and the index says so itself");
        assert_eq!(
            cache.search("acc", "订单", 10).expect("search again").len(),
            1,
            "and the user's mail is findable again"
        );
    }

    #[test]
    fn orphan_bodies_are_cleaned_without_touching_the_live_ones() {
        // Orphans inflate "正文已索引 N/M" — a count that lies about coverage is worse than none.
        let cache = Cache::open_in_memory().unwrap();
        let uid = seed_one(&cache, "acc", "订单");
        // A body that belongs to the live message, so there is something to leave alone.
        cache
            .put_detail(
                "acc",
                "INBOX",
                &detail(uid, "订单", "正文里有对账单这三个字"),
            )
            .expect("body");
        // A body whose message row is gone: what a cleared cache or a failed migration leaves.
        cache
            .db
            .write_blocking(move |conn: &Connection| {
                conn.execute(
                    "insert into bodies (account_id, folder, uid, detail, fetched_at)
                     values ('acc', 'Trash', 99, '孤儿的正文', 1)",
                    [],
                )
                .map(|_| ())
                .map_err(Into::into)
            })
            .expect("plant an orphan");

        let health = cache.index_health("acc").expect("health");
        assert_eq!(health.orphan_bodies, 1, "the extra row is named");

        let healed = cache.heal_index("acc", 100).expect("heal");
        assert_eq!(healed.orphans_deleted, 1);
        let health = cache.index_health("acc").expect("health after");
        assert_eq!(health.orphan_bodies, 0);
        // The live body is still there — a cleanup that removes the wrong row is worse than the mess.
        let kept = cache
            .db
            .read_guard()
            .query_row(
                "select count(*) from bodies where account_id = 'acc' and folder = 'INBOX' and uid = 11",
                [],
                |row| row.get::<_, i64>(0),
            )
            .expect("the live body is still indexed");
        assert_eq!(kept, 1, "the orphan is gone and the real one stays");
    }

    #[test]
    fn another_accounts_mess_is_not_cleaned_by_this_accounts_heal() {
        let cache = Cache::open_in_memory().unwrap();
        cache
            .upsert_summaries(
                "a",
                "INBOX",
                &[MessageSummary {
                    uid: 1,
                    subject: "甲的".into(),
                    from_name: None,
                    from_addr: None,
                    date: Some(1),
                    size: None,
                    seen: false,
                    flagged: false,
                    thread_key: None,
                    subject_key: Some("甲".into()),
                }],
            )
            .expect("a");
        cache
            .db
            .write_blocking(|conn: &Connection| {
                conn.execute(
                    "insert into bodies (account_id, folder, uid, detail, fetched_at)
                     values ('b', 'INBOX', 1, '乙的孤儿', 1)",
                    [],
                )
                .map(|_| ())
                .map_err(Into::into)
            })
            .expect("b's orphan");

        let healed = cache.heal_index("a", 100).expect("heal a");
        assert_eq!(healed.orphans_deleted, 0, "not this account's row");
        assert_eq!(
            cache.index_health("b").expect("b's health").orphan_bodies,
            1,
            "still there, where it belongs"
        );
    }

    #[test]
    fn the_health_count_comes_from_the_index_itself() {
        // `indexed` asked of the FTS table rather than computed from `messages`: a detector that
        // cannot see what it is checking for reports healthy while rows are missing.
        let cache = Cache::open_in_memory().unwrap();
        seed_one(&cache, "acc", "订单");
        let health = cache.index_health("acc").expect("health");
        assert_eq!((health.messages, health.indexed), (1, 1));

        // Drop the FTS row again and the count follows it down: the answer comes from the index.
        cache
            .db
            .write_blocking(|conn: &Connection| {
                conn.execute("delete from search where uid = 11", [])
                    .map(|_| ())
                    .map_err(Into::into)
            })
            .expect("drop the row");
        let health = cache.index_health("acc").expect("health");
        assert_eq!(
            (health.messages, health.indexed),
            (1, 0),
            "the messages count holds and the index count drops — which is the whole signal"
        );
    }
}
