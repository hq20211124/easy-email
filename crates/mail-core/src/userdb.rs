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
use crate::sync::SyncWindow;

/// Schema version of `user.db`. Bump when the statements in [`UserStore::migrate`] change.
pub const USER_SCHEMA_VERSION: i64 = 1;

/// `kv` key holding the chosen sync window.
pub const SYNC_WINDOW_KEY: &str = "sync_window";

/// Tables this database owns, in the order the diagnostics screen lists them.
pub const USER_TABLES: &[&str] = &["schema_migrations", "kv", "drafts", "outbox", "pending_ops"];

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

    /// How many sends are waiting in the outbox.
    pub fn outbox_count(&self) -> Result<u32> {
        self.count("outbox")
    }

    /// How many offline operations are queued.
    pub fn pending_op_count(&self) -> Result<u32> {
        self.count("pending_ops")
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
    fn migrate(&self) -> Result<()> {
        self.db.write_blocking(|conn: &Connection| {
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
                 create index if not exists pending_ops_queue on pending_ops(state, next_attempt_at);",
            )?;

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
        assert_eq!(store.pending_op_count().expect("ops"), 0);
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
}
