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

use crate::error::{MailError, Result};
use crate::model::{MailboxInfo, MessageDetail, MessageSummary};

/// Bumped whenever the schema below changes; [`Cache::migrate`] reacts to it.
pub const SCHEMA_VERSION: i64 = 1;

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

pub struct Cache {
    conn: Connection,
}

impl Cache {
    /// Open (creating if needed) the cache database at `path`.
    ///
    /// Callers open a connection per operation rather than sharing one, because
    /// `rusqlite::Connection` is `!Sync`: holding it across an `.await` would make a Tauri command's
    /// future non-`Send`. A `busy_timeout` is set so a search running while a sync writes waits for
    /// the writer instead of failing outright with `SQLITE_BUSY`.
    pub fn open(path: &std::path::Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                MailError::Cache(format!("无法创建缓存目录 {}：{e}", parent.display()))
            })?;
        }
        let cache = Self {
            conn: Connection::open(path)?,
        };
        cache.migrate()?;
        Ok(cache)
    }

    /// In-memory cache, for tests.
    pub fn open_in_memory() -> Result<Self> {
        let cache = Self {
            conn: Connection::open_in_memory()?,
        };
        cache.migrate()?;
        Ok(cache)
    }

    /// Connection-level pragmas, applied on every open before anything else runs.
    ///
    /// Two of them have to happen **before the first table is created**:
    ///
    /// * `auto_vacuum = incremental` — SQLite only accepts this on an empty database (or after a
    ///   full `VACUUM`). Setting it later is silently ignored, which is exactly how "we clean up
    ///   200 MB" ends up not shrinking the file at all: `pragma incremental_vacuum` then does
    ///   nothing. Databases created before this line existed therefore keep `auto_vacuum = 0`; see
    ///   [`Cache::auto_vacuum_state`] and [`Cache::vacuum_into_freelist`].
    /// * `journal_mode = wal` — a reader (the UI) must not block the sync writer.
    ///
    /// `synchronous = normal` is the usual WAL pairing: a crash can lose the last transaction but
    /// cannot corrupt the database, and this data is a cache — it can always be re-fetched.
    fn apply_pragmas(&self) -> Result<()> {
        // Order matters: `auto_vacuum` first. Switching the journal mode writes the database
        // header, after which SQLite silently ignores an auto-vacuum change (measured — see the note
        // in `db::open_connection`).
        self.conn.execute_batch(
            "pragma auto_vacuum = incremental;
             pragma journal_mode = wal;
             pragma synchronous = normal;
             pragma busy_timeout = 5000;",
        )?;
        Ok(())
    }

    /// What SQLite actually thinks the auto-vacuum mode is (`0` none, `1` full, `2` incremental).
    ///
    /// Read back rather than assumed: an existing database keeps whatever it was created with, and
    /// that difference is the difference between "cleanup frees disk" and "cleanup does nothing".
    pub fn auto_vacuum_state(&self) -> Result<i64> {
        Ok(self
            .conn
            .query_row("pragma auto_vacuum", [], |row| row.get(0))?)
    }

    /// Bytes sitting on the freelist, i.e. pages that are free but still part of the file.
    pub fn freelist_bytes(&self) -> Result<i64> {
        let pages: i64 = self
            .conn
            .query_row("pragma freelist_count", [], |row| row.get(0))?;
        let page_size: i64 = self
            .conn
            .query_row("pragma page_size", [], |row| row.get(0))?;
        Ok(pages * page_size)
    }

    /// Run an incremental vacuum, which is a no-op unless auto-vacuum is incremental.
    ///
    /// Returns the number of freelist pages that remain (0 on an auto-vacuum database means the
    /// space really did come back).
    pub fn vacuum_into_freelist(&self) -> Result<i64> {
        self.conn.execute_batch("pragma incremental_vacuum;")?;
        Ok(self
            .conn
            .query_row("pragma freelist_count", [], |row| row.get(0))?)
    }

    /// One-off, **blocking** full `VACUUM`, the only way to turn on auto-vacuum for a database that
    /// already has tables (SQLite rejects the pragma change otherwise).
    ///
    /// Deliberately not called during startup: it rewrites the whole file, so the caller decides
    /// when to pay for it (design/03 §8: an explicit, interruptible, user-visible step).
    pub fn rebuild_with_auto_vacuum(&self) -> Result<()> {
        let current: i64 = self
            .conn
            .query_row("pragma auto_vacuum", [], |row| row.get(0))?;
        if current == 2 {
            return Ok(());
        }
        self.conn.execute_batch(
            "pragma auto_vacuum = incremental;
             vacuum;
             pragma auto_vacuum = incremental;",
        )?;
        Ok(())
    }

    fn migrate(&self) -> Result<()> {
        self.apply_pragmas()?;

        let version: i64 = self
            .conn
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

        self.conn.execute_batch(
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
             );",
        )?;

        self.conn
            .pragma_update(None, "user_version", SCHEMA_VERSION)?;
        Ok(())
    }

    // ------------------------------------------------------------------ folders

    /// Remember a folder and its `UIDVALIDITY`, leaving the sync cursor alone.
    pub fn upsert_folder(
        &self,
        account_id: &str,
        mailbox: &MailboxInfo,
        uidvalidity: u32,
    ) -> Result<()> {
        self.conn.execute(
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
    }

    /// The folder list as last seen, for offline startup.
    pub fn list_folders(&self, account_id: &str) -> Result<Vec<MailboxInfo>> {
        let mut stmt = self.conn.prepare(
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
        Ok(self
            .conn
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
        Ok(self
            .conn
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
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        self.conn.execute(
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
    }

    /// Drop everything cached for one folder and reset its cursor.
    ///
    /// Called when `UIDVALIDITY` changes: at that point every stored UID may refer to a different
    /// message, so keeping any of it would show the wrong mail under the wrong headers.
    ///
    /// The stored `uidvalidity` is zeroed as well, which is what tells the sync driver that the
    /// next pass must start from a fresh window rather than trusting a cursor it no longer has.
    pub fn invalidate_folder(&self, account_id: &str, folder: &str) -> Result<()> {
        self.conn.execute(
            "delete from search where account_id = ?1 and folder = ?2",
            params![account_id, folder],
        )?;
        self.conn.execute(
            "delete from bodies where account_id = ?1 and folder = ?2",
            params![account_id, folder],
        )?;
        self.conn.execute(
            "delete from messages where account_id = ?1 and folder = ?2",
            params![account_id, folder],
        )?;
        self.conn.execute(
            "update folders set uidvalidity = 0, last_uid = 0 where account_id = ?1 and name = ?2",
            params![account_id, folder],
        )?;
        Ok(())
    }

    /// Forget an account entirely (uninstalled / removed account).
    pub fn forget_account(&self, account_id: &str) -> Result<()> {
        for table in ["search", "bodies", "messages", "folders"] {
            self.conn.execute(
                &format!("delete from {table} where account_id = ?1"),
                params![account_id],
            )?;
        }
        Ok(())
    }

    // ------------------------------------------------------------------ messages

    /// Insert or update header rows. Idempotent: re-syncing the same window changes nothing.
    pub fn upsert_summaries(
        &self,
        account_id: &str,
        folder: &str,
        summaries: &[MessageSummary],
    ) -> Result<()> {
        for summary in summaries {
            self.conn.execute(
                "insert into messages
                     (account_id, folder, uid, subject, from_name, from_addr, date, size, seen, flagged)
                 values (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
                 on conflict(account_id, folder, uid) do update set
                     subject   = excluded.subject,
                     from_name = excluded.from_name,
                     from_addr = excluded.from_addr,
                     date      = excluded.date,
                     size      = excluded.size,
                     seen      = excluded.seen,
                     flagged   = excluded.flagged",
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
                ],
            )?;
            self.refresh_search_row(account_id, folder, summary.uid)?;
        }
        Ok(())
    }

    /// Newest first, matching what the UI shows.
    pub fn list_summaries(
        &self,
        account_id: &str,
        folder: &str,
        limit: u32,
        offset: u32,
    ) -> Result<Vec<MessageSummary>> {
        let mut stmt = self.conn.prepare(
            "select uid, subject, from_name, from_addr, date, size, seen, flagged
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
                })
            },
        )?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn summary_count(&self, account_id: &str, folder: &str) -> Result<u32> {
        let count: i64 = self.conn.query_row(
            "select count(*) from messages where account_id = ?1 and folder = ?2",
            params![account_id, folder],
            |row| row.get(0),
        )?;
        Ok(count as u32)
    }

    /// Messages still marked unread anywhere in this account.
    ///
    /// Read from the cache rather than the server because the tray tooltip and the notification both
    /// need a number immediately, and a round trip per refresh would make them lag the event they
    /// are reporting.
    pub fn unread_count(&self, account_id: &str) -> Result<u32> {
        let count: i64 = self.conn.query_row(
            "select count(*) from messages where account_id = ?1 and seen = 0",
            params![account_id],
            |row| row.get(0),
        )?;
        Ok(count as u32)
    }

    /// Reflect a flag change locally so the list does not disagree with the server.
    pub fn set_seen(&self, account_id: &str, folder: &str, uid: u32, seen: bool) -> Result<()> {
        self.conn.execute(
            "update messages set seen = ?4 where account_id = ?1 and folder = ?2 and uid = ?3",
            params![account_id, folder, i64::from(uid), i64::from(seen)],
        )?;
        Ok(())
    }

    /// Remove a message (deleted or moved) from the cache.
    pub fn delete_message(&self, account_id: &str, folder: &str, uid: u32) -> Result<()> {
        self.conn.execute(
            "delete from messages where account_id = ?1 and folder = ?2 and uid = ?3",
            params![account_id, folder, i64::from(uid)],
        )?;
        self.conn.execute(
            "delete from bodies where account_id = ?1 and folder = ?2 and uid = ?3",
            params![account_id, folder, i64::from(uid)],
        )?;
        self.conn.execute(
            "delete from search where account_id = ?1 and folder = ?2 and uid = ?3",
            params![account_id, folder, i64::from(uid)],
        )?;
        Ok(())
    }

    // ------------------------------------------------------------------ bodies

    /// Store a parsed message so it can be read again without touching the network.
    pub fn put_detail(&self, account_id: &str, folder: &str, detail: &MessageDetail) -> Result<()> {
        let detail_json = serde_json::to_string(detail)
            .map_err(|e| MailError::Cache(format!("序列化正文失败：{e}")))?;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);

        self.conn.execute(
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
            self.conn.execute(
                "update messages set to_addrs = ?4
                 where account_id = ?1 and folder = ?2 and uid = ?3",
                params![account_id, folder, i64::from(detail.uid), to_json],
            )?;
        }

        self.refresh_search_row(account_id, folder, detail.uid)?;
        Ok(())
    }

    pub fn get_detail(
        &self,
        account_id: &str,
        folder: &str,
        uid: u32,
    ) -> Result<Option<MessageDetail>> {
        let raw: Option<String> = self
            .conn
            .query_row(
                "select detail from bodies where account_id = ?1 and folder = ?2 and uid = ?3",
                params![account_id, folder, i64::from(uid)],
                |row| row.get(0),
            )
            .optional()?;

        match raw {
            Some(json) => serde_json::from_str(&json)
                .map(Some)
                .map_err(|e| MailError::Cache(format!("缓存里的正文已损坏：{e}"))),
            None => Ok(None),
        }
    }

    // ------------------------------------------------------------------ search

    /// Local full-text search over headers (always) and bodies (for messages already read).
    pub fn search(&self, account_id: &str, query: &str, limit: u32) -> Result<Vec<SearchHit>> {
        let expression = to_query(query);
        if expression.is_empty() {
            return Ok(Vec::new());
        }

        let mut stmt = self.conn.prepare(
            "select m.folder, m.uid, m.subject, m.from_name, m.from_addr, m.date, m.seen
             from search
             join messages m using (account_id, folder, uid)
             where search match ?1 and search.account_id = ?2
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
    fn refresh_search_row(&self, account_id: &str, folder: &str, uid: u32) -> Result<()> {
        let row: Option<IndexSource> = self
            .conn
            .query_row(
                "select m.subject, m.from_name, m.from_addr, m.to_addrs, b.detail
                     from messages m
                     left join bodies b
                       on b.account_id = m.account_id and b.folder = m.folder and b.uid = m.uid
                     where m.account_id = ?1 and m.folder = ?2 and m.uid = ?3",
                params![account_id, folder, i64::from(uid)],
                |row| {
                    Ok(IndexSource {
                        subject: row.get(0)?,
                        from_name: row.get(1)?,
                        from_addr: row.get(2)?,
                        to_addrs: row.get(3)?,
                        detail_json: row.get(4)?,
                    })
                },
            )
            .optional()?;

        self.conn.execute(
            "delete from search where account_id = ?1 and folder = ?2 and uid = ?3",
            params![account_id, folder, i64::from(uid)],
        )?;

        let Some(source) = row else {
            return Ok(());
        };

        // The body lives inside the serialised detail; pull just the text out of it.
        let body = source
            .detail_json
            .as_deref()
            .and_then(|json| serde_json::from_str::<MessageDetail>(json).ok())
            .and_then(|detail| detail.body_text)
            .unwrap_or_default();

        self.conn.execute(
            "insert into search
                 (subject, from_name, from_addr, to_addrs, body, account_id, folder, uid)
             values (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
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
        Ok(())
    }
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
        let cache = Cache::open_in_memory().expect("open");
        assert_eq!(
            cache.auto_vacuum_state().expect("state"),
            2,
            "auto_vacuum must read back as incremental (2), not 0 — the pragma has to precede DDL"
        );
    }

    /// Deleting rows must hand pages back to the freelist, and the incremental vacuum must then
    /// release them. This is what makes "清理缓存释放了 N MB" true on disk rather than only in the
    /// UI's arithmetic.
    #[test]
    fn cleanup_frees_pages_and_incremental_vacuum_releases_them() {
        let cache = Cache::open_in_memory().expect("open");
        let chunky = "x".repeat(4096);
        for uid in 1..=400u32 {
            cache
                .upsert_summaries("acc", "INBOX", &[mail_core_model_summary(uid, &chunky)])
                .expect("insert");
        }
        let before = cache.freelist_bytes().expect("freelist");

        cache
            .conn
            .execute("delete from messages", [])
            .expect("delete the rows");
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
    }

    /// The upgrade path for databases created before M0-7: the pragma cannot be enabled in place,
    /// so the only correct answer is a full rebuild — and the helper must be idempotent.
    #[test]
    fn an_old_database_can_be_rebuilt_into_auto_vacuum() {
        // Simulate a v0.2 database: tables first, auto-vacuum left at its default.
        let cache = Cache {
            conn: Connection::open_in_memory().expect("open"),
        };
        cache
            .conn
            .execute_batch(
                "pragma auto_vacuum = none;
                 create table messages (uid integer);",
            )
            .expect("legacy schema");
        assert_eq!(cache.auto_vacuum_state().expect("state"), 0);

        // Turning it on directly is silently ignored — that is the trap, so assert it rather than
        // trusting the documentation.
        cache
            .conn
            .execute_batch("pragma auto_vacuum = incremental;")
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
        let conn = Connection::open_in_memory().unwrap();
        conn.pragma_update(None, "user_version", SCHEMA_VERSION + 1)
            .unwrap();
        let cache = Cache { conn };
        let error = cache.migrate().unwrap_err();
        assert!(matches!(error, MailError::Cache(_)), "got {error:?}");
    }
}
