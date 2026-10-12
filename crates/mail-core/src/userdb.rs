//! The **user** database: the half of the storage split that must never be deleted.
//!
//! `design/03` §3.1 splits storage in two, and the reason is a data-loss bug waiting to happen:
//! a cache can always be re-fetched from the server, so telling users "delete it if you are short of
//! space" is honest. Drafts, queued sends, pending flag changes and rules **cannot** be re-fetched.
//! Keeping them in the same file as a throwaway cache means the advice "just delete cache.db" quietly
//! destroys work.
//!
//! So this module owns the second file with its own pragmas (`synchronous = full`, because a draft
//! must survive a power cut) and its own schema version. It is deliberately small today: only what
//! the mail flow needs. Rules, contacts and signatures land with their features and bump
//! [`USER_SCHEMA_VERSION`] then.

use rusqlite::{Connection, OptionalExtension};

use crate::db::{Db, DbKind, DbOptions};
use crate::error::Result;
use crate::outbox::OutboxState;
use crate::sync::SyncWindow;

/// Schema version of `user.db`. Bump when the statements in [`UserStore::migrate`] change.
///
/// 2 added `quota_ledger` (`design/04` §5.7). Additive: `migrate` runs `create table if not
/// exists` for every table it owns, so an existing file picks the new one up on the next open.
pub const USER_SCHEMA_VERSION: i64 = 3;

/// How long a quota window lasts.
///
/// `design/04` §5.7 asks for a "local day" boundary. A rolling 24 hours from the first request of
/// the window is what is implemented: it is strictly conservative (no window can ever allow more
/// than the budget) and it needs no timezone database. The cost is that the reset time drifts with
/// when the app was first used that day, which the UI phrase "明天自动继续" covers honestly enough.
pub const QUOTA_WINDOW_SECS: i64 = 24 * 60 * 60;

/// `kv` key holding the chosen sync window.
pub const SYNC_WINDOW_KEY: &str = "sync_window";

/// The client registration an OAuth account signs in with.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct OAuthSettings {
    pub client_id: String,
    #[serde(default)]
    pub client_secret: Option<String>,
}

/// `kv` key holding one account's OAuth client id.
pub fn oauth_client_id_key(account_id: &str) -> String {
    format!("oauth_client_id:{account_id}")
}

/// `kv` key holding one account's OAuth client secret, if the provider needs one.
pub fn oauth_client_secret_key(account_id: &str) -> String {
    format!("oauth_client_secret:{account_id}")
}

/// Add a column to an existing table, once.
///
/// SQLite has no `alter table ... add column if not exists`, so the check is made against the schema
/// rather than the recorded version: the two can disagree (an interrupted migration, a file copied
/// between versions), and the schema is what a query actually runs against.
fn add_column_if_missing(
    conn: &rusqlite::Connection,
    table: &str,
    column: &str,
    kind: &str,
) -> Result<()> {
    let mut stmt = conn.prepare(&format!("pragma table_info({table})"))?;
    let existing = stmt
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    if !existing.iter().any(|name| name == column) {
        conn.execute_batch(&format!("alter table {table} add column {column} {kind};"))?;
    }
    Ok(())
}
/// `kv` key holding one account's notification preference.
///
/// Per account on purpose: somebody with a noisy work mailbox and a quiet personal one wants to
/// silence exactly one of them.
pub fn notify_key(account_id: &str) -> String {
    format!("notify:{account_id}")
}

/// Tables this database owns, in the order the diagnostics screen lists them.
pub const USER_TABLES: &[&str] = &[
    "schema_migrations",
    "kv",
    "drafts",
    "outbox",
    "pending_ops",
    "quota_ledger",
    "draft_mirrors",
];

/// Where the copy of a draft that **we** wrote to the server's Drafts folder is (`design/07` §5.1).
///
/// It exists for one reason: the next mirror of the same draft has to delete this copy first, or the
/// user's 草稿箱 collects one stale copy per autosave. `uid` is optional because `APPEND` does not
/// return one (A1-F43).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DraftMirror {
    pub draft_id: String,
    /// Which account it belongs to: a folder name alone cannot pick the credentials, and the row
    /// outlives the draft it describes.
    pub account_id: String,
    /// The folder it is in, as the wire name.
    pub folder: String,
    pub uid: Option<u32>,
    /// The `Message-ID` we wrote, which is how the copy is found when the UID is unknown.
    pub message_id: String,
    pub updated_at: i64,
}

/// A draft, as stored in `user.db`.
///
/// Attachments are **paths plus metadata**, never bytes: a draft with a 100 MB attachment must not
/// turn the user database into a second copy of the mail. `sha256` is what lets the send path notice
/// that a file changed under it (design/07 §5.3).
///
/// `deny_unknown_fields` matches `OutgoingMail`: the draft arrives as JSON from the renderer, and a
/// mistyped optional key would otherwise be dropped in silence (see `tools/payload_keys.mjs`).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Draft {
    pub id: String,
    pub account_id: String,
    pub to_addrs: String,
    pub cc_addrs: String,
    pub bcc_addrs: String,
    pub subject: String,
    pub body_text: String,
    pub body_html: Option<String>,
    /// JSON array of `{path,name,mime,size,sha256}`.
    pub attachments_json: String,
    pub in_reply_to: Option<String>,
    /// JSON array of message-ids.
    pub references: String,
    pub identity: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

impl Draft {
    /// A new, empty draft for an account. The id is the caller's (a UUID in the app).
    pub fn new(id: impl Into<String>, account_id: impl Into<String>, now: i64) -> Self {
        Self {
            id: id.into(),
            account_id: account_id.into(),
            to_addrs: String::new(),
            cc_addrs: String::new(),
            bcc_addrs: String::new(),
            subject: String::new(),
            body_text: String::new(),
            body_html: None,
            attachments_json: "[]".to_string(),
            in_reply_to: None,
            references: "[]".to_string(),
            identity: None,
            created_at: now,
            updated_at: now,
        }
    }
}

fn row_to_draft(row: &rusqlite::Row<'_>) -> rusqlite::Result<Draft> {
    Ok(Draft {
        id: row.get(0)?,
        account_id: row.get(1)?,
        to_addrs: row.get(2)?,
        cc_addrs: row.get(3)?,
        bcc_addrs: row.get(4)?,
        subject: row.get(5)?,
        body_text: row.get(6)?,
        body_html: row.get(7)?,
        attachments_json: row.get(8)?,
        in_reply_to: row.get(9)?,
        references: row.get(10)?,
        identity: row.get(11)?,
        created_at: row.get(12)?,
        updated_at: row.get(13)?,
    })
}

/// A queued send, as stored in `user.db`.
///
/// `payload_json` is the `OutgoingMail` the message was built from, and the spool file is the
/// message exactly as it will go on the wire. Both are kept: the spool is what gets sent (so a
/// retry cannot produce a different message than the one the user approved), and the payload is
/// what lets us rebuild the spool if that file is ever lost (`design/07` §5.3).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct OutboxItem {
    pub id: i64,
    pub account_id: String,
    pub draft_id: Option<String>,
    pub payload_json: String,
    /// Stored form. Read it through [`OutboxItem::parsed_state`] — never compare it as a string.
    pub state: String,
    pub attempts: u32,
    pub next_attempt_at: i64,
    pub last_error: Option<String>,
    pub undo_until: i64,
    pub scheduled_at: i64,
    pub spool_path: Option<String>,
    pub spool_bytes: i64,
    pub created_at: i64,
    pub sent_at: Option<i64>,
    pub sent_message_id: Option<String>,
}

impl OutboxItem {
    /// The state, with anything unreadable landing on `needs_attention` rather than `queued`.
    pub fn parsed_state(&self) -> OutboxState {
        OutboxState::parse_state(&self.state)
    }
}

// The pending-ops layer for `user.db`'s `pending_ops` table — the offline intent queue of
// `design/04` §5.5 / SYN-14 ("flags and deletions follow the server; local intents are compensated
// by an offline queue").
//
// The table has been in the schema since the beginning and **nothing ever wrote to it**: the queue
// was scaffolded as DDL and then left. Meanwhile `SyncStatus.pending_ops` reports a count of its
// rows, so the UI has been showing a permanent and honest 0 — the number was right, the feature was
// absent.
//
// The shape is the outbox's on purpose: the same states, the same claim-that-compares, the same
// backoff curve. Two queues with two vocabularies is how one of them ends up wrong.

/// What the user did while offline, to be replayed against the server.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PendingOpKind {
    MarkRead,
    MarkUnread,
    Move,
    Delete,
}

impl PendingOpKind {
    pub fn as_str(self) -> &'static str {
        match self {
            PendingOpKind::MarkRead => "mark_read",
            PendingOpKind::MarkUnread => "mark_unread",
            PendingOpKind::Move => "move",
            PendingOpKind::Delete => "delete",
        }
    }

    /// Anything unreadable becomes `None`.
    ///
    /// Deliberately **not** mapped onto one of the four: guessing would replay an intent the user
    /// never expressed, and dropping the row would lose one they did. The row stays, unread, and the
    /// replay pass reports it — the row is the user's data, and a version that cannot read it is a
    /// fact about this build, not a reason to throw their intent away.
    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "mark_read" => Some(PendingOpKind::MarkRead),
            "mark_unread" => Some(PendingOpKind::MarkUnread),
            "move" => Some(PendingOpKind::Move),
            "delete" => Some(PendingOpKind::Delete),
            _ => None,
        }
    }
}

/// The parameters of one pending op.
///
/// A struct rather than free-form JSON because the only variable part is a destination folder, and a
/// struct makes "unreadable" something the parser can say out loud instead of a panic in a worker.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PendingPayload {
    /// Where a `move` goes. `None` for everything else.
    #[serde(default)]
    pub target_folder: Option<String>,
}

/// One queued offline intent.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PendingOp {
    pub id: i64,
    pub account_id: String,
    pub folder: String,
    /// `None` for an op about a folder rather than a message.
    pub uid: Option<u64>,
    /// `None` when the stored kind is not one this build knows — see [`PendingOpKind::parse`].
    pub kind: Option<PendingOpKind>,
    pub payload_json: String,
    /// Stored form, exactly like the outbox's. Never compare it as a string.
    pub state: String,
    pub attempts: u32,
    pub next_attempt_at: i64,
    pub last_error: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

impl PendingOp {
    /// The payload, or `None` when the stored JSON is not one.
    pub fn payload(&self) -> Option<PendingPayload> {
        serde_json::from_str(&self.payload_json).ok()
    }
}

#[cfg(test)]
mod kind_tests {
    use super::*;

    #[test]
    fn a_kind_round_trips_and_unknown_values_stay_unknown() {
        for kind in [
            PendingOpKind::MarkRead,
            PendingOpKind::MarkUnread,
            PendingOpKind::Move,
            PendingOpKind::Delete,
        ] {
            assert_eq!(PendingOpKind::parse(kind.as_str()), Some(kind));
        }
        assert_eq!(PendingOpKind::parse("teleport"), None);
        assert_eq!(PendingOpKind::parse(""), None);
    }

    #[test]
    fn an_empty_payload_is_a_valid_payload() {
        // Rows written before a field existed read as `{}`; both shapes must be acceptable.
        assert_eq!(
            serde_json::from_str::<PendingPayload>("{}").ok(),
            Some(PendingPayload::default())
        );
        assert_eq!(
            serde_json::from_str::<PendingPayload>(r#"{"target_folder":"Archive"}"#)
                .ok()
                .and_then(|p| p.target_folder),
            Some("Archive".to_string())
        );
        assert!(serde_json::from_str::<PendingPayload>("not json").is_err());
    }
}

/// Offline intent queue (`design/04` §5.5 / SYN-14).
///
/// The table has existed since the first schema and nothing ever wrote to it; this is the half that
/// makes it real. The API mirrors the outbox's: claim-that-compares, one bounded pass, one backoff
/// curve shared with `next_attempt_delay_secs`.
impl UserStore {
    /// Queue an intent, or refresh it if the same one is already waiting.
    ///
    /// **Idempotent by design**: the same intent (account, folder, uid, kind, payload) queued twenty
    /// times while offline must stay one row — twenty replays of "mark this read" is twenty round
    /// trips for one user action, and on a metered connection that is the difference between a
    /// queue and a waste.
    ///
    /// The read-then-write is atomic **because this database has one writer thread** (M0-02): both
    /// statements run inside one `write_blocking` closure, so there is no interleaving to protect
    /// against. An earlier draft of this comment promised a partial unique index as a backstop and
    /// then never created it — a claim in a comment that no test or statement backs is the same lie
    /// as a green check that did not run, so the comment now says what is actually true: the
    /// single writer is the guarantee, and `uid`'s NULL being distinct in a unique index is exactly
    /// why an index would have been the weaker tool here anyway.
    ///
    /// Returns the row id (the existing one when the intent was already queued).
    pub fn enqueue_pending_op(
        &self,
        account_id: &str,
        folder: &str,
        uid: Option<u64>,
        kind: PendingOpKind,
        payload: &PendingPayload,
        now: i64,
    ) -> Result<i64> {
        let payload_json = serde_json::to_string(payload)
            .map_err(|e| crate::error::MailError::Cache(format!("序列化离线意图失败：{e}")))?;
        let account_id = account_id.to_string();
        let folder = folder.to_string();
        let uid = uid.map(|v| i64::try_from(v).unwrap_or(i64::MAX));
        self.db.write_blocking(move |conn| {
            // "Update the waiting row, else insert" — both inside the one writer closure, which is
            // what makes it atomic.
            let existing: Option<i64> = conn
                .query_row(
                    "select id from pending_ops
                      where account_id = ?1 and folder = ?2 and uid is ?3
                        and kind = ?4 and payload_json = ?5
                        and state in ('queued', 'in_flight')",
                    rusqlite::params![account_id, folder, uid, kind.as_str(), payload_json],
                    |row| row.get(0),
                )
                .optional()?;
            if let Some(id) = existing {
                // `updated_at` is ?2, not ?4: a SQL fragment with a gap in its numbering is a
                // query that needs four parameters and is handed two.
                conn.execute(
                    "update pending_ops set updated_at = ?2, next_attempt_at = 0,
                       attempts = 0, last_error = null
                      where id = ?1",
                    rusqlite::params![id, now],
                )?;
                return Ok(id);
            }
            conn.execute(
                "insert into pending_ops
                    (account_id, folder, uid, kind, payload_json, state,
                     attempts, next_attempt_at, created_at, updated_at)
                 values (?1, ?2, ?3, ?4, ?5, 'queued', 0, 0, ?6, ?6)",
                rusqlite::params![account_id, folder, uid, kind.as_str(), payload_json, now],
            )?;
            Ok(conn.last_insert_rowid())
        })
    }

    /// Rows whose turn it is: queued, and past their backoff.
    ///
    /// `in_flight` rows are **not** returned — a pass that sees them is the next pass, after a crash
    /// or a kill, and the startup path requeues them explicitly (the outbox does the same).
    pub fn due_pending_ops(&self, now: i64, limit: usize) -> Result<Vec<PendingOp>> {
        let guard = self.db.read_guard();
        let conn: &Connection = &guard;
        let mut stmt = conn.prepare(
            "select id, account_id, folder, uid, kind, payload_json, state, attempts,
                    next_attempt_at, last_error, created_at, updated_at
             from pending_ops
             where state = 'queued' and next_attempt_at <= ?1
             order by id asc
             limit ?2",
        )?;
        let rows = stmt.query_map(rusqlite::params![now, limit as i64], |row| {
            Ok(PendingOp {
                id: row.get(0)?,
                account_id: row.get(1)?,
                folder: row.get(2)?,
                uid: row.get::<_, Option<i64>>(3)?.map(|v| v as u64),
                kind: row
                    .get::<_, String>(4)
                    .ok()
                    .and_then(|raw| PendingOpKind::parse(&raw)),
                payload_json: row.get(5)?,
                state: row.get(6)?,
                attempts: row.get::<_, i64>(7)? as u32,
                next_attempt_at: row.get(8)?,
                last_error: row.get(9)?,
                created_at: row.get(10)?,
                updated_at: row.get(11)?,
            })
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    pub fn list_pending_ops(&self, account_id: Option<&str>) -> Result<Vec<PendingOp>> {
        let guard = self.db.read_guard();
        let conn: &Connection = &guard;
        // Both account and id ordering so a list a user is looking at does not jump around while
        // the queue is being worked.
        let sql = "select id, account_id, folder, uid, kind, payload_json, state, attempts,
                          next_attempt_at, last_error, created_at, updated_at
                   from pending_ops
                   where (?1 is null or account_id = ?1)
                     and state in ('queued', 'in_flight')
                   order by account_id, id asc";
        let mut stmt = conn.prepare(sql)?;
        let rows = stmt.query_map(rusqlite::params![account_id], |row| {
            Ok(PendingOp {
                id: row.get(0)?,
                account_id: row.get(1)?,
                folder: row.get(2)?,
                uid: row.get::<_, Option<i64>>(3)?.map(|v| v as u64),
                kind: row
                    .get::<_, String>(4)
                    .ok()
                    .and_then(|raw| PendingOpKind::parse(&raw)),
                payload_json: row.get(5)?,
                state: row.get(6)?,
                attempts: row.get::<_, i64>(7)? as u32,
                next_attempt_at: row.get(8)?,
                last_error: row.get(9)?,
                created_at: row.get(10)?,
                updated_at: row.get(11)?,
            })
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    /// Claim one row for this pass. `false` means somebody else has it.
    ///
    /// The compare-and-set is the whole point: a tick racing a manual "retry now", or two ticks after
    /// a wake-from-sleep, must not both apply the same intent.
    pub fn claim_pending_op(&self, id: i64, now: i64) -> Result<bool> {
        self.db.write_blocking(move |conn| {
            let changed = conn.execute(
                "update pending_ops set state = 'in_flight', updated_at = ?2
                  where id = ?1 and state = 'queued'",
                rusqlite::params![id, now],
            )?;
            Ok(changed > 0)
        })
    }

    /// The intent has been applied. The row stays as `done` on purpose: it is the audit trail that
    /// answers "what did the app do to my mailbox while I was offline".
    pub fn finish_pending_op(&self, id: i64, now: i64) -> Result<()> {
        self.db.write_blocking(move |conn| {
            conn.execute(
                "update pending_ops set state = 'done', updated_at = ?2, last_error = null
                  where id = ?1",
                rusqlite::params![id, now],
            )?;
            Ok(())
        })
    }

    /// Put a row back after a failed attempt, with the shared backoff curve.
    pub fn retry_pending_op(&self, id: i64, error: &str, now: i64) -> Result<()> {
        let error = error.to_string();
        self.db.write_blocking(move |conn| {
            let attempts: i64 = conn.query_row(
                "select attempts from pending_ops where id = ?1",
                [id],
                |r| r.get(0),
            )?;
            let delay = crate::outbox::next_attempt_delay_secs(attempts as u32 + 1);
            conn.execute(
                "update pending_ops
                    set state = 'queued', attempts = ?2, next_attempt_at = ?3,
                        last_error = ?4, updated_at = ?3
                  where id = ?1",
                rusqlite::params![id, attempts + 1, now + delay, error],
            )?;
            Ok(())
        })
    }

    /// Rows a previous run left mid-flight (crash between claim and outcome).
    ///
    /// They are requeued by the startup path rather than assumed done, exactly like the outbox: an
    /// intent that may or may not have been applied must be re-applied, because every one of these
    /// is idempotent on the server side (`STORE`/`MOVE`/`EXPUNGE` of the same message twice is the
    /// same state as once).
    pub fn requeue_pending_ops(&self, now: i64) -> Result<u32> {
        self.db.write_blocking(move |conn| {
            let changed = conn.execute(
                "update pending_ops set state = 'queued', updated_at = ?1
                  where state = 'in_flight'",
                rusqlite::params![now],
            )?;
            Ok(changed as u32)
        })
    }

    /// Cancel a queued intent (the user said never mind).
    pub fn cancel_pending_op(&self, id: i64) -> Result<bool> {
        self.db.write_blocking(move |conn| {
            let changed = conn.execute(
                "update pending_ops set state = 'cancelled'
                  where id = ?1 and state in ('queued', 'in_flight')",
                rusqlite::params![id],
            )?;
            Ok(changed > 0)
        })
    }

    pub fn pending_op(&self, id: i64) -> Result<Option<PendingOp>> {
        let guard = self.db.read_guard();
        let conn: &Connection = &guard;
        let row = conn
            .query_row(
                "select id, account_id, folder, uid, kind, payload_json, state, attempts,
                        next_attempt_at, last_error, created_at, updated_at
                 from pending_ops where id = ?1",
                rusqlite::params![id],
                |row| {
                    Ok(PendingOp {
                        id: row.get(0)?,
                        account_id: row.get(1)?,
                        folder: row.get(2)?,
                        uid: row.get::<_, Option<i64>>(3)?.map(|v| v as u64),
                        kind: row
                            .get::<_, String>(4)
                            .ok()
                            .and_then(|raw| PendingOpKind::parse(&raw)),
                        payload_json: row.get(5)?,
                        state: row.get(6)?,
                        attempts: row.get::<_, i64>(7)? as u32,
                        next_attempt_at: row.get(8)?,
                        last_error: row.get(9)?,
                        created_at: row.get(10)?,
                        updated_at: row.get(11)?,
                    })
                },
            )
            .optional()?;
        Ok(row)
    }
}

/// One account's quota state for the current window (`design/04` §5.7).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct QuotaLedger {
    pub account_id: String,
    /// Bytes downloaded in the current window.
    pub bytes_today: i64,
    /// The window's budget, or `None` when the provider has no documented limit.
    pub daily_budget: Option<i64>,
    /// When the current window started.
    pub window_start: i64,
    /// When the provider last told us to back off, if it did.
    pub throttled_until: Option<i64>,
    /// The oldest date this account can reach, once the server has made that clear.
    ///
    /// NetEase's "third-party clients may only read the last 30 days" shows up exactly here, and
    /// saying so is the difference between a policy and a bug the user blames on us.
    pub server_floor: Option<i64>,
}

impl QuotaLedger {
    /// Bytes left in this window, or `None` when there is no limit.
    pub fn remaining(&self) -> Option<i64> {
        self.daily_budget
            .map(|budget| (budget - self.bytes_today).max(0))
    }

    /// Whether the account may download right now.
    pub fn allows_download(&self, now: i64) -> bool {
        if self.throttled_until.is_some_and(|until| until > now) {
            return false;
        }
        self.remaining() != Some(0)
    }
}

fn row_to_quota(row: &rusqlite::Row<'_>) -> rusqlite::Result<QuotaLedger> {
    Ok(QuotaLedger {
        account_id: row.get(0)?,
        bytes_today: row.get(1)?,
        daily_budget: row.get(2)?,
        window_start: row.get(3)?,
        throttled_until: row.get(4)?,
        server_floor: row.get(5)?,
    })
}

/// A send being committed to the queue.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewOutbox {
    pub account_id: String,
    /// The draft this send came from, if any. Deleted once the message is on its way out.
    pub draft_id: Option<String>,
    pub payload_json: String,
    pub spool_path: Option<String>,
    pub spool_bytes: i64,
    /// Sends are eligible at `max(undo_until, scheduled_at, next_attempt_at)`; all three are NOT
    /// NULL so the due query can compare them directly (see the schema comment).
    pub undo_until: i64,
    pub scheduled_at: i64,
    pub created_at: i64,
}

const OUTBOX_COLUMNS: &str = "id, account_id, draft_id, payload_json, state, attempts, \
     next_attempt_at, last_error, undo_until, scheduled_at, spool_path, spool_bytes, created_at, \
     sent_at, sent_message_id";

fn row_to_outbox_item(row: &rusqlite::Row<'_>) -> rusqlite::Result<OutboxItem> {
    Ok(OutboxItem {
        id: row.get(0)?,
        account_id: row.get(1)?,
        draft_id: row.get(2)?,
        payload_json: row.get(3)?,
        state: row.get(4)?,
        attempts: row.get(5)?,
        next_attempt_at: row.get(6)?,
        last_error: row.get(7)?,
        undo_until: row.get(8)?,
        scheduled_at: row.get(9)?,
        spool_path: row.get(10)?,
        spool_bytes: row.get(11)?,
        created_at: row.get(12)?,
        sent_at: row.get(13)?,
        sent_message_id: row.get(14)?,
    })
}

/// Handle to `user.db`.
#[derive(Clone)]
pub struct UserStore {
    db: Db,
}

impl std::fmt::Debug for UserStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UserStore").finish()
    }
}

impl UserStore {
    /// Open (creating if needed) `user.db` at `path`.
    pub fn open(path: &std::path::Path) -> Result<Self> {
        let store = Self {
            db: Db::open(path, DbKind::User, DbOptions::for_kind(DbKind::User))?,
        };
        store.migrate()?;
        Ok(store)
    }

    /// Wrap an already-open handle (used when the app owns a [`crate::db::DbSet`]).
    pub fn from_db(db: Db) -> Result<Self> {
        let store = Self { db };
        store.migrate()?;
        Ok(store)
    }

    /// In-memory store, for tests.
    pub fn open_in_memory() -> Result<Self> {
        let store = Self {
            db: Db::open_in_memory("user")?,
        };
        store.migrate()?;
        Ok(store)
    }

    /// Underlying handle, for feature modules that need a query.
    pub fn db(&self) -> &Db {
        &self.db
    }

    /// Schema version recorded in the file.
    pub fn schema_version(&self) -> Result<i64> {
        // `read_guard` rather than `read_blocking`: it borrows a pooled connection instead of moving a
        // closure onto the writer thread, which keeps the synchronous API free of ownership churn.
        let guard = self.db.read_guard();
        let conn: &Connection = &guard;
        Ok(conn.query_row("pragma user_version", [], |row| row.get(0))?)
    }

    /// Whether a table exists — what the tests and the diagnostics bundle ask.
    pub fn has_table(&self, name: &str) -> Result<bool> {
        let guard = self.db.read_guard();
        let conn: &Connection = &guard;
        let found: Option<i64> = conn
            .query_row(
                "select 1 from sqlite_master where type = 'table' and name = ?1",
                [name],
                |row| row.get(0),
            )
            .optional()?;
        Ok(found.is_some())
    }

    /// Read a small value (window choice, last-used folder…), if set.
    pub fn kv_get(&self, key: &str) -> Result<Option<String>> {
        let guard = self.db.read_guard();
        let conn: &Connection = &guard;
        Ok(conn
            .query_row("select value from kv where key = ?1", [key], |row| {
                row.get(0)
            })
            .optional()?)
    }

    /// Write a small value.
    pub fn kv_set(&self, key: &str, value: &str) -> Result<()> {
        let key = key.to_string();
        let value = value.to_string();
        self.db.write_blocking(move |conn| {
            conn.execute(
                "insert into kv (key, value) values (?1, ?2)
                 on conflict(key) do update set value = excluded.value",
                rusqlite::params![key, value],
            )?;
            Ok(())
        })
    }

    // ------------------------------------------------------------------ drafts

    /// Insert or update a draft.
    ///
    /// Upsert, not insert: the composer saves on a timer and on every meaningful edit, and the same
    /// draft id arrives repeatedly. A duplicate-key error there would be a lost keystroke.
    pub fn save_draft(&self, draft: &Draft) -> Result<()> {
        let draft = draft.clone();
        self.db.write_blocking(move |conn| {
            conn.execute(
                "insert into drafts
                     (id, account_id, to_addrs, cc_addrs, bcc_addrs, subject, body_text, body_html,
                      attachments_json, in_reply_to, \"references\", identity, created_at, updated_at)
                 values (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)
                 on conflict(id) do update set
                     to_addrs = excluded.to_addrs,
                     cc_addrs = excluded.cc_addrs,
                     bcc_addrs = excluded.bcc_addrs,
                     subject = excluded.subject,
                     body_text = excluded.body_text,
                     body_html = excluded.body_html,
                     attachments_json = excluded.attachments_json,
                     in_reply_to = excluded.in_reply_to,
                     \"references\" = excluded.\"references\",
                     identity = excluded.identity,
                     updated_at = excluded.updated_at",
                rusqlite::params![
                    draft.id,
                    draft.account_id,
                    draft.to_addrs,
                    draft.cc_addrs,
                    draft.bcc_addrs,
                    draft.subject,
                    draft.body_text,
                    draft.body_html,
                    draft.attachments_json,
                    draft.in_reply_to,
                    draft.references,
                    draft.identity,
                    draft.created_at,
                    draft.updated_at,
                ],
            )?;
            Ok(())
        })
    }

    /// One draft, if it exists.
    pub fn load_draft(&self, id: &str) -> Result<Option<Draft>> {
        let guard = self.db.read_guard();
        let conn: &Connection = &guard;
        Ok(conn
            .query_row(
                "select id, account_id, to_addrs, cc_addrs, bcc_addrs, subject, body_text, body_html,
                        attachments_json, in_reply_to, \"references\", identity, created_at, updated_at
                 from drafts where id = ?1",
                [id],
                row_to_draft,
            )
            .optional()?)
    }

    /// Every draft for one account, most recently touched first — the order the "drafts (2)" list
    /// wants.
    pub fn list_drafts(&self, account_id: &str) -> Result<Vec<Draft>> {
        let guard = self.db.read_guard();
        let conn: &Connection = &guard;
        let mut stmt = conn.prepare(
            "select id, account_id, to_addrs, cc_addrs, bcc_addrs, subject, body_text, body_html,
                    attachments_json, in_reply_to, \"references\", identity, created_at, updated_at
             from drafts where account_id = ?1 order by updated_at desc, id",
        )?;
        let rows = stmt.query_map([account_id], row_to_draft)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Remove a draft. Permanent on purpose: this is "discard", and there is no undo for it in the UI.
    pub fn delete_draft(&self, id: &str) -> Result<bool> {
        let id = id.to_string();
        self.db.write_blocking(move |conn| {
            Ok(conn.execute("delete from drafts where id = ?1", [id])? > 0)
        })
    }

    // ------------------------------------------------------------------ OAuth settings

    /// What a signed-in OAuth account needs to renew its token.
    ///
    /// Stored in `kv` beside the other per-account settings rather than in `accounts.json`: that file
    /// is the *account list* (addresses, servers, display names) and is meant to be readable and
    /// copyable, while these two values are what the client registration consists of. A1-F32 is
    /// explicit that a desktop `client_secret` is not actually a secret — this is about keeping the
    /// account list clean, not about hiding the value.
    ///
    /// `client_secret` is optional because whether a provider demands one is exactly M0-8's open
    /// question.
    pub fn oauth_settings(&self, account_id: &str) -> Result<Option<OAuthSettings>> {
        let Some(client_id) = self.kv_get(&oauth_client_id_key(account_id))? else {
            return Ok(None);
        };
        if client_id.is_empty() {
            return Ok(None);
        }
        Ok(Some(OAuthSettings {
            client_id,
            client_secret: self
                .kv_get(&oauth_client_secret_key(account_id))?
                .filter(|value| !value.is_empty()),
        }))
    }

    /// Remember which client registration an account signs in with.
    pub fn set_oauth_settings(&self, account_id: &str, settings: &OAuthSettings) -> Result<()> {
        self.kv_set(&oauth_client_id_key(account_id), &settings.client_id)?;
        match settings.client_secret.as_deref() {
            // An empty value *removes* the secret: a blank box in the wizard means "this provider
            // does not need one", and keeping the old value would quietly keep using it.
            None => self.kv_set(&oauth_client_secret_key(account_id), ""),
            Some(secret) => self.kv_set(&oauth_client_secret_key(account_id), secret),
        }
    }
    // ------------------------------------------------------------------ notification preference

    /// Whether this account shows a system notification when new mail arrives.
    ///
    /// Stored in `kv` rather than as a new table: it is one bit per account, and `kv` is already the
    /// home of small user preferences (`design/03`). Default is on — a mail client that stays silent
    /// about new mail would be the surprise, not the other way round.
    pub fn notify_enabled(&self, account_id: &str) -> Result<bool> {
        Ok(self
            .kv_get(&notify_key(account_id))?
            .map(|raw| raw != "0")
            .unwrap_or(true))
    }

    /// Remember the user's choice for one account.
    pub fn set_notify_enabled(&self, account_id: &str, enabled: bool) -> Result<()> {
        self.kv_set(&notify_key(account_id), if enabled { "1" } else { "0" })
    }

    // ------------------------------------------------------------------ quota ledger

    /// The account's quota state, rolling the window over if a day has passed.
    ///
    /// The roll-over happens on read as well as on write so a ledger left over from yesterday cannot
    /// block today's first download — and so the UI can ask for it without causing a write.
    pub fn quota(&self, account_id: &str, now: i64, budget: Option<i64>) -> Result<QuotaLedger> {
        let account = account_id.to_string();
        self.db.write_blocking(move |conn| {
            let existing = conn
                .query_row(
                    "select account_id, bytes_today, daily_budget, window_start, throttled_until,
                            server_floor
                     from quota_ledger where account_id = ?1",
                    [&account],
                    row_to_quota,
                )
                .optional()?;

            match existing {
                Some(mut ledger) if now - ledger.window_start < QUOTA_WINDOW_SECS => {
                    // The budget can change under a stored ledger (`set_quota_budget`), so take the
                    // caller's value: the provider table is the source of truth, not this row.
                    if budget.is_some() {
                        ledger.daily_budget = budget;
                        conn.execute(
                            "update quota_ledger set daily_budget = ?2 where account_id = ?1",
                            rusqlite::params![account, budget],
                        )?;
                    }
                    Ok(ledger)
                }
                _ => {
                    // First use, or a new window: reset the bytes but keep the throttle and the
                    // floor, which are facts about the server rather than about this window.
                    let (throttled, floor) = existing
                        .map(|ledger| (ledger.throttled_until, ledger.server_floor))
                        .unwrap_or((None, None));
                    conn.execute(
                        "insert into quota_ledger
                             (account_id, bytes_today, daily_budget, window_start, throttled_until,
                              server_floor)
                         values (?1, 0, ?2, ?3, ?4, ?5)
                         on conflict(account_id) do update set
                             bytes_today = 0, daily_budget = excluded.daily_budget,
                             window_start = excluded.window_start",
                        rusqlite::params![account, budget, now, throttled, floor],
                    )?;
                    Ok(QuotaLedger {
                        account_id: account,
                        bytes_today: 0,
                        daily_budget: budget,
                        window_start: now,
                        throttled_until: throttled,
                        server_floor: floor,
                    })
                }
            }
        })
    }

    /// Debit the account for what a batch just downloaded.
    ///
    /// Returns the ledger as it stands afterwards, so the caller can decide to stop without another
    /// read. Called after every network batch — a path that skips it is a path that blows through
    /// the provider's limit, which is why the gate is asserted in tests.
    pub fn quota_add_bytes(
        &self,
        account_id: &str,
        bytes: i64,
        now: i64,
        budget: Option<i64>,
    ) -> Result<QuotaLedger> {
        if bytes > 0 {
            let account = account_id.to_string();
            self.db.write_blocking(move |conn| {
                conn.execute(
                    "insert into quota_ledger (account_id, bytes_today, window_start)
                     values (?1, ?2, ?3)
                     on conflict(account_id) do update set bytes_today = bytes_today + ?2",
                    rusqlite::params![account, bytes, now],
                )?;
                Ok(())
            })?;
        }
        self.quota(account_id, now, budget)
    }

    /// Record that the provider asked us to slow down, and until when.
    pub fn set_quota_throttled(&self, account_id: &str, until: i64) -> Result<()> {
        let account = account_id.to_string();
        self.db.write_blocking(move |conn| {
            conn.execute(
                "insert into quota_ledger (account_id, throttled_until) values (?1, ?2)
                 on conflict(account_id) do update set throttled_until = ?2",
                rusqlite::params![account, until],
            )?;
            Ok(())
        })
    }

    /// Record how far back the server is willing to go, once it has made that clear.
    pub fn set_quota_server_floor(&self, account_id: &str, floor: i64) -> Result<()> {
        let account = account_id.to_string();
        self.db.write_blocking(move |conn| {
            conn.execute(
                "insert into quota_ledger (account_id, server_floor) values (?1, ?2)
                 on conflict(account_id) do update set server_floor = ?2",
                rusqlite::params![account, floor],
            )?;
            Ok(())
        })
    }

    // ------------------------------------------------------------------ draft mirrors

    /// Where the server-side copy of this draft is, if we ever wrote one.
    pub fn draft_mirror(&self, draft_id: &str) -> Result<Option<DraftMirror>> {
        let guard = self.db.read_guard();
        let conn: &Connection = &guard;
        Ok(conn
            .query_row(
                "select draft_id, account_id, folder, uid, message_id, updated_at
                 from draft_mirrors where draft_id = ?1",
                [draft_id],
                |row| {
                    Ok(DraftMirror {
                        draft_id: row.get(0)?,
                        account_id: row.get(1)?,
                        folder: row.get(2)?,
                        uid: row.get::<_, Option<i64>>(3)?.map(|uid| uid as u32),
                        message_id: row.get(4)?,
                        updated_at: row.get(5)?,
                    })
                },
            )
            .optional()?)
    }

    /// Remember which server-side copy belongs to this draft.
    pub fn set_draft_mirror(&self, mirror: &DraftMirror) -> Result<()> {
        let mirror = mirror.clone();
        self.db.write_blocking(move |conn: &Connection| {
            conn.execute(
                "insert into draft_mirrors
                     (draft_id, account_id, folder, uid, message_id, updated_at)
                 values (?1, ?2, ?3, ?4, ?5, ?6)
                 on conflict(draft_id) do update set
                     account_id = excluded.account_id, folder = excluded.folder,
                     uid = excluded.uid, message_id = excluded.message_id,
                     updated_at = excluded.updated_at",
                rusqlite::params![
                    mirror.draft_id,
                    mirror.account_id,
                    mirror.folder,
                    mirror.uid.map(i64::from),
                    mirror.message_id,
                    mirror.updated_at
                ],
            )?;
            Ok(())
        })
    }

    /// Mirror rows whose draft no longer exists.
    ///
    /// The two ways that happens are both normal: the user discarded the draft, or the send path
    /// deleted it once the message went out. Either way the server-side copy we wrote is now the only
    /// thing left of a draft nobody has, and it is exactly what this list is for — a copy in the
    /// user's 草稿箱 that nothing in the UI can ever reach.
    pub fn orphaned_draft_mirrors(&self) -> Result<Vec<DraftMirror>> {
        let guard = self.db.read_guard();
        let conn: &Connection = &guard;
        let mut stmt = conn.prepare(
            "select m.draft_id, m.account_id, m.folder, m.uid, m.message_id, m.updated_at
             from draft_mirrors m
             left join drafts d on d.id = m.draft_id
             where d.id is null
             order by m.updated_at asc",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(DraftMirror {
                draft_id: row.get(0)?,
                account_id: row.get(1)?,
                folder: row.get(2)?,
                uid: row.get::<_, Option<i64>>(3)?.map(|uid| uid as u32),
                message_id: row.get(4)?,
                updated_at: row.get(5)?,
            })
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }
    /// Forget the mirror row, and report what it said.
    ///
    /// Returns the row so the caller can delete the server-side copy: dropping the record without
    /// removing the copy would leave one in the user's 草稿箱 that nothing can ever clean up.
    pub fn take_draft_mirror(&self, draft_id: &str) -> Result<Option<DraftMirror>> {
        let existing = self.draft_mirror(draft_id)?;
        if existing.is_some() {
            let id = draft_id.to_string();
            self.db.write_blocking(move |conn: &Connection| {
                conn.execute("delete from draft_mirrors where draft_id = ?1", [id])?;
                Ok(())
            })?;
        }
        Ok(existing)
    }

    // ------------------------------------------------------------------ outbox

    /// Commit a send to the queue, and report the row id it got.
    ///
    /// The caller has already built the spool: `design/07` §5.3 freezes the bytes at enqueue time so
    /// a retry, a crash or a slow network cannot change what goes out.
    pub fn enqueue_outbox(&self, new: &NewOutbox) -> Result<i64> {
        let new = new.clone();
        self.db.write_blocking(move |conn| {
            conn.execute(
                "insert into outbox
                     (account_id, draft_id, payload_json, state, attempts, next_attempt_at,
                      undo_until, scheduled_at, spool_path, spool_bytes, created_at)
                 values (?1, ?2, ?3, 'queued', 0, 0, ?4, ?5, ?6, ?7, ?8)",
                rusqlite::params![
                    new.account_id,
                    new.draft_id,
                    new.payload_json,
                    new.undo_until,
                    new.scheduled_at,
                    new.spool_path,
                    new.spool_bytes,
                    new.created_at,
                ],
            )?;
            Ok(conn.last_insert_rowid())
        })
    }

    /// One queued send.
    pub fn outbox_item(&self, id: i64) -> Result<Option<OutboxItem>> {
        let guard = self.db.read_guard();
        let conn: &Connection = &guard;
        Ok(conn
            .query_row(
                &format!("select {OUTBOX_COLUMNS} from outbox where id = ?1"),
                [id],
                row_to_outbox_item,
            )
            .optional()?)
    }

    /// Everything in the outbox for one account, or for all of them, newest first.
    ///
    /// Every state, not just the open ones: the UI also shows sent and cancelled items, which is how
    /// a user can tell that a message actually left.
    pub fn list_outbox(&self, account_id: Option<&str>) -> Result<Vec<OutboxItem>> {
        let guard = self.db.read_guard();
        let conn: &Connection = &guard;
        let mut items = Vec::new();
        match account_id {
            Some(id) => {
                let mut stmt = conn.prepare(&format!(
                    "select {OUTBOX_COLUMNS} from outbox where account_id = ?1
                     order by created_at desc, id desc"
                ))?;
                let rows = stmt.query_map([id], row_to_outbox_item)?;
                for row in rows {
                    items.push(row?);
                }
            }
            None => {
                let mut stmt = conn.prepare(&format!(
                    "select {OUTBOX_COLUMNS} from outbox order by created_at desc, id desc"
                ))?;
                let rows = stmt.query_map([], row_to_outbox_item)?;
                for row in rows {
                    items.push(row?);
                }
            }
        }
        Ok(items)
    }

    /// The sends whose turn has come.
    ///
    /// `queued` is the only state the worker may pick up, and it means "waiting for its turn" — the
    /// undo window, a scheduled time, or a backoff delay, all expressed in the three time columns.
    /// `failed` and `needs_attention` are terminal and stay out of here on purpose: a permanent
    /// failure that stayed eligible would be retried forever.
    ///
    /// The three time columns are compared directly rather than through `is null or`: they are NOT
    /// NULL by schema precisely because `null <= now` is NULL in SQL, which would make a row
    /// unreachable forever (`design/07` §5.3).
    pub fn due_outbox(&self, now: i64, limit: usize) -> Result<Vec<OutboxItem>> {
        let guard = self.db.read_guard();
        let conn: &Connection = &guard;
        let mut stmt = conn.prepare(&format!(
            "select {OUTBOX_COLUMNS} from outbox
             where state = 'queued'
               and undo_until <= ?1 and scheduled_at <= ?1 and next_attempt_at <= ?1
             order by created_at asc, id asc
             limit ?2"
        ))?;
        let rows = stmt.query_map(rusqlite::params![now, limit as i64], row_to_outbox_item)?;
        let mut items = Vec::new();
        for row in rows {
            items.push(row?);
        }
        Ok(items)
    }

    /// Rows a previous run left mid-flight.
    ///
    /// A crash between "we are sending" and "we recorded the outcome" leaves these, and they are the
    /// reason startup checks the Sent folder before requeueing (`design/07` §5.3.1): the message may
    /// well have been delivered.
    pub fn outbox_in_flight(&self) -> Result<Vec<OutboxItem>> {
        let guard = self.db.read_guard();
        let conn: &Connection = &guard;
        let mut stmt = conn.prepare(&format!(
            "select {OUTBOX_COLUMNS} from outbox where state = 'sending' order by created_at asc"
        ))?;
        let rows = stmt.query_map([], row_to_outbox_item)?;
        let mut items = Vec::new();
        for row in rows {
            items.push(row?);
        }
        Ok(items)
    }

    /// Claim a row for the worker. Only a row that is still due can be claimed, so two ticks — or
    /// two windows — cannot both send the same message.
    pub fn claim_outbox(&self, id: i64, now: i64) -> Result<bool> {
        self.db.write_blocking(move |conn| {
            let changed = conn.execute(
                "update outbox set state = 'sending' where id = ?1
                   and state = 'queued'
                   and undo_until <= ?2 and scheduled_at <= ?2 and next_attempt_at <= ?2",
                rusqlite::params![id, now],
            )?;
            Ok(changed > 0)
        })
    }

    /// Remember where the frozen bytes went.
    ///
    /// Only used when the worker had to rebuild a spool file that had gone missing: the path is
    /// otherwise written once, at enqueue time, together with the bytes.
    pub fn set_outbox_spool(&self, id: i64, path: &str, bytes: i64) -> Result<()> {
        let path = path.to_string();
        self.db.write_blocking(move |conn| {
            conn.execute(
                "update outbox set spool_path = ?2, spool_bytes = ?3 where id = ?1",
                rusqlite::params![id, path, bytes],
            )?;
            Ok(())
        })
    }

    /// Sends that are delivered but whose Sent copy has not been filed yet.
    ///
    /// The spool file **is** the marker: a successful send keeps it, and the filing pass removes it
    /// once the copy is in the folder. Which makes "the bytes are still here" and "this has not been
    /// archived yet" the same statement — so a crash in between needs no extra bookkeeping, the next
    /// pass simply does it. Searching before appending is what keeps that from producing two copies
    /// (`design/07` §5.4).
    pub fn sent_awaiting_copy(&self) -> Result<Vec<OutboxItem>> {
        let guard = self.db.read_guard();
        let conn: &Connection = &guard;
        let mut stmt = conn.prepare(&format!(
            "select {OUTBOX_COLUMNS} from outbox
             where state = 'sent' and spool_path is not null
             order by sent_at asc, id asc"
        ))?;
        let rows = stmt.query_map([], row_to_outbox_item)?;
        let mut items = Vec::new();
        for row in rows {
            items.push(row?);
        }
        Ok(items)
    }

    /// Forget a row's spool file: either it has been filed, or the bytes are gone for good and
    /// retrying forever would be a loop with nothing in it.
    pub fn clear_outbox_spool(&self, id: i64) -> Result<()> {
        self.db.write_blocking(move |conn| {
            conn.execute(
                "update outbox set spool_path = null, spool_bytes = 0 where id = ?1",
                [id],
            )?;
            Ok(())
        })
    }

    /// The message is on its way out.
    pub fn mark_outbox_sent(&self, id: i64, message_id: Option<&str>, now: i64) -> Result<()> {
        let message_id = message_id.map(str::to_string);
        self.db.write_blocking(move |conn| {
            conn.execute(
                "update outbox set state = 'sent', sent_at = ?2, sent_message_id = ?3,
                     last_error = null
                 where id = ?1",
                rusqlite::params![id, now, message_id],
            )?;
            Ok(())
        })
    }

    /// The attempt failed in a way worth trying again: bump the count and say when.
    ///
    /// Back to `queued`, not `failed`: `due_outbox` only picks up rows that are waiting, and a
    /// retryable failure is exactly that — waiting, with `next_attempt_at` saying until when.
    pub fn mark_outbox_retry(&self, id: i64, error: &str, next_attempt_at: i64) -> Result<()> {
        let error = error.to_string();
        self.db.write_blocking(move |conn| {
            conn.execute(
                "update outbox set state = 'queued', attempts = attempts + 1,
                     last_error = ?2, next_attempt_at = ?3
                 where id = ?1",
                rusqlite::params![id, error, next_attempt_at],
            )?;
            Ok(())
        })
    }

    /// Take the row out of the worker's hands for good.
    ///
    /// Used for both terminal outcomes: a permanent refusal, and the "a human has to decide" cases
    /// from [`crate::outbox::needs_user_decision`]. They differ only in what the UI offers next, so
    /// they differ only in the stored state.
    pub fn mark_outbox_failed(&self, id: i64, reason: &str, needs_attention: bool) -> Result<()> {
        let reason = reason.to_string();
        let state = if needs_attention {
            "needs_attention"
        } else {
            "failed"
        };
        self.db.write_blocking(move |conn| {
            conn.execute(
                "update outbox set state = ?2, last_error = ?3, attempts = attempts + 1
                 where id = ?1",
                rusqlite::params![id, state, reason],
            )?;
            Ok(())
        })
    }

    /// Cancel a send. Only before the worker claims it: once a row is `sending` the message is on the
    /// wire, and pretending otherwise would be the "撤销" lie `design/07` §5.2 warns about.
    pub fn cancel_outbox(&self, id: i64) -> Result<bool> {
        self.db.write_blocking(move |conn| {
            let changed = conn.execute(
                "update outbox set state = 'cancelled' where id = ?1 and state = 'queued'",
                [id],
            )?;
            Ok(changed > 0)
        })
    }

    /// Put a cancelled or failed message back in the queue, ready to go out now.
    pub fn requeue_outbox(&self, id: i64) -> Result<bool> {
        self.db.write_blocking(move |conn| {
            let changed = conn.execute(
                "update outbox set state = 'queued', next_attempt_at = 0, scheduled_at = 0,
                     undo_until = 0, last_error = null
                 where id = ?1 and state in ('cancelled', 'failed', 'needs_attention')",
                [id],
            )?;
            Ok(changed > 0)
        })
    }

    /// Put a row a crash left in `sending` back in the queue.
    ///
    /// Deliberately separate from [`UserStore::requeue_outbox`]: that one is the user pressing 重试
    /// on a row the worker has finished with, and it refuses to touch a row that might be mid-send.
    /// This one is only ever called by crash recovery, which knows no send is running.
    pub fn requeue_in_flight(&self, id: i64) -> Result<bool> {
        self.db.write_blocking(move |conn| {
            let changed = conn.execute(
                "update outbox set state = 'queued', next_attempt_at = 0, scheduled_at = 0,
                     undo_until = 0
                 where id = ?1 and state = 'sending'",
                [id],
            )?;
            Ok(changed > 0)
        })
    }

    /// Remove a row for good.
    pub fn delete_outbox(&self, id: i64) -> Result<bool> {
        self.db.write_blocking(move |conn| {
            Ok(conn.execute("delete from outbox where id = ?1", [id])? > 0)
        })
    }

    /// The sync window the user chose, or the default when they never chose one.
    ///
    /// Stored in `user.db` rather than the cache on purpose: it is a preference, and re-downloading a
    /// mailbox because a cache file was deleted is exactly the surprise this split exists to avoid.
    pub fn sync_window(&self) -> Result<SyncWindow> {
        Ok(self
            .kv_get(SYNC_WINDOW_KEY)?
            .map(|raw| SyncWindow::from_key(&raw))
            .unwrap_or_default())
    }

    /// Remember the user's choice.
    pub fn set_sync_window(&self, window: SyncWindow) -> Result<()> {
        self.kv_set(SYNC_WINDOW_KEY, &window.as_key())
    }

    /// How many drafts are stored. Used by the UI's "drafts (2)" badge and by tests.
    pub fn draft_count(&self) -> Result<u32> {
        self.count("drafts")
    }

    /// How many sends are still waiting. Used by the UI's "发件箱 (2)" badge.
    ///
    /// Open states only: the table keeps sent and cancelled rows too (they are the history the user
    /// looks at), and counting those would leave the badge showing a number forever.
    pub fn outbox_count(&self) -> Result<u32> {
        let guard = self.db.read_guard();
        let conn: &Connection = &guard;
        let n: i64 = conn.query_row(
            "select count(*) from outbox where state in ('queued', 'sending')",
            [],
            |row| row.get(0),
        )?;
        Ok(n as u32)
    }

    /// How many offline operations are queued.
    /// How much offline work is left: queued and in-flight, **not** done or cancelled.
    ///
    /// This used to be a raw row count, which is the sort of number that only ever goes up — a
    /// status bar showing "3 个待处理" after everything has been sent is worse than showing nothing,
    /// because the user learns to stop reading it.
    pub fn pending_ops_outstanding(&self) -> Result<u32> {
        let guard = self.db.read_guard();
        let conn: &Connection = &guard;
        Ok(conn.query_row(
            "select count(*) from pending_ops where state in ('queued', 'in_flight')",
            [],
            |row| row.get::<_, i64>(0).map(|n| n as u32),
        )?)
    }

    fn count(&self, table: &str) -> Result<u32> {
        // The table name is **not** user input: it comes from this module's own call sites above, and
        // the assertion keeps that true if someone later passes one through.
        debug_assert!(
            USER_TABLES.contains(&table),
            "count() only takes a table this module owns"
        );
        let guard = self.db.read_guard();
        let conn: &Connection = &guard;
        let n: i64 = conn.query_row(&format!("select count(*) from {table}"), [], |row| {
            row.get(0)
        })?;
        Ok(n as u32)
    }

    /// Create the schema. All of it or none of it: the version number must describe a whole schema.
    ///
    /// A file written by a **newer** version is refused rather than opened. `create table if not
    /// exists` would leave its tables alone, so the danger is not the schema — it is the meaning of
    /// what is already in it: a column that changed shape, a state name that now means something
    /// else, a draft this version would rewrite in the old form. Refusing costs the user a message
    /// ("升级程序"); misreading costs them their drafts. `cache.db` has the same guard for the same
    /// reason, and it is worth having here even though these tables are additive today.
    fn migrate(&self) -> Result<()> {
        self.db.write_blocking(|conn: &Connection| {
            let version: i64 = conn.query_row("pragma user_version", [], |row| row.get(0))?;
            if version > USER_SCHEMA_VERSION {
                return Err(crate::error::MailError::Cache(format!(
                    "用户数据库版本 {version} 比程序支持的 {USER_SCHEMA_VERSION} 更新，请升级程序后再打开"
                )));
            }

            conn.execute_batch(
                "create table if not exists schema_migrations (
                     version    integer primary key,
                     applied_at integer not null
                 );

                 -- Small key/value state that belongs to the user, not the cache: the sync window
                 -- they chose, the folder they last looked at, and so on.
                 create table if not exists kv (
                     key   text primary key,
                     value text not null
                 );

                 -- Drafts are user data. `attachments_json` holds [{path,name,mime,size,sha256}]
                 -- and never the bytes: a draft with a 100 MB attachment must not bloat the database.
                 create table if not exists drafts (
                     id            text primary key,
                     account_id    text not null,
                     to_addrs      text not null default '',
                     cc_addrs      text not null default '',
                     bcc_addrs     text not null default '',
                     subject       text not null default '',
                     body_text     text not null default '',
                     body_html     text,
                     attachments_json text not null default '[]',
                     in_reply_to   text,
                     \"references\"  text not null default '[]',
                     identity      text,
                     created_at    integer not null,
                     updated_at    integer not null
                 );
                 create index if not exists drafts_by_account on drafts(account_id, updated_at desc);

                 -- The outbox: sends that have been committed to but not yet delivered. `undo_until`,
                 -- `scheduled_at` and `next_attempt_at` are NOT NULL on purpose — `null <= now` is
                 -- NULL in SQL, so a nullable column here makes the due-query skip rows forever.
                 create table if not exists outbox (
                     id              integer primary key autoincrement,
                     account_id      text not null,
                     draft_id        text,
                     payload_json    text not null,
                     state           text not null default 'queued',
                     attempts        integer not null default 0,
                     next_attempt_at integer not null default 0,
                     last_error      text,
                     undo_until      integer not null default 0,
                     scheduled_at    integer not null default 0,
                     spool_path      text,
                     spool_bytes     integer not null default 0,
                     created_at      integer not null,
                     sent_at         integer,
                     sent_message_id text
                 );
                 create index if not exists outbox_pending on outbox(state, next_attempt_at);

                 -- Offline actions taken against the *server* (mark read, move, delete, label).
                 create table if not exists pending_ops (
                     id              integer primary key autoincrement,
                     account_id      text not null,
                     folder          text not null,
                     uid             integer,
                     kind            text not null,
                     payload_json    text not null,
                     state           text not null default 'queued',
                     attempts        integer not null default 0,
                     next_attempt_at integer not null default 0,
                     last_error      text,
                     created_at      integer not null,
                     updated_at      integer not null
                 );
                 create index if not exists pending_ops_queue on pending_ops(state, next_attempt_at);

                 -- The daily download quota ledger (design/04 §5.7). It lives here, in the file that
                 -- is never deleted, on purpose: a ledger in the deletable cache would forget how
                 -- much was downloaded today, and the next run would happily blow through the
                 -- provider's limit and get the account throttled.
                 create table if not exists quota_ledger (
                     account_id      text primary key,
                     bytes_today     integer not null default 0,
                     daily_budget    integer,
                     window_start    integer not null default 0,
                     throttled_until integer,
                     server_floor    integer
                 );

                 -- Where the copy of a draft that *we* wrote to the server's Drafts folder lives
                 -- (design/07 §3/§5.1). It has to be remembered for one reason: mirroring the same
                 -- draft again has to delete the previous copy, or every autosave would leave another
                 -- one behind in the user's 草稿箱. A table rather than new columns on `drafts`, so
                 -- that adding it stays additive for files that already exist.
                 create table if not exists draft_mirrors (
                     draft_id   text primary key,
                     account_id text not null default '',
                     folder     text not null,
                     uid        integer,
                     message_id text not null,
                     updated_at integer not null
                 );",
            )?;

            // `draft_mirrors` gained `account_id` after the table had already shipped in a release.
            // `create table if not exists` cannot add a column: an existing file keeps the shape it
            // was created with, and every query that names `account_id` fails with
            // "no such column: account_id" — which is what a **real profile** did, found by the
            // mirror probe (M1a-59). The tests had never seen it because they all start from a fresh
            // schema, which is exactly the blind spot this kind of migration exists to cover.
            add_column_if_missing(conn, "draft_mirrors", "account_id", "text not null default ''")?;

            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);
            conn.execute(
                "insert or ignore into schema_migrations (version, applied_at) values (?1, ?2)",
                rusqlite::params![USER_SCHEMA_VERSION, now],
            )?;
            conn.pragma_update(None, "user_version", USER_SCHEMA_VERSION)?;
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "easy-email-userdb-{}-{}-{}.sqlite",
            std::process::id(),
            name,
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ))
    }

    #[test]
    fn the_schema_comes_up_with_every_table_it_promises() {
        let store = UserStore::open_in_memory().expect("open");
        for table in USER_TABLES {
            assert!(
                store.has_table(table).expect("has_table"),
                "{table} must exist: it is part of the user database's contract"
            );
        }
        assert_eq!(
            store.schema_version().expect("version"),
            USER_SCHEMA_VERSION
        );
    }

    /// The property the whole split exists for: user data survives on its own file, with the
    /// durability setting that matches what it holds.
    #[test]
    fn user_db_is_its_own_file_with_full_durability() {
        let path = temp_path("separate");
        let store = UserStore::open(&path).expect("open");
        store.kv_set("sync_window", "years:2").expect("kv set");
        assert_eq!(
            store.kv_get("sync_window").expect("kv get").as_deref(),
            Some("years:2")
        );

        let synchronous: i64 = {
            let guard = store.db().read_guard();
            let conn: &Connection = &guard;
            conn.query_row("pragma synchronous", [], |row| row.get(0))
                .expect("pragma")
        };
        assert_eq!(
            synchronous, 2,
            "a draft or a queued send must survive a power cut, so user.db is FULL (2) while the \
             cache stays NORMAL (1)"
        );
        drop(store);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn the_sync_window_defaults_to_two_years_and_is_remembered() {
        let path = temp_path("window");
        let store = UserStore::open(&path).expect("open");
        assert_eq!(
            store.sync_window().expect("window"),
            SyncWindow::default(),
            "a fresh install must not download fifteen years of mail"
        );

        store
            .set_sync_window(SyncWindow::Years(5))
            .expect("set window");
        assert_eq!(store.sync_window().expect("window"), SyncWindow::Years(5));
        drop(store);

        // And it survives a restart, because it lives in the user database.
        let again = UserStore::open(&path).expect("reopen");
        assert_eq!(again.sync_window().expect("window"), SyncWindow::Years(5));
        drop(again);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn kv_set_overwrites_and_the_counts_start_at_zero() {
        let store = UserStore::open_in_memory().expect("open");
        store.kv_set("a", "1").expect("set");
        store.kv_set("a", "2").expect("overwrite");
        assert_eq!(store.kv_get("a").expect("get").as_deref(), Some("2"));
        assert_eq!(store.kv_get("missing").expect("get"), None);

        assert_eq!(store.draft_count().expect("drafts"), 0);
        assert_eq!(store.outbox_count().expect("outbox"), 0);
        assert_eq!(store.pending_ops_outstanding().expect("ops"), 0);
    }

    /// Re-opening must be a no-op, not a second migration: a schema version that changes on every
    /// start would make "which version is this file" meaningless.
    #[test]
    fn migrating_twice_changes_nothing() {
        let path = temp_path("idempotent");
        let store = UserStore::open(&path).expect("open");
        store.kv_set("keep", "me").expect("kv");
        drop(store);

        let again = UserStore::open(&path).expect("reopen");
        assert_eq!(
            again.schema_version().expect("version"),
            USER_SCHEMA_VERSION
        );
        assert_eq!(
            again.kv_get("keep").expect("kv").as_deref(),
            Some("me"),
            "re-opening must not wipe user data"
        );
        drop(again);
        let _ = std::fs::remove_file(&path);
    }
    #[test]
    fn a_user_database_from_a_newer_version_is_refused_rather_than_misread() {
        // The file's *meaning* is what is at stake, not its schema: `create table if not exists`
        // would leave a future shape alone and this version would then write today's shape into it.
        let path = temp_path("newer");
        {
            let store = UserStore::open(&path).expect("open");
            assert_eq!(
                store.schema_version().expect("version"),
                USER_SCHEMA_VERSION
            );
        }
        {
            let db = Db::open(&path, DbKind::User, DbOptions::for_kind(DbKind::User)).expect("db");
            db.write_blocking(|conn: &Connection| {
                conn.pragma_update(None, "user_version", USER_SCHEMA_VERSION + 1)?;
                Ok(())
            })
            .expect("bump");
        }

        let error = UserStore::open(&path).expect_err("a newer file must not be opened");
        assert!(
            error.to_string().contains("更新"),
            "the message has to say what to do: {error}"
        );
        let _ = std::fs::remove_file(&path);
    }

    // ------------------------------------------------------------------- outbox

    fn queued_send(account: &str, at: i64) -> NewOutbox {
        NewOutbox {
            account_id: account.to_string(),
            draft_id: None,
            payload_json: r#"{"to":["a@example.com"],"subject":"s"}"#.to_string(),
            spool_path: None,
            spool_bytes: 0,
            undo_until: 0,
            scheduled_at: 0,
            created_at: at,
        }
    }

    #[test]
    fn the_outbox_badge_counts_only_what_is_still_open() {
        // The table keeps sent and cancelled rows as history. Counting them would leave a badge on
        // screen that never goes away, which is how a user learns to ignore the badge.
        let store = UserStore::open_in_memory().expect("open");
        let first = store
            .enqueue_outbox(&queued_send("acc", 1))
            .expect("enqueue");
        let second = store
            .enqueue_outbox(&queued_send("acc", 2))
            .expect("enqueue");
        assert_eq!(store.outbox_count().expect("count"), 2);

        store
            .mark_outbox_sent(first, Some("<id@example.com>"), 9)
            .expect("sent");
        assert_eq!(store.outbox_count().expect("count"), 1);

        // In flight still counts: the user is waiting for it.
        assert!(store.claim_outbox(second, 0).expect("claim"));
        assert_eq!(store.outbox_count().expect("count"), 1);
        assert!(
            store.due_outbox(9_999, 10).expect("due").is_empty(),
            "an in-flight row is not due, whatever the clock says"
        );

        assert!(!store.cancel_outbox(second).expect("cancel"), "too late");
        assert_eq!(
            store.outbox_count().expect("count"),
            1,
            "a message already on the wire stays open"
        );
    }

    #[test]
    fn a_retry_goes_back_to_queued_but_a_permanent_failure_does_not() {
        // The difference between "wait and try again" and "stop, and tell the user". Getting it
        // wrong in one direction loops forever; in the other it drops somebody's mail.
        let store = UserStore::open_in_memory().expect("open");
        let retrying = store
            .enqueue_outbox(&queued_send("acc", 1))
            .expect("enqueue");
        let dead = store
            .enqueue_outbox(&queued_send("acc", 2))
            .expect("enqueue");
        // Which rows are eligible right now, by id: the other row in this fixture is queued and due
        // as well, so a bare count would pass for the wrong reason.
        let due = |now: i64| -> Vec<i64> {
            store
                .due_outbox(now, 10)
                .expect("due")
                .into_iter()
                .map(|item| item.id)
                .collect()
        };

        assert!(store.claim_outbox(retrying, 10).expect("claim"));
        store
            .mark_outbox_retry(retrying, "421 try later", 40)
            .expect("retry");
        let item = store.outbox_item(retrying).expect("item").expect("present");
        assert_eq!(item.parsed_state(), OutboxState::Queued);
        assert_eq!(item.attempts, 1);
        assert!(!due(39).contains(&retrying), "the backoff has not elapsed");
        assert!(due(40).contains(&retrying));

        assert!(store.claim_outbox(dead, 10).expect("claim"));
        store
            .mark_outbox_failed(dead, "550 no such user", false)
            .expect("failed");
        assert!(
            !due(i64::MAX / 2).contains(&dead),
            "a permanent failure must never become eligible again"
        );
        assert!(
            store.requeue_outbox(dead).expect("requeue"),
            "…unless a human asks for it"
        );
        assert!(due(20).contains(&dead));
    }

    #[test]
    fn a_send_is_not_due_until_its_undo_window_has_passed() {
        let store = UserStore::open_in_memory().expect("open");
        let mut item = queued_send("acc", 1_000);
        item.undo_until = 1_005;
        let id = store.enqueue_outbox(&item).expect("enqueue");

        assert!(store.due_outbox(1_004, 10).expect("due").is_empty());
        assert!(
            !store.claim_outbox(id, 1_004).expect("claim"),
            "claiming inside the undo window would be the 'not really cancellable' lie"
        );
        assert_eq!(store.due_outbox(1_005, 10).expect("due").len(), 1);
        assert!(store.claim_outbox(id, 1_005).expect("claim"));
    }

    #[test]
    fn the_quota_ledger_accumulates_and_rolls_over_a_day() {
        let store = UserStore::open_in_memory().expect("open");
        let budget = Some(1_000);

        let first = store.quota("acc", 1_000, budget).expect("quota");
        assert_eq!(first.bytes_today, 0);
        assert_eq!(first.remaining(), Some(1_000));
        assert!(first.allows_download(1_000));

        let after = store
            .quota_add_bytes("acc", 400, 1_100, budget)
            .expect("add");
        assert_eq!(after.bytes_today, 400);
        assert_eq!(after.remaining(), Some(600));
        assert_eq!(
            store
                .quota("acc", 1_200, budget)
                .expect("quota")
                .bytes_today,
            400,
            "reading must not reset the window"
        );

        // Exact exhaustion stops downloads, which is the whole point: waiting to be told means the
        // account is already throttled.
        let spent = store
            .quota_add_bytes("acc", 600, 1_300, budget)
            .expect("add");
        assert_eq!(spent.remaining(), Some(0));
        assert!(!spent.allows_download(1_300));

        // A new window forgets the bytes...
        let next = store
            .quota("acc", 1_000 + QUOTA_WINDOW_SECS, budget)
            .expect("quota");
        assert_eq!(next.bytes_today, 0);
        assert!(next.allows_download(1_000 + QUOTA_WINDOW_SECS));
        // ...but not what the account has learned about the server.
        store.set_quota_server_floor("acc", 500).expect("floor");
        store.set_quota_throttled("acc", 999_999).expect("throttle");
        let carried = store
            .quota("acc", 1_000 + QUOTA_WINDOW_SECS * 2, budget)
            .expect("quota");
        assert_eq!(carried.server_floor, Some(500));
        assert_eq!(carried.throttled_until, Some(999_999));
        assert!(
            !carried.allows_download(1_000 + QUOTA_WINDOW_SECS * 2),
            "a throttle outlives the window that learned about it"
        );
        assert!(
            carried.allows_download(999_999),
            "and stops mattering when it expires"
        );
    }

    #[test]
    fn a_provider_without_a_documented_limit_is_not_throttled_by_us() {
        // A made-up budget would stop somebody's mail for no reason, so it has to be possible to say
        // "no limit" and have that mean exactly that.
        let store = UserStore::open_in_memory().expect("open");
        store
            .quota_add_bytes("acc", 10_000_000, 100, None)
            .expect("add");
        let ledger = store.quota("acc", 100, None).expect("quota");
        assert_eq!(ledger.daily_budget, None);
        assert_eq!(ledger.remaining(), None);
        assert!(ledger.allows_download(100));
    }

    #[test]
    fn the_quota_ledger_outlives_a_restart_and_a_cache_wipe() {
        // The design puts this in `user.db` for one reason: a ledger in the deletable cache would
        // forget today's usage, and the next run would march straight into the provider's limit.
        let path = temp_path("quota");
        let budget = Some(1_000);

        {
            let store = UserStore::open(&path).expect("open");
            store.quota_add_bytes("acc", 700, 100, budget).expect("add");
        }
        let reopened = UserStore::open(&path).expect("reopen");
        assert_eq!(
            reopened
                .quota("acc", 200, budget)
                .expect("quota")
                .bytes_today,
            700,
            "a restart must not hand back a fresh budget"
        );
        assert!(
            reopened
                .quota("acc", 200, budget)
                .expect("quota")
                .allows_download(200),
            "300 bytes are still left, so the account may still sync"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn an_older_user_database_gains_the_ledger_on_open() {
        // v1 files are already on real machines. `migrate` runs `create table if not exists` for
        // everything it owns, so an additive table arrives on the next open — this test is what keeps
        // somebody from "optimising" that into a version check that refuses to open old files.
        let path = temp_path("upgrade");
        {
            let store = UserStore::open(&path).expect("open");
            assert_eq!(
                store.schema_version().expect("version"),
                USER_SCHEMA_VERSION
            );
        }

        // Drop the table and the recorded version to simulate a file written by v1.
        {
            let db = Db::open(&path, DbKind::User, DbOptions::for_kind(DbKind::User)).expect("db");
            db.write_blocking(|conn: &Connection| {
                conn.execute_batch("drop table quota_ledger; pragma user_version = 1;")?;
                Ok(())
            })
            .expect("downgrade");
        }

        let upgraded = UserStore::open(&path).expect("reopen");
        assert!(
            upgraded.has_table("quota_ledger").expect("has_table"),
            "the additive table must arrive on open"
        );
        assert_eq!(
            upgraded.schema_version().expect("version"),
            USER_SCHEMA_VERSION
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn oauth_settings_round_trip_and_a_blank_secret_removes_it() {
        let store = UserStore::open_in_memory().expect("open");
        assert_eq!(
            store.oauth_settings("me@example.com").expect("read"),
            None,
            "an account with no registration is a password account"
        );

        store
            .set_oauth_settings(
                "me@example.com",
                &OAuthSettings {
                    client_id: "client-123".into(),
                    client_secret: Some("shh".into()),
                },
            )
            .expect("write");
        assert_eq!(
            store.oauth_settings("me@example.com").expect("read"),
            Some(OAuthSettings {
                client_id: "client-123".into(),
                client_secret: Some("shh".into())
            })
        );

        // A blank box in the wizard means "this provider does not require a secret"; keeping the old
        // value would quietly keep sending it.
        store
            .set_oauth_settings(
                "me@example.com",
                &OAuthSettings {
                    client_id: "client-456".into(),
                    client_secret: None,
                },
            )
            .expect("write");
        assert_eq!(
            store.oauth_settings("me@example.com").expect("read"),
            Some(OAuthSettings {
                client_id: "client-456".into(),
                client_secret: None
            })
        );

        assert_eq!(
            store.oauth_settings("other@example.com").expect("read"),
            None,
            "settings are per account"
        );
    }

    #[test]
    fn notifications_default_to_on_and_remember_being_turned_off() {
        // Default on: a mail client that stays silent about new mail is the surprise. And the choice
        // is per account — a noisy work mailbox and a quiet personal one are a normal pair.
        let store = UserStore::open_in_memory().expect("open");
        assert!(store.notify_enabled("work").expect("read"));

        store.set_notify_enabled("work", false).expect("set");
        assert!(!store.notify_enabled("work").expect("read"));
        assert!(
            store.notify_enabled("personal").expect("read"),
            "silencing one account must not silence the others"
        );

        store.set_notify_enabled("work", true).expect("set");
        assert!(store.notify_enabled("work").expect("read"));
    }
    #[test]
    fn a_mirror_without_its_draft_is_reported_as_orphaned() {
        // Both ways it happens are normal — the user discarded the draft, or the send path deleted it
        // once the message went out — and in both cases the server-side copy we wrote is now litter
        // that nothing in the UI can reach.
        let store = UserStore::open_in_memory().expect("open");
        let mut draft = Draft::new("d1", "acc", 1);
        draft.subject = "草稿".into();
        store.save_draft(&draft).expect("save");

        for id in ["d1", "d2"] {
            store
                .set_draft_mirror(&DraftMirror {
                    draft_id: id.into(),
                    account_id: "acc".into(),
                    folder: "Drafts".into(),
                    uid: Some(7),
                    message_id: format!("<{id}@x>"),
                    updated_at: 1,
                })
                .expect("mirror");
        }

        let orphans = store.orphaned_draft_mirrors().expect("orphans");
        assert_eq!(
            orphans
                .iter()
                .map(|mirror| mirror.draft_id.as_str())
                .collect::<Vec<_>>(),
            vec!["d2"],
            "a draft that still exists is not an orphan"
        );

        // And the row goes when the sweep has dealt with it.
        let taken = store
            .take_draft_mirror("d2")
            .expect("take")
            .expect("present");
        assert_eq!(taken.message_id, "<d2@x>");
        assert!(store.orphaned_draft_mirrors().expect("orphans").is_empty());
    }

    // ------------------------------------------------------------------- drafts

    fn draft(id: &str, account: &str, now: i64) -> Draft {
        Draft::new(id, account, now)
    }

    /// A file created before `draft_mirrors` had `account_id` must gain it on open.
    ///
    /// This is the one that was **missing in a real release**: the tests all start from a fresh
    /// schema, so nothing noticed that an existing profile kept the old table and every mirror query
    /// failed with "no such column: account_id". The mirror had therefore never worked on any profile
    /// older than the column — reported on a real machine by `tools/probe-draft-mirror.mjs`.
    #[test]
    fn an_older_profile_gains_the_mirror_account_column_on_open() {
        let path = std::env::temp_dir().join(format!(
            "easy-email-mirror-migration-{}-{}.sqlite",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));

        // A v2 file: `draft_mirrors` without `account_id`, plus a row already in it.
        {
            let conn = rusqlite::Connection::open(&path).expect("legacy open");
            conn.execute_batch(
                "pragma user_version = 2;
                 create table draft_mirrors (
                     draft_id   text primary key,
                     folder     text not null,
                     uid        integer,
                     message_id text not null,
                     updated_at integer not null
                 );
                 insert into draft_mirrors (draft_id, folder, uid, message_id, updated_at)
                 values ('d1', 'Drafts', 7, '<m@x>', 1);",
            )
            .expect("legacy schema");
        }

        let store = UserStore::open(&path).expect("opening must migrate, not refuse");
        assert_eq!(
            store.schema_version().expect("version"),
            USER_SCHEMA_VERSION,
            "the version has to move, or the next open repeats this"
        );
        let mirror = store
            .draft_mirror("d1")
            .expect("the column must be readable now")
            .expect("and the pre-existing row must still be there");
        assert_eq!(
            mirror.account_id, "",
            "a row from before the column has no account, which is the honest value"
        );
        assert_eq!(mirror.folder, "Drafts");
        drop(store);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_mistyped_optional_key_is_refused_rather_than_dropped() {
        // `body_html` is optional, so a payload that sends `bodyHtml` would deserialise into a
        // draft with no HTML at all — a silent downgrade of what the user wrote. The key set is
        // compared against the frontend's builders by tools/check.ps1; this keeps the runtime
        // honest for callers the gate does not see.
        let payload = serde_json::json!({
            "id": "d1", "account_id": "acc",
            "to_addrs": "", "cc_addrs": "", "bcc_addrs": "",
            "subject": "s", "body_text": "b", "bodyHtml": "<p>b</p>",
            "attachments_json": "[]", "in_reply_to": null, "references": "[]",
            "identity": null, "created_at": 1, "updated_at": 1
        });
        let parsed: std::result::Result<Draft, serde_json::Error> = serde_json::from_value(payload);
        assert!(
            parsed.is_err(),
            "an unknown key must be an error, not a silently ignored field"
        );
    }

    #[test]
    fn the_payload_the_composer_sends_deserialises() {
        // Mirrors src/payload.js -> draft(). Kept next to the struct so the shape the renderer
        // really sends is pinned on this side too, not only in the frontend's own gate.
        let payload = serde_json::json!({
            "id": "d1", "account_id": "acc",
            "to_addrs": "a@example.com", "cc_addrs": "", "bcc_addrs": "",
            "subject": "十月报销", "body_text": "见附件", "body_html": null,
            "attachments_json": "[]", "in_reply_to": null, "references": "[]",
            "identity": null, "created_at": 1, "updated_at": 2
        });
        let draft: Draft = serde_json::from_value(payload).expect("the composer's payload");
        assert_eq!(draft.subject, "十月报销");
        assert_eq!(draft.body_html, None);
    }

    #[test]
    fn a_draft_round_trips_with_every_field() {
        let store = UserStore::open_in_memory().expect("open");
        let mut d = draft("d1", "acc", 100);
        d.subject = "十月报销".to_string();
        d.body_text = "见附件".to_string();
        d.to_addrs = "a@example.com".to_string();
        d.attachments_json = r#"[{"path":"C:\x.pdf","name":"x.pdf","mime":"application/pdf","size":10,"sha256":"ab"}]"#.to_string();
        d.references = r#"["<m1@example.com>"]"#.to_string();
        store.save_draft(&d).expect("save");

        let loaded = store.load_draft("d1").expect("load").expect("present");
        assert_eq!(
            loaded, d,
            "every field must survive, including the JSON columns"
        );
        assert_eq!(store.draft_count().expect("count"), 1);
    }

    #[test]
    fn saving_the_same_draft_twice_updates_it_instead_of_failing() {
        // The composer autosaves; a duplicate-key error here would be a lost keystroke.
        let store = UserStore::open_in_memory().expect("open");
        let mut d = draft("d1", "acc", 100);
        d.subject = "first".to_string();
        store.save_draft(&d).expect("save");

        d.subject = "second".to_string();
        d.updated_at = 200;
        store.save_draft(&d).expect("save again");

        assert_eq!(store.draft_count().expect("count"), 1, "still one row");
        let loaded = store.load_draft("d1").expect("load").expect("present");
        assert_eq!(loaded.subject, "second");
        assert_eq!(loaded.created_at, 100, "the original creation time is kept");
        assert_eq!(loaded.updated_at, 200);
    }

    #[test]
    fn drafts_are_listed_newest_first_and_scoped_to_their_account() {
        let store = UserStore::open_in_memory().expect("open");
        for (id, account, at) in [("a", "acc", 10), ("b", "acc", 30), ("c", "other", 99)] {
            store.save_draft(&draft(id, account, at)).expect("save");
        }

        let mine: Vec<String> = store
            .list_drafts("acc")
            .expect("list")
            .into_iter()
            .map(|d| d.id)
            .collect();
        assert_eq!(
            mine,
            vec!["b", "a"],
            "most recently touched first, and never another account's drafts"
        );
    }

    #[test]
    fn deleting_a_draft_is_permanent_and_reports_whether_it_existed() {
        let store = UserStore::open_in_memory().expect("open");
        store.save_draft(&draft("d1", "acc", 1)).expect("save");

        assert!(store.delete_draft("d1").expect("delete"));
        assert_eq!(store.load_draft("d1").expect("load"), None);
        assert!(
            !store.delete_draft("d1").expect("delete again"),
            "a second delete must say 'nothing was there', not pretend to work"
        );
    }

    #[test]
    fn drafts_outlive_a_restart() {
        // The whole reason drafts are not in the deletable cache: the app closing, or the machine
        // restarting, must not lose what somebody was writing.
        let path = temp_path("drafts");
        {
            let store = UserStore::open(&path).expect("open");
            let mut d = draft("d1", "acc", 1);
            d.body_text = "半句话".to_string();
            store.save_draft(&d).expect("save");
        }
        let again = UserStore::open(&path).expect("reopen");
        assert_eq!(
            again
                .load_draft("d1")
                .expect("load")
                .expect("present")
                .body_text,
            "半句话"
        );
        drop(again);
        let _ = std::fs::remove_file(&path);
    }
    // ------------------------------------------------------------ pending ops

    fn store() -> UserStore {
        UserStore::open_in_memory().expect("store")
    }

    #[test]
    fn a_pending_op_survives_the_round_trip() {
        let store = store();
        let id = store
            .enqueue_pending_op(
                "acc",
                "INBOX",
                Some(42),
                PendingOpKind::Move,
                &PendingPayload {
                    target_folder: Some("Archive".to_string()),
                },
                1_000,
            )
            .expect("enqueue");
        let row = store.pending_op(id).expect("read").expect("row");
        assert_eq!(row.account_id, "acc");
        assert_eq!(row.folder, "INBOX");
        assert_eq!(row.uid, Some(42));
        assert_eq!(row.kind, Some(PendingOpKind::Move));
        assert_eq!(
            row.payload().and_then(|p| p.target_folder),
            Some("Archive".to_string()),
            "the destination must survive, or the replay would move mail nowhere"
        );
        assert_eq!(row.state, "queued");
    }

    #[test]
    fn re_enqueueing_the_same_intent_keeps_one_row() {
        // Twenty clicks on "mark read" while offline is one intent, not twenty: each replay costs a
        // round trip on a metered connection, and the queue exists to save work, not to create it.
        let store = store();
        let mut ids = Vec::new();
        for _ in 0..20 {
            ids.push(
                store
                    .enqueue_pending_op(
                        "acc",
                        "INBOX",
                        Some(7),
                        PendingOpKind::MarkRead,
                        &PendingPayload::default(),
                        1_000,
                    )
                    .expect("enqueue"),
            );
        }
        assert!(
            ids.windows(2).all(|w| w[0] == w[1]),
            "the same intent must keep its row (and its id)"
        );
        assert_eq!(store.list_pending_ops(None).expect("list").len(), 1);
        assert_eq!(store.pending_ops_outstanding().expect("count"), 1);

        // A different destination is a different intent, so a second move does get its own row.
        store
            .enqueue_pending_op(
                "acc",
                "INBOX",
                Some(7),
                PendingOpKind::Move,
                &PendingPayload {
                    target_folder: Some("Other".to_string()),
                },
                1_000,
            )
            .expect("enqueue move");
        assert_eq!(store.list_pending_ops(None).expect("list").len(), 2);
    }

    #[test]
    fn a_claimed_op_is_not_handed_to_the_next_pass() {
        let store = store();
        let id = store
            .enqueue_pending_op(
                "acc",
                "INBOX",
                Some(1),
                PendingOpKind::MarkRead,
                &PendingPayload::default(),
                1,
            )
            .expect("enqueue");

        assert!(store.claim_pending_op(id, 10).expect("first claim"));
        assert!(
            !store.claim_pending_op(id, 11).expect("second claim"),
            "the compare-and-set is what stops two ticks applying one intent"
        );
        assert!(
            store.due_pending_ops(12, 10).expect("due").is_empty(),
            "an in-flight row is not due again — that is the same claim, one layer up"
        );
        store.finish_pending_op(id, 20).expect("finish");
        assert_eq!(store.pending_ops_outstanding().expect("count"), 0);
    }

    #[test]
    fn a_failed_op_comes_back_later_and_not_immediately() {
        let store = store();
        let id = store
            .enqueue_pending_op(
                "acc",
                "INBOX",
                Some(1),
                PendingOpKind::Delete,
                &PendingPayload::default(),
                1,
            )
            .expect("enqueue");
        store.claim_pending_op(id, 2).expect("claim");
        store
            .retry_pending_op(id, "connection refused", 2)
            .expect("retry");

        let row = store.pending_op(id).expect("read").expect("row");
        assert_eq!(
            row.attempts, 1,
            "the attempt is counted, for the backoff curve"
        );
        assert!(row.next_attempt_at > 2, "the next try is in the future");
        assert_eq!(row.last_error.as_deref(), Some("connection refused"));
        assert!(
            store.due_pending_ops(2, 10).expect("not yet").is_empty(),
            "immediately retrying is what got the server annoyed in the first place"
        );
        let delay = crate::outbox::next_attempt_delay_secs(1);
        assert_eq!(
            store.due_pending_ops(2 + delay, 10).expect("due now").len(),
            1
        );
    }

    #[test]
    fn an_unknown_kind_is_preserved_not_fatal() {
        // A row written by a newer version: the intent is real, this build just cannot name it. It
        // stays visible with `kind: None`, and the replay pass reports it rather than guessing.
        let store = store();
        store
            .db
            .write_blocking(move |conn| {
                conn.execute(
                    "insert into pending_ops
                        (account_id, folder, uid, kind, payload_json, state, created_at, updated_at)
                     values ('acc', 'INBOX', 5, 'teleport', '{}', 'queued', 1, 1)",
                    [],
                )?;
                Ok(())
            })
            .expect("plant a row from the future");
        let rows = store.list_pending_ops(None).expect("list");
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].kind, None,
            "an unreadable kind reads as None, not as a guess"
        );
        assert_eq!(
            rows[0].uid,
            Some(5),
            "the rest of the row is still there to look at"
        );
    }

    #[test]
    fn unreadable_payload_is_reported_not_dropped() {
        let store = store();
        let id = store
            .enqueue_pending_op(
                "acc",
                "INBOX",
                Some(5),
                PendingOpKind::MarkRead,
                &PendingPayload::default(),
                1,
            )
            .expect("enqueue");
        store
            .db
            .write_blocking(move |conn| {
                conn.execute(
                    "update pending_ops set payload_json = 'not json' where id = ?1",
                    [id],
                )?;
                Ok(())
            })
            .expect("corrupt the payload");
        let row = store.pending_op(id).expect("read").expect("row");
        assert!(
            row.payload().is_none(),
            "a bad payload is a None, not a panic"
        );
    }

    #[test]
    fn done_ops_do_not_block_the_same_key_again() {
        // The dedupe only covers waiting rows: an intent that was applied must be queueable again,
        // or the second "mark read" a month later would silently do nothing.
        let store = store();
        let id = store
            .enqueue_pending_op(
                "acc",
                "INBOX",
                Some(9),
                PendingOpKind::MarkRead,
                &PendingPayload::default(),
                1,
            )
            .expect("enqueue");
        store.claim_pending_op(id, 2).expect("claim");
        store.finish_pending_op(id, 3).expect("finish");

        let again = store
            .enqueue_pending_op(
                "acc",
                "INBOX",
                Some(9),
                PendingOpKind::MarkRead,
                &PendingPayload::default(),
                100,
            )
            .expect("enqueue again");
        assert_ne!(again, id, "a finished row is history; this is a new intent");
        assert_eq!(store.pending_ops_outstanding().expect("count"), 1);
    }

    #[test]
    fn the_count_means_work_left_not_history() {
        let store = store();
        for kind in [PendingOpKind::MarkRead, PendingOpKind::MarkUnread] {
            store
                .enqueue_pending_op("acc", "INBOX", Some(3), kind, &PendingPayload::default(), 1)
                .expect("enqueue");
        }
        assert_eq!(store.pending_ops_outstanding().expect("count"), 2);
        let done = store
            .enqueue_pending_op(
                "acc",
                "Sent",
                None,
                PendingOpKind::MarkRead,
                &PendingPayload::default(),
                1,
            )
            .expect("enqueue");
        store.claim_pending_op(done, 2).expect("claim");
        store.finish_pending_op(done, 3).expect("finish");
        assert_eq!(
            store.pending_ops_outstanding().expect("count"),
            2,
            "a finished intent is not work; counting it would make the status number only rise"
        );
        let cancelled = store
            .enqueue_pending_op(
                "acc",
                "Sent",
                None,
                PendingOpKind::Delete,
                &PendingPayload::default(),
                1,
            )
            .expect("enqueue");
        assert!(store.cancel_pending_op(cancelled).expect("cancel"));
        assert_eq!(store.pending_ops_outstanding().expect("count"), 2);
        assert!(
            !store.cancel_pending_op(cancelled).expect("cancel twice"),
            "cancelling twice is a no-op, not a second state change"
        );
    }

    #[test]
    fn a_crash_leaves_rows_requeueable_rather_than_stuck() {
        // The outbox requeues in-flight rows at startup for the same reason: an intent that may have
        // been applied is re-applied, and every one of these is idempotent on the server.
        let store = store();
        let id = store
            .enqueue_pending_op(
                "acc",
                "INBOX",
                Some(1),
                PendingOpKind::MarkRead,
                &PendingPayload::default(),
                1,
            )
            .expect("enqueue");
        store.claim_pending_op(id, 2).expect("claim");
        assert_eq!(store.requeue_pending_ops(50).expect("requeue"), 1);
        assert_eq!(store.due_pending_ops(51, 10).expect("due").len(), 1);
    }
}
