# PLAN —— 历史邮件回填 + 离线提示

**执行者注意**：本文件是机械指令，不是建议。每一步都给了**完整的替换前/替换后代码**，
照着改即可，不需要再设计。改完按**第 11 节**逐条验收。执行完**删掉本文件**。

> 写这份计划时已经用脚本核对过：**31 个「替换前」锚点在当前仓库里存在且唯一**，
> **12 个插入位置锚点也各只出现一次**，并且把 31 处改动套用到一份临时副本上
> `node --check` 通过、id 检查通过。所以**如果某个锚点找不到，说明文件已经变了 ——
> 停下来核对，不要凭感觉改**。

---

## 0. 范围

只做 HANDOFF §7 ① 剩下的那一小块：

1. **历史回填**：缓存只有最新 500 封，"加载更多"越过这 500 封之后，向后（更早）取邮件。
   每次 200 封，**绝不移动 `last_uid` 游标**（回填与增量同步是两个方向）。
2. **离线提示**：界面开始使用已经存在但从未被调用的 `sync_status` 命令，
   离线时明确显示"下面是本地缓存 + 原因"，而不是把陈旧数据当新鲜的展示。

**明确不做**（不要顺手做，HANDOFF 已否决或超出范围）：
OAuth2、自动发现（ISPDB/SRV/Autodiscover）、多文件夹推送、附件大小上限、POP3、
正文流式下载、把前端换成有打包器的工程、`cargo tauri build` 打安装包。

**已知副作用（不要"修"）**：回填进来的旧邮件里未读的会算进未读数，
所以托盘 tooltip / 推送通知里的"未读 N"会随着回填变大。这是**正确**的 ——
`unread_count` 的定义一直是"缓存里所有未读"，回填只是让它覆盖得更全。

---

## 1. 改动清单

| # | 文件 | 动作 |
|---|---|---|
| 1 | `crates/mail-core/src/cache.rs` | `SCHEMA_VERSION` 1→2；`migrate()` 拆成 v1 建表 + v2 加列，新增 `has_column()`；`invalidate_folder()` 清 `history_complete`；新增 `oldest_uid()` / `history_complete()` / `mark_history_complete()`；+4 个单测 |
| 2 | `crates/mail-core/src/imap.rs` | 新增 `OlderPage` 结构体 + `ImapClient::fetch_older_headers()`；**顺带修掉增量同步截断丢邮件的真 bug**（`keep_oldest_page`，见 §5.3）+1 个单测 |
| 3 | `crates/mail-core/src/backfill.rs` | **新文件**：`BACKFILL_BATCH` / `BackfillOutcome` / `BackfillDriver` + 6 个单测 |
| 4 | `crates/mail-core/src/lib.rs` | 挂上 `pub mod backfill` + 重导出 |
| 5 | `crates/mail-core/src/sync.rs` | 只改两处文档注释，**不动任何逻辑** |
| 6 | `src-tauri/src/lib.rs` | `MAX_LIST_ROWS`；`list_messages` 上限 500→1000；新增 `cached_page()` 辅助函数；新增 `load_more_messages` 命令 + `OlderMessages` 结构体；注册进 `generate_handler!`；import 只加 `BackfillDriver` |
| 7 | `src/index.html` | 新增 `#offline-note`、`#list-note` |
| 8 | `src/style.css` | 新增 `.offline-note` / `.list-note` |
| 9 | `src/app.js` | `MAX_LIST_ROWS`；`state.historyDone`；`loadMore()`；`renderListFooter()`；`refreshSyncStatus()`；4 处既有函数微调（`selectMailbox` / `loadMessages` / `openHit` / `refreshAccounts`） |
| 10 | `tools/check.ps1` | 新增一项门禁：回填不得碰游标 + 分页上限两侧一致 |
| 11 | `crates/mail-core/tests/loopback.rs` | 夹具支持 `history` 前置旧邮件；+2 个端到端回填用例（一页到底 / 连翻三页）；import 加 `BackfillDriver`、`BACKFILL_BATCH` |
| 12 | `README.md` / `HANDOFF.md` | 见**第 10 节**（**必须按实测结果写，不许照抄本节的乐观说法**） |

---

## 2. `crates/mail-core/src/cache.rs`

### 2.1 `SCHEMA_VERSION`

**替换前**
```rust
/// Bumped whenever the schema below changes; [`Cache::migrate`] reacts to it.
pub const SCHEMA_VERSION: i64 = 1;
```

**替换后**
```rust
/// Bumped whenever the schema below changes; [`Cache::migrate`] reacts to it.
///
/// * **1** — folders / messages / bodies / the FTS5 index.
/// * **2** — `folders.history_complete`, so a backfill that reached the bottom of a folder is not
///   repeated on every "load more" (see [`crate::backfill`]).
pub const SCHEMA_VERSION: i64 = 2;
```

### 2.2 `migrate()` —— 整体替换

**替换前**（现有 `fn migrate`，第 133~210 行整段）

**替换后**
```rust
    fn migrate(&self) -> Result<()> {
        // WAL keeps a reader (the UI) from blocking the sync writer. `synchronous = NORMAL` is the
        // usual pairing: a crash can lose the last transaction but cannot corrupt the database,
        // and this data is a cache — it can always be re-fetched.
        self.conn.execute_batch(
            "pragma journal_mode = wal;
             pragma synchronous = normal;
             pragma busy_timeout = 5000;",
        )?;

        let version: i64 = self
            .conn
            .query_row("pragma user_version", [], |row| row.get(0))?;
        if version == SCHEMA_VERSION {
            return Ok(());
        }

        // A database from the future is the only thing worth refusing: every older version has a
        // migration below it.
        if version > SCHEMA_VERSION {
            return Err(MailError::Cache(format!(
                "缓存数据库版本 {version} 比程序支持的 {SCHEMA_VERSION} 更新，请升级程序"
            )));
        }

        // v0 → v1. Everything is `if not exists`, so a database left half-created by a crash is
        // simply completed.
        if version < 1 {
            self.conn.execute_batch(
                "create table if not exists folders (
                     account_id       text not null,
                     name             text not null,
                     display_name     text not null,
                     delimiter        text,
                     selectable       integer not null default 1,
                     uidvalidity      integer not null default 0,
                     last_uid         integer not null default 0,
                     synced_at        integer,
                     history_complete integer not null default 0,
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
        }

        // v1 → v2. A database written by an earlier build already has the table, so the column is
        // added rather than the table rebuilt: the cached mail is the user's, and a migration that
        // threw it away to add one integer would be indistinguishable from data loss.
        if version < 2 && !self.has_column("folders", "history_complete")? {
            self.conn.execute_batch(
                "alter table folders add column history_complete integer not null default 0;",
            )?;
        }

        self.conn
            .pragma_update(None, "user_version", SCHEMA_VERSION)?;
        Ok(())
    }

    /// Whether `table` already has `column`.
    ///
    /// SQLite's `alter table` has no `if not exists` form, so this is what makes the migration
    /// idempotent: a run interrupted between the `alter` and the `user_version` bump would
    /// otherwise fail with "duplicate column name" on every start, leaving the app permanently
    /// unable to open its own cache.
    fn has_column(&self, table: &str, column: &str) -> Result<bool> {
        let mut stmt = self.conn.prepare(&format!("pragma table_info({table})"))?;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            let name: String = row.get(1)?;
            if name == column {
                return Ok(true);
            }
        }
        Ok(false)
    }
```

> 注意：`table` 只由本文件里的常量传入，不做拼接转义 —— 不要改成接受外部输入。

### 2.3 `invalidate_folder()` —— 两处独立改动

#### 2.3a 函数文档

**替换前**
```rust
    /// The stored `uidvalidity` is zeroed as well, which is what tells the sync driver that the
    /// next pass must start from a fresh window rather than trusting a cursor it no longer has.
    pub fn invalidate_folder(&self, account_id: &str, folder: &str) -> Result<()> {
```
**替换后**
```rust
    /// The stored `uidvalidity` is zeroed as well, which is what tells the sync driver that the
    /// next pass must start from a fresh window rather than trusting a cursor it no longer has.
    ///
    /// `history_complete` goes back to 0 for the same reason: it is a claim about *these* UIDs, and
    /// after a renumbering it describes nothing. Keeping it would make the folder permanently
    /// un-backfillable — the UI would never offer to load older mail again.
    pub fn invalidate_folder(&self, account_id: &str, folder: &str) -> Result<()> {
```

#### 2.3b 函数体末尾那条 `update`（**不是** `set_cursor` 里那条，别改错）

**替换前**
```rust
        self.conn.execute(
            "update folders set uidvalidity = 0, last_uid = 0 where account_id = ?1 and name = ?2",
            params![account_id, folder],
        )?;
```
**替换后**
```rust
        self.conn.execute(
            "update folders set uidvalidity = 0, last_uid = 0, history_complete = 0
             where account_id = ?1 and name = ?2",
            params![account_id, folder],
        )?;
```

> **不要动 `upsert_folder`。** 它的 `on conflict ... do update set` 列表里**不能**出现
> `history_complete`：每次增量同步都会 upsert 文件夹行，如果那一列被写进去，
> 已经回填到底的文件夹会每同步一次就忘一次。第 2.6 节有一个测试专门钉这个。

### 2.4 新增三个方法

**插在**下面这一行之前（它是 `invalidate_folder` 的文档首行）：
```rust
    /// Drop everything cached for one folder and reset its cursor.
```

```rust
    /// UID of the oldest message cached for a folder, i.e. where a history backfill resumes.
    ///
    /// Read from the rows rather than stored: the oldest row *is* the answer, and a stored copy
    /// would go stale the moment that message is deleted or moved.
    pub fn oldest_uid(&self, account_id: &str, folder: &str) -> Result<Option<u32>> {
        let oldest: Option<i64> = self.conn.query_row(
            "select min(uid) from messages where account_id = ?1 and folder = ?2",
            params![account_id, folder],
            |row| row.get(0),
        )?;
        Ok(oldest.map(|uid| uid as u32))
    }

    /// Whether the bottom of the folder has already been reached, i.e. whether a backfill has
    /// nothing older left to ask for.
    ///
    /// Deliberately *not* named "everything is cached": the flag records what a `UID SEARCH` said
    /// about the folder (no more UIDs below the floor), not a byte-for-byte inventory of what a
    /// fetch delivered. Keeping the two apart matters because the flag suppresses future attempts —
    /// so it may only claim the thing that was actually established.
    ///
    /// False for a folder that has never been synced, which is the safe direction: asking the
    /// server once more costs a round trip, while believing the bottom is reached when it is not
    /// costs mail the user can never reach.
    pub fn history_complete(&self, account_id: &str, folder: &str) -> Result<bool> {
        let complete: Option<i64> = self
            .conn
            .query_row(
                "select history_complete from folders where account_id = ?1 and name = ?2",
                params![account_id, folder],
                |row| row.get(0),
            )
            .optional()?;
        Ok(complete.unwrap_or(0) != 0)
    }

    /// Remember that a backfill reached the bottom of the folder.
    ///
    /// The update is fenced on `uidvalidity` on purpose. If the server renumbered the folder while
    /// the backfill was in flight, its rows are about to be discarded, and marking the folder
    /// complete would be a claim about a cache that no longer exists — the older mail would then be
    /// unreachable with nothing left to ask for it again.
    pub fn mark_history_complete(
        &self,
        account_id: &str,
        folder: &str,
        uidvalidity: u32,
    ) -> Result<()> {
        self.conn.execute(
            "update folders set history_complete = 1
             where account_id = ?1 and name = ?2 and uidvalidity = ?3",
            params![account_id, folder, i64::from(uidvalidity)],
        )?;
        Ok(())
    }
```

### 2.5 测试基建：`cache.rs` 的 `mod tests` 里加一个夹具

**插在**下面这一行之前（它是 `mod tests` 里第一个测试）：
```rust
    #[test]
    fn schema_is_created_with_fts5_available() {
```

```rust
    fn inbox() -> MailboxInfo {
        MailboxInfo {
            name: "INBOX".to_string(),
            display_name: "收件箱".to_string(),
            delimiter: Some("/".to_string()),
            selectable: true,
        }
    }
```

### 2.6 四个新单测

**插在**下面这一行之前（`mod tests` 的最后一个测试）：
```rust
    #[test]
    fn a_cache_from_a_newer_schema_is_refused_rather_than_misread() {
```

```rust
    #[test]
    fn the_oldest_cached_uid_is_where_a_backfill_resumes() {
        let cache = Cache::open_in_memory().unwrap();
        assert_eq!(
            cache.oldest_uid("acc", "INBOX").unwrap(),
            None,
            "nothing cached means no floor to walk down from"
        );

        cache
            .upsert_summaries(
                "acc",
                "INBOX",
                &[
                    summary(9, "九", "a@b.c", 9),
                    summary(3, "三", "a@b.c", 3),
                    summary(7, "七", "a@b.c", 7),
                ],
            )
            .unwrap();
        assert_eq!(cache.oldest_uid("acc", "INBOX").unwrap(), Some(3));

        // A stored copy of the floor would still say 3 here, and the next backfill would re-fetch
        // mail that is no longer there.
        cache.delete_message("acc", "INBOX", 3).unwrap();
        assert_eq!(cache.oldest_uid("acc", "INBOX").unwrap(), Some(7));
    }

    #[test]
    fn a_backfill_may_only_complete_the_generation_it_read() {
        let cache = Cache::open_in_memory().unwrap();
        cache.upsert_folder("acc", &inbox(), 111).unwrap();
        assert!(
            !cache.history_complete("acc", "INBOX").unwrap(),
            "a folder that has never been backfilled is not complete"
        );

        // A page fetched against another generation must not be able to claim this one is done.
        cache.mark_history_complete("acc", "INBOX", 222).unwrap();
        assert!(!cache.history_complete("acc", "INBOX").unwrap());

        cache.mark_history_complete("acc", "INBOX", 111).unwrap();
        assert!(cache.history_complete("acc", "INBOX").unwrap());

        // And the folder being renumbered throws the claim away with the rows.
        cache.invalidate_folder("acc", "INBOX").unwrap();
        assert!(
            !cache.history_complete("acc", "INBOX").unwrap(),
            "a renumbered folder must be backfillable again"
        );
    }

    #[test]
    fn re_syncing_a_folder_does_not_forget_its_history() {
        // The trap: adding the new column to `upsert_folder`'s conflict clause. Every incremental
        // sync upserts the folder row, so a folder that had been backfilled to the bottom would
        // forget that fact on every single sync and offer to load more forever.
        let cache = Cache::open_in_memory().unwrap();
        cache.upsert_folder("acc", &inbox(), 111).unwrap();
        cache.mark_history_complete("acc", "INBOX", 111).unwrap();

        cache.upsert_folder("acc", &inbox(), 111).unwrap();
        assert!(cache.history_complete("acc", "INBOX").unwrap());
    }

    #[test]
    fn a_v1_database_is_upgraded_in_place_with_its_rows_intact() {
        // What an earlier build left on disk: the same tables, no `history_complete`, and
        // user_version = 1. Recreating the schema instead of altering it would throw the user's
        // cached mail away to add one integer.
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "create table folders (
                 account_id text not null, name text not null, display_name text not null,
                 delimiter text, selectable integer not null default 1,
                 uidvalidity integer not null default 0, last_uid integer not null default 0,
                 synced_at integer, primary key (account_id, name));
             create table messages (
                 account_id text not null, folder text not null, uid integer not null,
                 subject text not null default '', from_name text, from_addr text, to_addrs text,
                 date integer, size integer, seen integer not null default 0,
                 flagged integer not null default 0, primary key (account_id, folder, uid));
             create table bodies (
                 account_id text not null, folder text not null, uid integer not null,
                 detail text not null, fetched_at integer not null,
                 primary key (account_id, folder, uid));
             create virtual table search using fts5(
                 subject, from_name, from_addr, to_addrs, body,
                 account_id unindexed, folder unindexed, uid unindexed, tokenize = 'unicode61');
             insert into folders (account_id, name, display_name, selectable, uidvalidity, last_uid)
                 values ('acc', 'INBOX', '收件箱', 1, 7, 42);
             insert into messages (account_id, folder, uid, subject, date, seen, flagged)
                 values ('acc', 'INBOX', 42, '旧邮件', 100, 0, 0);
             pragma user_version = 1;",
        )
        .unwrap();

        let cache = Cache { conn };
        cache.migrate().expect("a v1 database must be upgraded");

        assert_eq!(
            cache.summary_count("acc", "INBOX").unwrap(),
            1,
            "the cached rows must survive the migration"
        );
        assert_eq!(
            cache.folder_state("acc", "INBOX").unwrap(),
            Some(FolderState {
                uidvalidity: 7,
                last_uid: 42
            }),
            "and so must the cursor"
        );
        assert!(
            !cache.history_complete("acc", "INBOX").unwrap(),
            "a v1 cache has never completed a backfill, so it must not claim to have"
        );
        let version: i64 = cache
            .conn
            .query_row("pragma user_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);

        // Idempotent: a second open must not try to add the column again.
        cache.migrate().expect("migrating an up-to-date database is a no-op");
    }
```

---

## 3. `crates/mail-core/src/imap.rs`

### 3.1 新增 `OlderPage`

**插在**下面这一行之前（它紧跟 `HeaderPage` 的定义）：
```rust
/// Everything a transport must be, named once.
```

```rust
/// One page of *older* headers, and whether the bottom of the folder was reached.
///
/// `exhausted` is computed from the `UID SEARCH` result, not from `summaries.len()` after the
/// fetch: a server may answer a fetch with fewer messages than were asked for (a UID can be
/// expunged between the search and the fetch), and guessing "exhausted" there means history the
/// user can never reach.
#[derive(Debug, Clone)]
pub struct OlderPage {
    /// `UIDVALIDITY` as reported by this `SELECT`.
    pub uidvalidity: u32,
    /// The newest `limit` messages below `before_uid`, oldest first.
    pub summaries: Vec<MessageSummary>,
    /// There is nothing older than `summaries` in the folder.
    pub exhausted: bool,
}
```

### 3.2 新增 `fetch_older_headers()`

**插在**下面这一行之前（它是 `fetch_message` 的文档首行，即 `fetch_headers_since` 之后）：
```rust
    /// Fetch and fully parse one message.
```

```rust
    /// Page *backwards* through a folder: the newest `limit` headers below `before_uid`.
    ///
    /// This is the one primitive that moves away from the incremental cursor instead of along it.
    /// A `UID FETCH` set cannot express "older than", so the UID list is read first (`UID SEARCH
    /// ALL` — the same call the first sync already makes) and the window is taken from it. That is
    /// also what makes `exhausted` exact rather than a guess: if the folder holds no more UIDs below
    /// `before_uid` than the window itself, the bottom has been reached.
    ///
    /// The range is sent as a closed `a:b` rather than `<floor>:*`. An open-ended range whose start
    /// is above the folder's highest UID resolves to the *last* message — RFC 3501 says ranges are
    /// unordered — which is how a backfill would silently re-fetch the newest mail forever.
    pub async fn fetch_older_headers(
        &mut self,
        mailbox: &str,
        before_uid: u32,
        limit: usize,
    ) -> Result<OlderPage> {
        let limit = limit.max(1);
        timed(async {
            let selected = self.session.select(mailbox).await.map_err(imap_err)?;
            // A public field in async-imap, not an accessor.
            let uidvalidity = selected.uid_validity.unwrap_or(0);

            // UID 1 is the oldest a folder can hold, so there is nothing below it.
            if before_uid <= 1 {
                return Ok(OlderPage {
                    uidvalidity,
                    summaries: Vec::new(),
                    exhausted: true,
                });
            }

            let mut uids: Vec<u32> = self
                .session
                .uid_search("ALL")
                .await
                .map_err(imap_err)?
                .into_iter()
                .filter(|uid| *uid < before_uid)
                .collect();
            uids.sort_unstable();

            if uids.is_empty() {
                return Ok(OlderPage {
                    uidvalidity,
                    summaries: Vec::new(),
                    exhausted: true,
                });
            }

            // Exact, not a heuristic: when there are no more older UIDs than one page, this page is
            // all that is left.
            let exhausted = uids.len() <= limit;
            let window = &uids[uids.len().saturating_sub(limit)..];
            let set = format!("{}:{}", window[0], window[window.len() - 1]);

            let mut stream = self
                .session
                .uid_fetch(&set, "(UID FLAGS RFC822.SIZE BODY.PEEK[HEADER])")
                .await
                .map_err(imap_err)?;

            let mut summaries = Vec::with_capacity(window.len());
            while let Some(fetch) = stream.try_next().await.map_err(imap_err)? {
                let summary = summarise(&fetch);
                // Defensive: both ends of the range are below `before_uid`, so anything at or above
                // it would mean the server answered a different question.
                if summary.uid < before_uid {
                    summaries.push(summary);
                }
            }
            summaries.sort_by_key(|m| m.uid);

            Ok(OlderPage {
                uidvalidity,
                summaries,
                exhausted,
            })
        })
        .await
    }
```

---

## 4. `crates/mail-core/src/backfill.rs`（新文件，整份照抄）

```rust
//! History backfill: pull in the mail that was already old when the folder was first synced.
//!
//! Incremental sync only ever walks *forward* — [`crate::sync`] keeps a `(uidvalidity, last_uid)`
//! cursor and asks for `UID FETCH <last_uid+1>:*`. That leaves the newest
//! [`crate::sync::INITIAL_SYNC`] headers cached and everything older unreachable, which is fine
//! until the user scrolls past the bottom of a folder they have had for years.
//!
//! This module walks the other way, and the two directions must stay independent:
//!
//! * **The forward cursor is never touched here.** A backfill stores rows *below* `last_uid`, so
//!   moving it would make the next sync either re-read the folder or — worse — skip whatever
//!   arrived while the backfill was running. That is why this is a separate driver rather than
//!   another arm of [`crate::sync::Fetch`], and why `tools/check.ps1` refuses to see a call to
//!   `Cache::set_cursor` in the non-test part of this file.
//! * **Where to resume is derived, not stored.** The oldest cached UID is the resume point, read
//!   from the rows themselves, so deleting a message cannot leave a pointer behind. The only state
//!   that has to persist is the *end* condition: that a backfill once reached the bottom of the
//!   folder, so "load more" does not ask the server again on every later press.
//!
//! Like [`crate::sync`], this is a driver carrying owned data across the one `await` it needs:
//! `rusqlite::Connection` is `!Sync`, so a future holding a `&Cache` across an `.await` would not
//! be `Send`, and a Tauri command's future must be.

use serde::Serialize;

use crate::cache::Cache;
use crate::error::Result;
use crate::imap::{ImapClient, OlderPage};
use crate::model::MailboxInfo;

/// How many older headers one backfill pulls in.
///
/// Headers only — a few hundred bytes each — and the page is shown as soon as it lands, so this is
/// sized for "a screenful or two of scrolling" rather than for the folder.
pub const BACKFILL_BATCH: usize = 200;

/// What one backfill did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BackfillOutcome {
    /// Older headers stored by this pass.
    pub fetched: usize,
    /// How many messages the folder now has cached in total.
    pub total_cached: u32,
    /// The bottom of the folder has been reached; there is nothing older to ask for.
    pub exhausted: bool,
    /// The server had renumbered the folder: nothing was stored, and the cache needs a refresh.
    pub renumbered: bool,
}

/// Carries one backfill across the one `await` it needs.
///
/// Owned data only, for the same reason [`crate::sync::SyncDriver`] is: it has to stay `Send` so a
/// Tauri command can hold it while the network call is in flight without holding the `!Sync`
/// SQLite connection.
#[derive(Debug, Clone)]
pub struct BackfillDriver {
    account_id: String,
    folder: MailboxInfo,
    /// Fetch headers strictly below this UID.
    before_uid: u32,
    /// Generation the cached rows belong to; 0 means "unknown".
    cached_uidvalidity: u32,
    limit: usize,
}

impl BackfillDriver {
    /// Read the cache and decide whether there is anything to ask the server for.
    ///
    /// `None` means "do not touch the network": either the folder's history is already known to be
    /// complete, or nothing is cached for it at all. An unsynced folder has no floor to walk down
    /// from — it gets its first window from [`crate::sync`].
    pub fn start(cache: &Cache, account_id: &str, folder: &MailboxInfo) -> Result<Option<Self>> {
        if cache.history_complete(account_id, &folder.name)? {
            return Ok(None);
        }
        let Some(before_uid) = cache.oldest_uid(account_id, &folder.name)? else {
            return Ok(None);
        };
        let cached_uidvalidity = cache
            .folder_state(account_id, &folder.name)?
            .map_or(0, |state| state.uidvalidity);

        Ok(Some(Self {
            account_id: account_id.to_string(),
            folder: folder.clone(),
            before_uid,
            cached_uidvalidity,
            limit: BACKFILL_BATCH,
        }))
    }

    /// The UID the next page is fetched below.
    pub fn before_uid(&self) -> u32 {
        self.before_uid
    }

    /// Perform this pass's fetch. The only network call in a backfill.
    pub async fn execute(&self, client: &mut ImapClient) -> Result<OlderPage> {
        client
            .fetch_older_headers(&self.folder.name, self.before_uid, self.limit)
            .await
    }

    /// Store what came back. Deliberately does not move the forward cursor.
    pub fn absorb(&mut self, cache: &Cache, page: &OlderPage) -> Result<BackfillOutcome> {
        // `0` means the server did not say, and treating that as a change would throw away a good
        // page. Same rule `SyncDriver` follows.
        if self.cached_uidvalidity != 0
            && page.uidvalidity != 0
            && page.uidvalidity != self.cached_uidvalidity
        {
            // Every stored UID may now point at a different message, so these headers are not
            // trustworthy *relative to what is cached*. Storing them would put mail under the wrong
            // subject line, which is the one thing this cache must never do. The caller refreshes.
            return Ok(BackfillOutcome {
                fetched: 0,
                total_cached: cache.summary_count(&self.account_id, &self.folder.name)?,
                exhausted: false,
                renumbered: true,
            });
        }

        cache.upsert_summaries(&self.account_id, &self.folder.name, &page.summaries)?;
        if page.exhausted {
            // Fenced on the generation inside the cache: if the folder was renumbered while this
            // page was in flight, the claim is dropped rather than written against a cache that no
            // longer exists.
            cache.mark_history_complete(&self.account_id, &self.folder.name, page.uidvalidity)?;
        }

        Ok(BackfillOutcome {
            fetched: page.summaries.len(),
            total_cached: cache.summary_count(&self.account_id, &self.folder.name)?,
            exhausted: page.exhausted,
            renumbered: false,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache::FolderState;
    use crate::model::MessageSummary;

    fn summary(uid: u32) -> MessageSummary {
        MessageSummary {
            uid,
            subject: format!("主题 {uid}"),
            from_name: None,
            from_addr: Some("a@b.c".to_string()),
            date: Some(i64::from(uid)),
            size: None,
            seen: false,
            flagged: false,
        }
    }

    fn folder() -> MailboxInfo {
        MailboxInfo {
            name: "INBOX".to_string(),
            display_name: "收件箱".to_string(),
            delimiter: Some("/".to_string()),
            selectable: true,
        }
    }

    /// A folder cached from `uids` upwards, under generation `generation`.
    fn cache_with(generation: u32, uids: std::ops::RangeInclusive<u32>) -> Cache {
        let cache = Cache::open_in_memory().expect("cache");
        cache
            .upsert_folder("acc", &folder(), generation)
            .expect("folder");
        let rows: Vec<MessageSummary> = uids.map(summary).collect();
        cache
            .upsert_summaries("acc", "INBOX", &rows)
            .expect("rows");
        cache
    }

    #[test]
    fn a_folder_that_has_never_been_synced_has_nothing_to_backfill() {
        let cache = Cache::open_in_memory().unwrap();
        assert!(
            BackfillDriver::start(&cache, "acc", &folder()).unwrap().is_none(),
            "with no rows cached there is no floor to walk down from"
        );
    }

    #[test]
    fn the_plan_resumes_from_the_oldest_cached_uid() {
        let cache = cache_with(5, 40..=42);
        let driver = BackfillDriver::start(&cache, "acc", &folder())
            .unwrap()
            .expect("a plan");
        assert_eq!(driver.before_uid(), 40);
    }

    #[test]
    fn a_backfill_stores_older_headers_and_leaves_the_cursor_alone() {
        let cache = cache_with(5, 40..=42);
        cache.set_cursor("acc", "INBOX", 5, 42).unwrap();

        let mut driver = BackfillDriver::start(&cache, "acc", &folder())
            .unwrap()
            .expect("a plan");
        let page = OlderPage {
            uidvalidity: 5,
            summaries: (30..=32).map(summary).collect(),
            exhausted: false,
        };
        let outcome = driver.absorb(&cache, &page).unwrap();

        assert_eq!(outcome.fetched, 3);
        assert_eq!(outcome.total_cached, 6);
        assert!(!outcome.exhausted);
        assert!(!outcome.renumbered);
        assert_eq!(
            cache.folder_state("acc", "INBOX").unwrap(),
            Some(FolderState {
                uidvalidity: 5,
                last_uid: 42
            }),
            "a backfill walks backwards: moving the forward cursor would make the next sync \
             re-read the folder, or skip whatever arrived while this one ran"
        );
        assert_eq!(
            cache.oldest_uid("acc", "INBOX").unwrap(),
            Some(30),
            "and the new floor is what the next page is fetched below"
        );
    }

    #[test]
    fn reaching_the_bottom_is_remembered() {
        let cache = cache_with(5, 40..=42);
        let mut driver = BackfillDriver::start(&cache, "acc", &folder())
            .unwrap()
            .expect("a plan");
        let page = OlderPage {
            uidvalidity: 5,
            summaries: vec![summary(1), summary(2)],
            exhausted: true,
        };
        assert!(driver.absorb(&cache, &page).unwrap().exhausted);

        assert!(
            BackfillDriver::start(&cache, "acc", &folder()).unwrap().is_none(),
            "the bottom must be remembered, or every press would ask the server again"
        );
    }

    #[test]
    fn a_page_from_another_generation_is_discarded_rather_than_stored() {
        let cache = cache_with(5, 40..=42);
        let mut driver = BackfillDriver::start(&cache, "acc", &folder())
            .unwrap()
            .expect("a plan");

        // The server renumbered the folder while the fetch was in flight.
        let page = OlderPage {
            uidvalidity: 6,
            summaries: vec![summary(30)],
            exhausted: true,
        };
        let outcome = driver.absorb(&cache, &page).unwrap();

        assert!(outcome.renumbered, "the caller has to refresh instead");
        assert_eq!(outcome.fetched, 0);
        assert_eq!(
            cache.summary_count("acc", "INBOX").unwrap(),
            3,
            "headers from a dead generation must not be stored under the live one"
        );
        assert!(
            !cache.history_complete("acc", "INBOX").unwrap(),
            "and the folder must not be declared complete on their say-so"
        );
    }

    #[test]
    fn a_server_that_reports_no_generation_at_all_never_looks_renumbered() {
        // Same rule as `SyncDriver`: 0 means "cannot tell", and treating it as a change would throw
        // away a perfectly good page.
        let cache = cache_with(0, 40..=42);
        let mut driver = BackfillDriver::start(&cache, "acc", &folder())
            .unwrap()
            .expect("a plan");
        let page = OlderPage {
            uidvalidity: 0,
            summaries: vec![summary(30)],
            exhausted: true,
        };
        let outcome = driver.absorb(&cache, &page).unwrap();

        assert!(!outcome.renumbered);
        assert_eq!(outcome.fetched, 1);
        assert_eq!(cache.summary_count("acc", "INBOX").unwrap(), 4);
    }
}
```

---

## 5. `mail-core` 的导出与注释，外加一个顺带修掉的真 bug

### 5.1 `lib.rs` —— 四处独立改动

#### 5.1a 模块文档加一行

**替换前**
```rust
//! - [cache] — local SQLite copy of what has been synced, plus the FTS5 search index.
//! - [`sync`] — incremental folder sync on top of the cache.
```
**替换后**
```rust
//! - [cache] — local SQLite copy of what has been synced, plus the FTS5 search index.
//! - [`sync`] — incremental folder sync on top of the cache.
//! - [`backfill`] — pull older history into the cache, without disturbing the sync cursor.
```

#### 5.1b 模块声明

**替换前**
```rust
pub mod cache;
```
**替换后**
```rust
pub mod backfill;
pub mod cache;
```

#### 5.1c 重导出

**替换前**
```rust
pub use cache::{Cache, FolderState, SearchHit};
```
**替换后**
```rust
pub use backfill::{BACKFILL_BATCH, BackfillDriver, BackfillOutcome};
pub use cache::{Cache, FolderState, SearchHit};
```

#### 5.1d `OlderPage` 也要导出

**替换前**
```rust
pub use imap::{HeaderPage, ImapClient};
```
**替换后**
```rust
pub use imap::{HeaderPage, ImapClient, OlderPage};
```

### 5.2 `sync.rs` —— 只改两处注释，**不动任何代码**

**替换前**
```rust
/// How many headers a folder gets on its very first sync.
///
/// Headers only, so this is on the order of a few hundred kilobytes. History beyond this is not
/// cached yet — see the note in README about what search can and cannot see.
pub const INITIAL_SYNC: usize = 500;
```
**替换后**
```rust
/// How many headers a folder gets on its very first sync.
///
/// Headers only, so this is on the order of a few hundred kilobytes. Anything older is reachable
/// on demand rather than up front: see [`crate::backfill`], which walks *below* the oldest cached
/// UID in [`BACKFILL_BATCH`][crate::backfill::BACKFILL_BATCH] steps without touching the cursor
/// this module owns.
pub const INITIAL_SYNC: usize = 500;
```

**替换前**（模块头，第 5~9 行）
```rust
//! * **First time** — fetch the newest [`INITIAL_SYNC`] headers. Deliberately *not* everything from
//!   UID 1: a folder with twenty thousand messages would make the first launch unusable, and the UI
//!   only ever shows one page at a time anyway.
```
**替换后**
```rust
//! * **First time** — fetch the newest [`INITIAL_SYNC`] headers. Deliberately *not* everything from
//!   UID 1: a folder with twenty thousand messages would make the first launch unusable, and the UI
//!   only ever shows one page at a time anyway. Older mail is fetched on demand by
//!   [`crate::backfill`], which is a separate driver precisely because it must not move the cursor
//!   below.
```

### 5.3 顺带修掉的一个真 bug：增量同步截断时**丢邮件**（改 `imap.rs`）

**这不是本次功能引入的，是写计划时顺带发现的。** 它必须修，因为它正好毁掉回填承诺的那件事
（"更早的邮件都拿得到"）：

`fetch_headers_since` 一次拿到的新邮件超过 `SYNC_BATCH`（500）时，保留的是**最新**的 500 封；
而 `advance_cursor` 把 `last_uid` 推进到**已存的最大 UID**。于是被丢掉的那一段
（旧游标 +1 到这一批的下界之间）**永远不会再被请求**：增量同步只会往上走，
回填只往"最小已存 UID 之下"走，两边都够不到中间那个洞。注释里那句
"the rest is picked up next time" 是**错的**（游标只推进到"存了什么"，不保证"中间没漏"）。

触发条件不苛刻：一次性进来 500 封以上（长假回来、订阅型文件夹），且此后 UIDVALIDITY 不变。
后果是静默的：列表上两段日期之间少一段，搜索也搜不到。

改法：截断时保留**最旧**的一批。代价是那种极端情况下最新几封要等下一次同步才出现，
换来的是不再丢邮件，而且注释变成真的。

**替换前**
```rust
            // Keep the newest `limit` when a folder has more new mail than we want in one go; the
            // cursor only advances to what was actually stored, so the rest is picked up next time.
            if limit > 0 && summaries.len() > limit {
                summaries = summaries.split_off(summaries.len() - limit);
            }
```
**替换后**
```rust
            // More new mail than one batch: keep the *oldest* of it. See `keep_oldest_page`.
            keep_oldest_page(&mut summaries, limit);
```

**新增辅助函数**，**插在**下面这一行之前（它是 `HeaderPage` 的文档首行，**现在文件里就有**，
所以这一步不依赖 §3.1 是否已经做过）：

```rust
/// One page of incremental headers, with the folder generation it belongs to.
```

```rust
/// Trim an over-long incremental page to `limit` messages, **keeping the oldest**.
///
/// Which end is kept is not a detail. The cursor advances to the highest stored UID and thereafter
/// means "everything up to here has been fetched" — so keeping the *newest* batch would move it past
/// mail that was never stored, and no later sync would ever ask for it again. Nothing else can
/// recover it either: the backfill only walks below the oldest cached UID, which is far below the
/// hole. Keeping the oldest batch makes progress instead, because the next sync continues above it.
fn keep_oldest_page(summaries: &mut Vec<MessageSummary>, limit: usize) {
    if limit > 0 && summaries.len() > limit {
        summaries.truncate(limit);
    }
}
```

**新增单测**（放在 `imap.rs` 的 `mod tests` 里，`dt` 旁边；`MessageSummary` 已经在该文件 import 了）：

```rust
    fn summary(uid: u32) -> MessageSummary {
        MessageSummary {
            uid,
            subject: format!("主题 {uid}"),
            from_name: None,
            from_addr: Some("a@b.c".to_string()),
            date: Some(i64::from(uid)),
            size: None,
            seen: false,
            flagged: false,
        }
    }

    #[test]
    fn an_over_long_incremental_page_keeps_the_oldest_batch() {
        // The dropped end is unreachable forever: `advance_cursor` moves `last_uid` to the highest
        // *stored* UID, so anything below it that was never stored is never requested again.
        let mut page: Vec<MessageSummary> = (1..=10).map(summary).collect();
        keep_oldest_page(&mut page, 4);
        assert_eq!(
            page.iter().map(|m| m.uid).collect::<Vec<_>>(),
            vec![1, 2, 3, 4],
            "keeping the newest batch would skip 1..=6 for good"
        );

        // A page that already fits, and a limit of 0 (no cap), are both left alone.
        let mut fits: Vec<MessageSummary> = (1..=3).map(summary).collect();
        keep_oldest_page(&mut fits, 4);
        assert_eq!(fits.len(), 3);
        keep_oldest_page(&mut fits, 0);
        assert_eq!(fits.len(), 3);
    }
```

---

## 6. `src-tauri/src/lib.rs`

### 6.1 import

**替换前**
```rust
use mail_core::{
    AccountConfig, Cache, ConnectionPool, ConnectionReport, MailError, MailboxInfo, MessageDetail,
    MessageSummary, OutgoingAttachment, OutgoingMail, PushEvent, SearchHit, SyncDriver,
    SyncOutcome, account_with_login, secret, watch,
};
```
**替换后**
```rust
use mail_core::{
    AccountConfig, BackfillDriver, Cache, ConnectionPool, ConnectionReport, MailError, MailboxInfo,
    MessageDetail, MessageSummary, OutgoingAttachment, OutgoingMail, PushEvent, SearchHit,
    SyncDriver, SyncOutcome, account_with_login, secret, watch,
};
```

> **只导入 `BackfillDriver`。** 不要顺手写上 `BackfillOutcome` —— 命令里只用 `let outcome = ...`
> 接住返回值、从不写出类型名，多导入一个就是 `unused_imports` 警告，而门禁带 `-D warnings`。

### 6.2 常量 `MAX_LIST_ROWS`

**插在**下面这一行之前：
```rust
/// The full built-in provider database, for the setup wizard.
```

```rust
/// Upper bound on how many list rows one command hands to the UI.
///
/// Not a UX limit — paging here means "ask for a bigger page on each press", and this is what stops
/// that from becoming an unbounded IPC payload and an unbounded DOM. The frontend mirrors it as
/// `MAX_LIST_ROWS` in `src/app.js`, and `tools/check.ps1` fails if the two numbers disagree: too
/// high on this side and the button silently does nothing, too low and the backend refuses a page
/// the UI asked for.
const MAX_LIST_ROWS: usize = 1000;
```

### 6.3 `list_messages` —— 上限 + 抽出 `cached_page`

**替换前**
```rust
    // Guard against a caller asking for the whole mailbox; the UI pages at 50.
    let limit = limit.clamp(1, 500);
```
**替换后**
```rust
    // Guard against a caller asking for the whole mailbox; the UI pages at 50 and grows from there.
    let limit = limit.clamp(1, MAX_LIST_ROWS);
```

**替换前**
```rust
    let cached = Cache::open(&state.cache_path)
        .map_err(describe)?
        .list_summaries(&account_id, &mailbox, limit as u32, 0)
        .map_err(describe)?;
```
**替换后**
```rust
    let cached = cached_page(&state.cache_path, &account_id, &mailbox, limit as u32)?;
```

### 6.4 新增 `cached_page()`

**插在**下面这一行之前：
```rust
#[tauri::command]
async fn list_mailboxes(
```

```rust
/// One page of the cached list, newest first.
///
/// A free function rather than a closure so that no borrow of the command's `State` is held across
/// the `await`s in `load_more_messages`, and because the two list commands must page identically:
/// a different query here would make "load more" jump.
fn cached_page(
    cache_path: &Path,
    account_id: &str,
    mailbox: &str,
    limit: u32,
) -> Result<Vec<MessageSummary>, String> {
    Cache::open(cache_path)
        .map_err(describe)?
        .list_summaries(account_id, mailbox, limit, 0)
        .map_err(describe)
}
```

### 6.5 新命令 `load_more_messages` + 返回类型

**插在**下面这一行之前（即 `list_messages` 之后）：
```rust
#[tauri::command]
async fn fetch_message(
```

```rust
/// The message list, one page further back.
///
/// Serves the page from the cache and only goes to the server when the cache cannot fill it — so
/// the presses that stay inside the window the first sync already stored cost no round trip at all.
/// When it does go out it goes *backwards* ([`BackfillDriver`]), which leaves the incremental
/// cursor where it is; the two directions are orthogonal on purpose.
#[tauri::command]
async fn load_more_messages(
    state: State<'_, AppState>,
    account_id: String,
    mailbox: String,
    limit: usize,
) -> Result<OlderMessages, String> {
    let cfg = state.find(&account_id)?;
    let limit = limit.clamp(1, MAX_LIST_ROWS) as u32;

    // The folder has to be known to the cache: `get_folder` gives the wire name and display name
    // without a `LIST`. An unknown folder has never been synced, and there is no floor to walk down
    // from until something is cached, so sync it first.
    //
    // Bound to a `let` first on purpose: a `match` scrutinee's temporaries live until the end of the
    // match, which would keep this SQLite connection open across the `refresh_folder` round trip —
    // the one thing every other command in this file avoids.
    let known = Cache::open(&state.cache_path)
        .map_err(describe)?
        .get_folder(&cfg.id, &mailbox)
        .map_err(describe)?;
    let info = match known {
        Some(info) => info,
        None => {
            refresh_folder(&state.pool, &state.cache_path, &cfg, &mailbox).await?;
            Cache::open(&state.cache_path)
                .map_err(describe)?
                .get_folder(&cfg.id, &mailbox)
                .map_err(describe)?
                .ok_or_else(|| format!("服务器上找不到文件夹「{mailbox}」"))?
        }
    };

    let page_now = cached_page(&state.cache_path, &account_id, &mailbox, limit)?;

    // The cache can already fill the page: no reason to bother the server, and none to need the
    // credential — which is what makes paging inside the cached window work offline.
    if page_now.len() as u32 >= limit {
        return Ok(OlderMessages {
            messages: page_now,
            exhausted: Cache::open(&state.cache_path)
                .map_err(describe)?
                .history_complete(&account_id, &mailbox)
                .map_err(describe)?,
            fetched: 0,
            renumbered: false,
        });
    }

    let planned = BackfillDriver::start(
        &Cache::open(&state.cache_path).map_err(describe)?,
        &account_id,
        &info,
    )
    .map_err(describe)?;

    let Some(mut driver) = planned else {
        // `start` returns `None` for two different reasons and they must not be conflated: the
        // bottom is already known, or nothing is cached for this folder at all. Only the first is
        // "there is nothing older"; the second is "not known", and hard-coding `true` here would
        // tell the UI a folder with no cached rows is fully loaded.
        let exhausted = Cache::open(&state.cache_path)
            .map_err(describe)?
            .history_complete(&account_id, &mailbox)
            .map_err(describe)?;
        return Ok(OlderMessages {
            messages: page_now,
            exhausted,
            fetched: 0,
            renumbered: false,
        });
    };

    let password = password_for(&cfg.id)?;

    // Built once, outside the macro, so `start` runs *before* the credential is demanded: a folder
    // whose bottom is already known must not need a password to say so. A pool retry therefore
    // re-runs the same fetch window rather than re-planning — which is harmless, because
    // `upsert_summaries` is idempotent and `mark_history_complete` is fenced on the generation.
    let outcome = match pool_op!(state.pool, &cfg, &password, |client| {
        let page = driver.execute(&mut client).await?;
        driver.absorb(&Cache::open(&state.cache_path)?, &page)
    }) {
        Ok(outcome) => outcome,
        Err(error) => {
            let message = describe(error);
            // The same field `sync_status` reports: a backfill *is* a server refresh, and if it just
            // failed, the list on screen is at least as stale as the offline notice claims.
            *state.last_sync_error.lock().expect("sync error mutex") = Some(message.clone());
            return Err(message);
        }
    };

    // Reaching the server at all means that notice is out of date.
    *state.last_sync_error.lock().expect("sync error mutex") = None;

    if outcome.renumbered {
        log_line(&format!(
            "[easy-email] 回填 {mailbox}：服务器重新编号了该文件夹，本次结果已丢弃"
        ));
        return Ok(OlderMessages {
            messages: page_now,
            exhausted: false,
            fetched: 0,
            renumbered: true,
        });
    }

    log_line(&format!(
        "[easy-email] 回填 {mailbox}：新增 {} 封，缓存共 {} 封{}",
        outcome.fetched,
        outcome.total_cached,
        if outcome.exhausted {
            "（已到最早一封）"
        } else {
            ""
        }
    ));

    Ok(OlderMessages {
        messages: cached_page(&state.cache_path, &account_id, &mailbox, limit)?,
        exhausted: outcome.exhausted,
        fetched: outcome.fetched,
        renumbered: false,
    })
}

/// One page of the list, as returned by [`load_more_messages`].
#[derive(serde::Serialize)]
struct OlderMessages {
    /// The page as the cache now holds it, newest first.
    messages: Vec<MessageSummary>,
    /// Nothing older exists in this folder, so "加载更多" has nothing left to do.
    exhausted: bool,
    /// Older headers this call pulled from the server (0 when the cache already had enough).
    fetched: usize,
    /// The server renumbered the folder: this page is stale and the caller should refresh.
    renumbered: bool,
}
```

### 6.6 `generate_handler!`

**替换前**
```rust
            list_messages,
            fetch_message,
```
**替换后**
```rust
            list_messages,
            load_more_messages,
            fetch_message,
```

---

## 7. 前端

### 7.1 `src/index.html`

**替换前**
```html
        <header class="pane-header">
          <h1 id="mailbox-title">收件箱</h1>
          <div class="pane-actions">
            <button id="btn-refresh" class="btn ghost" title="重新拉取">↻ 刷新</button>
          </div>
        </header>
```
**替换后**
```html
        <header class="pane-header">
          <h1 id="mailbox-title">收件箱</h1>
          <div class="pane-actions">
            <button id="btn-refresh" class="btn ghost" title="重新拉取">↻ 刷新</button>
          </div>
        </header>
        <!--
          Shown when the last server refresh failed. The list below is still served from the cache,
          which is the whole point of having one — but presenting cached mail as if it were fresh is
          how a user ends up trusting an inbox that quietly stopped updating hours ago.
        -->
        <div id="offline-note" class="offline-note" hidden></div>
```

**替换前**
```html
        <div id="list-footer" class="list-footer" hidden>
          <button id="btn-load-more" class="btn ghost block">加载更多</button>
        </div>
```
**替换后**
```html
        <div id="list-footer" class="list-footer" hidden>
          <button id="btn-load-more" class="btn ghost block">加载更多</button>
          <!-- Replaces the button once the server has nothing older for this folder. -->
          <div id="list-note" class="list-note muted" hidden></div>
        </div>
```

### 7.2 `src/style.css`

**插在**下面这一行之前（`reader` 那一段的分隔注释）：
```css
/* ---------------------------------------------------------------- reader */
```

```css
/* Offline notice: the list is cached mail, and saying so is the difference between "slow" and
   "silently wrong". `pre-line` keeps the backend's cause/advice line break readable. */
.offline-note {
  margin: 0 10px 6px;
  padding: 5px 8px;
  border: 1px solid var(--border);
  border-radius: 7px;
  background: var(--surface-2);
  color: var(--text-muted);
  font-size: 11px;
  overflow-wrap: anywhere;
  white-space: pre-line;
}

.list-note {
  text-align: center;
  font-size: 11px;
  padding: 2px 0;
}
```

### 7.3 `src/app.js`

#### (a) 常量

**替换前**
```js
const PAGE_SIZE = 50;
```
**替换后**
```js
const PAGE_SIZE = 50;

/**
 * Upper bound on how many rows the list will ever hold.
 *
 * The mirror of `MAX_LIST_ROWS` in `src-tauri/src/lib.rs`; `tools/check.ps1` fails if the two
 * numbers disagree. It exists because paging here means "ask for a bigger page on each press":
 * without a ceiling the IPC payload and the DOM would both grow without bound.
 */
const MAX_LIST_ROWS = 1000;
```

#### (b) state

**替换前**
```js
  messages: [],
  limit: PAGE_SIZE,
```
**替换后**
```js
  messages: [],
  limit: PAGE_SIZE,
  /** The server has nothing older for the current folder, so "load more" is finished. */
  historyDone: false,
```

#### (c) 事件绑定

**替换前**
```js
  $('btn-load-more').addEventListener('click', () => {
    state.limit += PAGE_SIZE;
    loadMessages();
  });
```
**替换后**
```js
  $('btn-load-more').addEventListener('click', loadMore);
```

#### (d) `selectMailbox()` —— 换文件夹等于换历史

**替换前**
```js
  state.mailbox = box.name;
  state.mailboxLabel = box.display_name || box.name;
}
```
**替换后**
```js
  state.mailbox = box.name;
  state.mailboxLabel = box.display_name || box.name;
  // Each folder has its own history: whether the bottom has been reached is a fact about the folder
  // that was on screen a moment ago, not about this one.
  state.historyDone = false;
}
```

#### (d2) `loadMessages()` —— 收信期间不要留着"加载更多"

**替换前**
```js
  const list = $('message-list');
  list.innerHTML = '<div class="empty"><span class="spinner"></span>正在收取邮件…</div>';
```
**替换后**
```js
  const list = $('message-list');
  list.innerHTML = '<div class="empty"><span class="spinner"></span>正在收取邮件…</div>';
  // Nothing to page while the page itself is loading, and a click here would race this request:
  // the newest page must win. `renderMessages` shows the footer again when it has rows.
  $('list-footer').hidden = true;
```

#### (e) `loadMessages()` —— 失败路径的页脚 + 状态提示

**替换前**
```js
  if (messages === null) {
    list.innerHTML = '<div class="empty">收取失败，请查看右下角提示。</div>';
    return;
  }

  // Newest first: the server returns ascending UIDs and people want the latest on top.
  state.messages = messages.slice().sort((a, b) => b.uid - a.uid);
  renderMessages();
}
```
**替换后**
```js
  if (messages === null) {
    list.innerHTML = '<div class="empty">收取失败，请查看右下角提示。</div>';
    $('list-footer').hidden = true;
    await refreshSyncStatus();
    return;
  }

  // Newest first: the server returns ascending UIDs and people want the latest on top.
  state.messages = messages.slice().sort((a, b) => b.uid - a.uid);
  renderMessages();
  await refreshSyncStatus();
}

/**
 * One page further back through the current folder.
 *
 * The backend decides whether that costs anything: if the cache already holds more than the list is
 * showing it is a local read, and only a short page sends it to the server for older headers. The
 * button is disabled meanwhile, because a second press would ask for the same window twice.
 */
async function loadMore() {
  if (!state.accountId || !state.mailbox) return;

  // Deliberately no `if (state.historyDone) return` here. "The server has nothing older" is not the
  // same as "there is nothing left to show": a refresh resets the page to 50 rows, and the cached
  // rows behind them can still be paged through with no network at all. What hides the button is
  // `renderListFooter`, which needs both facts at once.
  const next = state.limit + PAGE_SIZE;
  if (next > MAX_LIST_ROWS) {
    // Belt and braces: `renderListFooter` already hides the button at the cap.
    toast(`列表最多显示 ${MAX_LIST_ROWS} 封，更早的邮件请用上方搜索。`, 'info', 8000);
    return;
  }

  const button = $('btn-load-more');
  const label = button.textContent;
  button.disabled = true;
  button.textContent = '正在加载更早的邮件…';

  try {
    // On failure `run` has already shown the reason; the list and `limit` stay put, so pressing
    // again is simply a retry.
    const page = await run('加载更早的邮件', () =>
      invoke('load_more_messages', {
        accountId: state.accountId,
        mailbox: state.mailbox,
        limit: next,
      })
    );
    if (page === null) {
      // The backend recorded this failure as well, so the notice above the list can explain it
      // rather than leaving it to a toast that disappears.
      await refreshSyncStatus();
      return;
    }

    if (page.renumbered) {
      // Every cached UID may now mean a different message, so the whole folder is re-read. The
      // backend has already forgotten that this history was complete.
      state.historyDone = false;
      toast('服务器重新编号了这个文件夹，正在重新同步。', 'info', 8000);
      await loadMessages({ reset: true });
      return;
    }

    state.limit = next;
    state.historyDone = page.exhausted;
    state.messages = page.messages.slice().sort((a, b) => b.uid - a.uid);
    renderMessages();
    // Reaching the server at all means an "offline" notice on screen is out of date.
    await refreshSyncStatus();

    if (page.fetched > 0) {
      toast(`已从服务器取回更早的 ${page.fetched} 封。`, 'info');
    }
  } finally {
    button.disabled = false;
    button.textContent = label;
  }
}
```

#### (f) `renderMessages()` 结尾 + 新增 `renderListFooter()`

**替换前**
```js
    button.addEventListener('click', () => openMessage(message));
    list.appendChild(button);
  }

  $('list-footer').hidden = state.messages.length < state.limit;
}
```
**替换后**
```js
    button.addEventListener('click', () => openMessage(message));
    list.appendChild(button);
  }

  renderListFooter();
}

/**
 * Whether there is more mail to page back to, and what to say about it.
 *
 * The old rule was "a full page means there might be more", which stops being true once older mail
 * can be fetched on demand. But the naive replacement — hide the button as soon as the server says
 * the bottom was reached — is worse: `historyDone` survives a refresh, and a refresh resets the page
 * to 50 rows out of a cache that may hold hundreds, so the button would vanish while most of the
 * cached mail was still unrendered and unreachable.
 *
 * Both facts are needed: the page came back short (nothing left to show) *and* the server has
 * nothing older to fetch.
 */
function renderListFooter() {
  const capped = state.limit >= MAX_LIST_ROWS;
  const done = capped || (state.historyDone && state.messages.length < state.limit);
  $('list-footer').hidden = false;
  $('btn-load-more').hidden = done;
  $('list-note').hidden = !done;
  if (!done) return;
  // Say the cap out loud when that is what stopped the paging: at 1000 rows the bottom may well
  // still be cached and unrendered, and "已经到最早的一封了" would be a lie.
  $('list-note').textContent = capped
    ? `列表最多显示 ${MAX_LIST_ROWS} 封，更早的邮件请用搜索。`
    : '已经到最早的一封了。';
}
```

#### (g) 新增 `refreshSyncStatus()`

**插在**下面这一行之前（它紧跟在 `renderListFooter` 之后）：

```js
/** Search results, each tagged with the folder it came from. */
```

```js
/**
 * Show why the mail on screen may be stale, or hide the notice when it is not.
 *
 * `sync_status` reports the last refresh failure the backend saw plus how much is cached. A failure
 * of this call itself is deliberately silent — `run` would toast about the *notice*, which is noise
 * on top of the failure that produced it.
 */
async function refreshSyncStatus() {
  const note = $('offline-note');
  if (!state.accountId) {
    note.hidden = true;
    return;
  }

  const status = await invoke('sync_status', { accountId: state.accountId }).catch(() => null);
  if (!status || !status.offline) {
    note.hidden = true;
    note.textContent = '';
    return;
  }

  note.hidden = false;
  // Not "you are offline": the recorded failure can equally be a rejected credential or a folder
  // the server no longer has. What is certain is that this list is cached mail and the last refresh
  // failed, so that is what it says, followed by the reason.
  note.textContent = `显示的是本地缓存（共 ${status.cached_total} 封）。上次与服务器同步失败：${errText(
    status.last_error
  )}`;
}
```

#### (h) `refreshAccounts()` —— 没有账号时也要把离线提示收掉

**替换前**
```js
  if (state.accounts.length === 0) {
    list.innerHTML = '<div class="empty">还没有账号，点击下方按钮添加。</div>';
    state.accountId = null;
    state.mailboxes = [];
    state.messages = [];
    renderMailboxes();
    renderMessages();
    return;
  }
```
**替换后**
```js
  if (state.accounts.length === 0) {
    list.innerHTML = '<div class="empty">还没有账号，点击下方按钮添加。</div>';
    state.accountId = null;
    state.mailboxes = [];
    state.messages = [];
    state.historyDone = false;
    renderMailboxes();
    renderMessages();
    // Removing the last account must take its offline notice with it: the banner is about an
    // account's cache, and there is no longer an account.
    await refreshSyncStatus();
    return;
  }
```

#### (i) `openHit()` —— 跳到的文件夹同样要重置

**替换前**
```js
  if (hit.folder !== state.mailbox) {
    state.mailbox = hit.folder;
    state.mailboxLabel = folderLabel(hit.folder);
    $('mailbox-title').textContent = state.mailboxLabel;
    renderMailboxes();
  }
```
**替换后**
```js
  if (hit.folder !== state.mailbox) {
    state.mailbox = hit.folder;
    state.mailboxLabel = folderLabel(hit.folder);
    // Same reason as `selectMailbox`: this is a different folder with its own history.
    state.historyDone = false;
    $('mailbox-title').textContent = state.mailboxLabel;
    renderMailboxes();
  }
```

---

## 8. `tools/check.ps1` —— 新增一项门禁

**插在**下面这一行之前（`check.ps1` 的 rust 段开头）：
```powershell
    # ------------------------------------------------------------ rust
```

```powershell
    Step 'history backfill cannot move the sync cursor' {
        # Backfill walks backwards (from the oldest cached UID down); incremental sync walks forwards
        # (`last_uid` up). They share one cache and must never share a cursor: a backfill that moved
        # `last_uid` would either make the next sync re-read the whole folder or, worse, skip the
        # mail that arrived while it was running. This reads the source rather than relying on a
        # test, because the failure mode is "someone adds one line to the wrong driver".
        #
        # Everything from `#[cfg(test)]` down is exempt: the unit test has to *set* a cursor in order
        # to prove a backfill leaves it alone.
        $backfill = Get-Content 'crates/mail-core/src/backfill.rs' -Raw
        $code = ($backfill -split '(?m)^#\[cfg\(test\)\]')[0]
        $cache = Get-Content 'crates/mail-core/src/cache.rs' -Raw
        $lib = Get-Content 'src-tauri/src/lib.rs' -Raw
        $js = Get-Content 'src/app.js' -Raw
        $problems = @()

        if ($code -match '\.set_cursor\(') {
            $problems += 'backfill.rs calls Cache::set_cursor: a backfill must never move the incremental cursor'
        }
        if ($cache -notmatch 'update folders set uidvalidity = 0,[\s\S]{0,120}?history_complete = 0') {
            $problems += 'cache.rs: invalidate_folder must reset the cursor and history_complete in the same update, or a renumbered folder stays "fully cached" and its older mail becomes unreachable'
        }

        # Paging means "ask for a bigger page on each press", so the ceiling has to agree on both
        # sides of the IPC boundary: too high in the renderer and the button silently does nothing,
        # too low in the backend and it refuses a page the renderer asked for.
        $rustCap = [regex]::Match($lib, 'const MAX_LIST_ROWS: usize = (\d+);').Groups[1].Value
        $jsCap = [regex]::Match($js, 'const MAX_LIST_ROWS = (\d+);').Groups[1].Value
        if (-not $rustCap -or $rustCap -ne $jsCap) {
            $problems += "MAX_LIST_ROWS must be declared in both src-tauri/src/lib.rs and src/app.js with the same value (rust='$rustCap', js='$jsCap')"
        }

        if ($problems.Count -gt 0) {
            $problems | ForEach-Object { Write-Host $_ -ForegroundColor Red }
            $global:LASTEXITCODE = 1
            return
        }
        Write-Host "backfill cannot touch the cursor; page cap = $rustCap on both sides"
        $global:LASTEXITCODE = 0
    }
```

---

## 9. `crates/mail-core/tests/loopback.rs`

### 9.1 import

**替换前**
```rust
use mail_core::{
    AccountConfig, Cache, ConnectionPool, Endpoint, ImapClient, MailboxInfo, OutgoingAttachment,
    OutgoingMail, PushEvent, Security, SyncDriver, SyncOutcome, pool_op, watch_every,
};
```
**替换后**
```rust
use mail_core::{
    AccountConfig, BackfillDriver, BACKFILL_BATCH, Cache, ConnectionPool, Endpoint, ImapClient,
    MailboxInfo, OutgoingAttachment, OutgoingMail, PushEvent, Security, SyncDriver, SyncOutcome,
    pool_op, watch_every,
};
```

> 顺序按 rustfmt 的习惯写法即可（大写在前、小写在后）；`cargo fmt --all` 会兜底。

### 9.2 夹具：可前置任意多封旧邮件

**替换前**
```rust
fn all_messages() -> Vec<Vec<u8>> {
    vec![
        message_one(),
        message_two(),
        message_three(),
        message_four(),
    ]
}
```
**替换后**
```rust
/// Synthetic older mail, so a fixture can hold more than one sync window.
///
/// `history` messages are *prepended*, so they take the lowest UIDs (`1..=history`) and the four
/// hand-written fixtures keep their relative order above them. Subjects are numbered, so a test can
/// say which window it got. Without this every fixture fits in the first window and a backfill would
/// have nothing to do — which is exactly the state the feature exists to leave behind.
fn history_messages(history: usize) -> Vec<Vec<u8>> {
    (0..history)
        .map(|n| {
            format!(
                "From: History <history@example.com>\r\n\
                 To: me@example.com\r\n\
                 Subject: 历史邮件 {n:04}\r\n\
                 Date: Mon, 01 Jan 2018 08:00:00 +0800\r\n\
                 Message-ID: <history-{n}@example.com>\r\n\
                 Content-Type: text/plain; charset=utf-8\r\n\
                 \r\n\
                 这是一封更早的邮件。\r\n"
            )
            .into_bytes()
        })
        .collect()
}

fn all_messages(history: usize) -> Vec<Vec<u8>> {
    let mut messages = history_messages(history);
    messages.extend([
        message_one(),
        message_two(),
        message_three(),
        message_four(),
    ]);
    messages
}
```

### 9.3 `Server` 增加一个旋钮

**替换前**
```rust
struct Server {
    /// Advertise and accept `UID EXPUNGE`, covering both delete strategies.
    uidplus: bool,
```
**替换后**
```rust
struct Server {
    /// Advertise and accept `UID EXPUNGE`, covering both delete strategies.
    uidplus: bool,
    /// Older messages prepended to the fixture, i.e. how much mail there is below the first sync
    /// window. 0 keeps every existing test's UID numbering.
    history: usize,
```

**替换前**
```rust
        Self {
            uidplus: false,
            uidvalidity: Arc::new(AtomicU32::new(1)),
```
**替换后**
```rust
        Self {
            uidplus: false,
            history: 0,
            uidvalidity: Arc::new(AtomicU32::new(1)),
```

### 9.4 把 `history` 传进 `serve_imap`

`spawn_server` 里，在 `let extra = Arc::clone(&server.extra);` 之后加一行：
```rust
            let history = server.history;
```
并在 `serve_imap(...)` 的实参列表末尾（`extra,` 之后）加上 `history,`。

`serve_imap` 的签名，**替换前**
```rust
async fn serve_imap(
    stream: TcpStream,
    uidplus: bool,
    log: Transcript,
    uidvalidity: Arc<AtomicU32>,
    close_on_noop: Arc<AtomicBool>,
    idle: Arc<Mutex<Idle>>,
    extra: Arc<AtomicU32>,
) -> std::io::Result<()> {
```
**替换后**（注意那个 `#[allow]`：加了 `history` 就是 8 个参数，clippy 的 `too_many_arguments`
阈值是 7，而门禁带 `-D warnings`；`imap.rs` 里那个 9 参数的测试辅助函数已经有同样的 `#[allow]`）
```rust
#[allow(clippy::too_many_arguments)]
async fn serve_imap(
    stream: TcpStream,
    uidplus: bool,
    log: Transcript,
    uidvalidity: Arc<AtomicU32>,
    close_on_noop: Arc<AtomicBool>,
    idle: Arc<Mutex<Idle>>,
    extra: Arc<AtomicU32>,
    history: usize,
) -> std::io::Result<()> {
```

**替换前**
```rust
    let messages = all_messages();
```
**替换后**
```rust
    let messages = all_messages(history);
```

> 顺带说明（**不要改**）：夹具把 `uid == 1` 的邮件标成 `\Seen`。有了 `history`，UID 1 会变成
> 第一封合成旧邮件 —— 只有新用例用 `history > 0`，所以既有断言不受影响。

### 9.5 新用例

**插在**下面这一行之前（即 `syncing_a_folder_twice_only_fetches_what_is_new` 之后）：
```rust
#[tokio::test]
async fn a_uidvalidity_change_wipes_the_folder_and_refills_it() {
```

```rust
#[tokio::test]
async fn loading_more_history_pages_backwards_without_moving_the_cursor() {
    // More mail than one sync window, which is the situation the feature exists for.
    const HISTORY: usize = 600;
    let (addr, _log) = spawn_server(Server {
        uidplus: true,
        history: HISTORY,
        ..Server::default()
    })
    .await;
    let cfg = account(addr, addr);
    let mut client = within(ImapClient::connect(&cfg, "app-password"))
        .await
        .expect("connect");
    let cache = Cache::open_in_memory().expect("cache");

    // First sync: the newest 500 of 604, i.e. UIDs 105..=604.
    let total = HISTORY as u32 + 4;
    let first = within(sync_once(&mut client, &cache, &cfg.id, &inbox())).await;
    assert_eq!(first.fetched, 500, "a cold cache takes one window, not the folder");
    assert_eq!(first.last_uid, total);
    assert_eq!(cache.oldest_uid(&cfg.id, "INBOX").unwrap(), Some(total - 499));
    // UID 1 is *below* the first window, so it is neither listed nor searchable yet. Asserting on
    // the newest subject alone would prove nothing: UIDs 105..600 are history mail too.
    assert!(
        cache
            .search(&cfg.id, "历史邮件 0000", 10)
            .expect("search")
            .is_empty(),
        "the oldest mail is not cached yet, so it cannot be found yet"
    );

    // "Load more": the page is short, so the driver walks below the oldest cached UID.
    let mut driver = BackfillDriver::start(&cache, &cfg.id, &inbox())
        .expect("plan")
        .expect("a backfill is possible");
    assert_eq!(driver.before_uid(), total - 499);

    let page = within(driver.execute(&mut client)).await.expect("fetch");
    assert_eq!(page.uidvalidity, 1, "the fixture's generation");
    assert_eq!(page.summaries.len(), 104, "everything below the window");
    assert!(
        page.exhausted,
        "and that is the bottom: UID 1 is in there, so no further page can be non-empty"
    );

    let outcome = driver.absorb(&cache, &page).unwrap();
    assert_eq!(outcome.fetched, 104);
    assert_eq!(outcome.total_cached, total, "the whole folder is cached now");
    assert!(!outcome.renumbered);
    assert_eq!(
        cache
            .folder_state(&cfg.id, "INBOX")
            .unwrap()
            .map(|state| state.last_uid),
        Some(total),
        "a backfill walks backwards: 604 is still where the next sync resumes"
    );

    // The point of the exercise: the oldest mail is now listed and searchable.
    let listed = cache
        .list_summaries(&cfg.id, "INBOX", total, 0)
        .expect("list");
    assert_eq!(listed.len(), total as usize);
    assert!(listed.iter().any(|m| m.subject == "历史邮件 0000"));
    let hits = cache.search(&cfg.id, "历史邮件 0000", 10).expect("search");
    assert_eq!(
        hits.len(),
        1,
        "backfilled headers must reach the local index, or the feature only half works"
    );
    assert_eq!(hits[0].uid, 1, "and it is the oldest message in the folder");

    // And the folder is remembered as complete, so the next press costs no round trip.
    assert!(
        BackfillDriver::start(&cache, &cfg.id, &inbox())
            .expect("plan")
            .is_none(),
        "reaching the bottom must be persisted, not re-derived"
    );

    // The incremental sync is still incremental: the cursor never moved.
    let after = within(sync_once(&mut client, &cache, &cfg.id, &inbox())).await;
    assert_eq!(after.fetched, 0, "nothing new arrived");
    assert_eq!(after.total_cached, total, "and nothing was re-inserted");
}
```

### 9.6 第二个用例：一页装不下的历史

**插在**和上一节**同一行**之前（即 §9.5 那个用例的末尾之后，`a_uidvalidity_change...` 之前）：

```rust
#[tokio::test]
async fn a_uidvalidity_change_wipes_the_folder_and_refills_it() {
```

```rust
#[tokio::test]
async fn backfill_walks_down_through_several_batches() {
    // 904 messages: 900 synthetic + the 4 hand-written ones. The first window is UIDs 405..=904.
    const HISTORY: usize = 900;
    let (addr, _log) = spawn_server(Server {
        uidplus: true,
        history: HISTORY,
        ..Server::default()
    })
    .await;
    let cfg = account(addr, addr);
    let mut client = within(ImapClient::connect(&cfg, "app-password"))
        .await
        .expect("connect");
    let cache = Cache::open_in_memory().expect("cache");

    let total = HISTORY as u32 + 4;
    let first = within(sync_once(&mut client, &cache, &cfg.id, &inbox())).await;
    assert_eq!(first.fetched, 500);
    assert_eq!(first.last_uid, total);
    let floor = total - 499;

    // Page 1: a full batch below the floor, and *not* the bottom.
    let mut driver = BackfillDriver::start(&cache, &cfg.id, &inbox())
        .expect("plan")
        .expect("page 1");
    assert_eq!(driver.before_uid(), floor);
    let page = within(driver.execute(&mut client)).await.expect("fetch");
    assert_eq!(page.summaries.len(), BACKFILL_BATCH);
    assert!(
        !page.exhausted,
        "204 older messages are still below this page"
    );
    assert_eq!(page.summaries.first().map(|m| m.uid), Some(floor - 200));
    assert_eq!(page.summaries.last().map(|m| m.uid), Some(floor - 1));
    let outcome = driver.absorb(&cache, &page).unwrap();
    assert_eq!(outcome.fetched, BACKFILL_BATCH);
    assert!(!outcome.exhausted);
    assert_eq!(cache.summary_count(&cfg.id, "INBOX").unwrap(), 700);
    assert_eq!(cache.oldest_uid(&cfg.id, "INBOX").unwrap(), Some(floor - 200));

    // Page 2: another full batch. Still not the bottom — "exhausted" must only flip when it is true,
    // or the UI would stop offering pages with mail still unreachable behind it.
    let mut driver = BackfillDriver::start(&cache, &cfg.id, &inbox())
        .expect("plan")
        .expect("page 2");
    assert_eq!(driver.before_uid(), floor - 200);
    let page = within(driver.execute(&mut client)).await.expect("fetch");
    assert_eq!(page.summaries.len(), BACKFILL_BATCH);
    assert!(!page.exhausted);
    driver.absorb(&cache, &page).unwrap();
    assert_eq!(cache.summary_count(&cfg.id, "INBOX").unwrap(), 900);
    assert_eq!(cache.oldest_uid(&cfg.id, "INBOX").unwrap(), Some(5));

    // Page 3: four messages left, so this is a short page and the bottom at the same time.
    let mut driver = BackfillDriver::start(&cache, &cfg.id, &inbox())
        .expect("plan")
        .expect("page 3");
    assert_eq!(driver.before_uid(), 5);
    let page = within(driver.execute(&mut client)).await.expect("fetch");
    assert_eq!(page.summaries.len(), 4);
    assert!(page.exhausted, "UID 1 is the oldest a folder can hold");
    let outcome = driver.absorb(&cache, &page).unwrap();
    assert!(outcome.exhausted);
    assert_eq!(cache.summary_count(&cfg.id, "INBOX").unwrap(), total);

    // The whole folder is reachable, the bottom is remembered, and the cursor still never moved.
    assert_eq!(
        cache
            .list_summaries(&cfg.id, "INBOX", total, 0)
            .expect("list")
            .len(),
        total as usize
    );
    assert!(
        BackfillDriver::start(&cache, &cfg.id, &inbox())
            .expect("plan")
            .is_none(),
        "the third page marked the folder complete"
    );
    assert_eq!(
        cache
            .folder_state(&cfg.id, "INBOX")
            .unwrap()
            .map(|state| state.last_uid),
        Some(total)
    );
}
```

---

## 10. 文档（README.md / HANDOFF.md）

> **只写实测到的东西。** 第 12 节里没做的验证，一律写成"没验过"。

### 10.1 `README.md`

1. 进度表里这一行
   `| 历史邮件回填（缓存最新 500 封以外的部分） | ❌ 下一步 |`
   改成
   `| **历史邮件回填**：缓存不够时向服务器取更早的邮件 | ✅ 每次 200 封，增量游标不动 |`

2. 在「## 连接池与推送（v0.2）」之后、「## 附件下载（v0.2）」之前插入一节：

```markdown
## 历史回填（v0.2）

首次同步只取最新 500 封（两万封的文件夹不能让首屏不可用）。更早的邮件由「加载更多」按需取回，
**每次 200 封**，走的是与增量同步完全相反的方向。

* **回填绝不移动 `last_uid` 游标。** 游标只描述"新的那一头"，回填在旧的那一头工作。
  两者共用一个数据库但不共用游标：移动游标会让下一次同步要么重读整个文件夹，
  要么跳过回填期间到达的新邮件。所以它是一个独立的 driver（`backfill.rs`），
  `tools/check.ps1` 有一项直接拦"回填里出现 `Cache::set_cursor`"。
* **往哪翻是算出来的，不是存下来的。** 起点永远是缓存里最小的那个 UID（`select min(uid)`），
  删掉一封邮件不会留下指向它的指针。
* **`UID FETCH` 表达不了"更早"。** 所以先 `UID SEARCH ALL` 取到 UID 列表，再从里面切窗口。
  这同时让"到底了没有"变成精确判断（剩下的旧 UID 不够一页即到底），而不是靠"这次取回来几封"猜 ——
  服务器完全可能在 search 与 fetch 之间 expunge 掉一封。
* **取件范围必须写成 `a:b` 这种闭区间，不能写成 `<floor>:*`。** RFC 3501 规定区间无序，
  起点超过文件夹最大 UID 的区间会解析成**最后一封**，那会让回填永远重复取最新那几封。
* **"到底了"要持久化**（`folders.history_complete`），否则每按一次"加载更多"都要再问一次服务器。
  这个标记的含义严格限定为"服务器上已经没有被缓存的 UID 更早的邮件了"（依据是 `UID SEARCH` 的结果），
  **不是**"每一封都已经落库"—— 它会让后续不再尝试，所以只能声明真正被证实的那件事。
  写入时用 `uidvalidity` 做栅栏：回填途中服务器重新编号，这一页连同标记一起作废
  （`invalidate_folder` 也会清零它）。`SCHEMA_VERSION` 因此升到 2，迁移是 `alter table add column`
  ——**不是重建表**，用户已经缓存的邮件不会因为加一个整数列而消失。
* **"加载更多"现在是先本地后网络**：页面还在缓存的窗口里就是纯本地读，一次往返都不花；
  只有缓存填不满这一页时才联网取更早的。上限 1000 行（`MAX_LIST_ROWS`，前后端各一份常量，
  `check.ps1` 要求两侧一致）。
```

3. 「## 已知限制 / 下一步」里，把
   `- **缓存只覆盖最新 500 封。** 更早的历史没有回填机制，"加载更多"超过这个范围就没有了。`
   改成
   `- **首次同步只覆盖最新 500 封**，更早的由「加载更多」按需回填（每次 200 封，需要联网）。`
   把下面这两行（README 里是**一条**跨两行的列表项）
   `` - **离线时不会显式提示"离线"**：`sync_status` 命令已经能报告 `offline` 与失败原因， ``
   ``   但界面还没用它，所以离线时看到的是上次同步的数据、没有标注。 ``
   改成
   `- **列表上方会说明"显示的是本地缓存"以及上次同步失败的原因**（数据来自 `sync_status`），`
   `  不会再让你把陈旧数据当新鲜的看。`
   > 具体措辞按实现实际观感写，但必须提到"提示里带原因"。

4. **顺手改掉两处早就过期的数字**（它们本来就错，而这个计划让它们更错）：
   - 「目录结构」里的 `src/lib.rs               14 个 Tauri 命令 + 状态管理`
     → `21 个 Tauri 命令 + 状态管理`（本计划新增 `load_more_messages`；动手前实际是 20 个。
     **改完用 `Select-String -Path src-tauri/src/lib.rs -Pattern '^\s+\w+,$'` 之类数一遍确认**）。
   - 「两层验证」表里 `有 8 个用例` → 按实测的 loopback 用例数写（本计划之后是 24）。

### 10.2 `HANDOFF.md`

1. **两处**测试数字都要改（现在这两处自己就不一致）：§2 的
   `目前 **111 passed**（3 + 83 + 21 + 4）` 与 §6 的 `**已验证**：**104 个测试**`。
   **实测后**写成当次 `cargo test --workspace` 的真实总数与分解
   （动手前基线是 **116**：3 + 87 + 22 + 4；本计划新增 **13** 个用例 → 预期 **129**：3 + 98 + 24 + 4）。
2. §2 的门禁清单里补上新那一项的名字（`history backfill cannot move the sync cursor`）。
3. §5 决策记录追加：

```markdown
- **回填与增量同步是两个方向，绝不共用游标。** 回填（`backfill.rs`）往下走、增量（`sync.rs`）
  往上走，共用一个 `cache.db`。`check.ps1` 直接检查 `backfill.rs` 的非测试部分里不出现
  `Cache::set_cursor`——这类 bug 的表现是"下一次同步漏掉一批邮件"，测试很难稳定地抓到。
  `OlderPage.exhausted` 必须来自 `UID SEARCH` 的结果（剩下的旧 UID 不够一页），不能来自
  "这次取回来几封"：服务器可以在 search 与 fetch 之间 expunge。
  游标区间必须写 `a:b`，`<floor>:*` 在 RFC 3501 下会解析成最后一封。
- **`folders.history_complete` 是"这一代 UID 已经全部缓存"的断言**，所以：
  `invalidate_folder` 必须清零它；写入时用 `uidvalidity` 做栅栏（`update ... and uidvalidity = ?`）；
  `upsert_folder` 的 conflict 子句里**不能**出现它（每次同步都会 upsert，加了就等于每次都忘）。
  `cache.rs` 三个单测分别钉住这三条。
- **`SCHEMA_VERSION` 1→2 的迁移是 `alter table add column`**，不是重建表：缓存是用户的，
  为了加一个整数列把它清掉与丢数据无异。`alter table` 没有 `if not exists`，所以
  `migrate()` 先用 `pragma table_info` 检查（否则中断一次就永远打不开缓存）。
- **增量同步的截断必须保留"最旧的一批"（`keep_oldest_page`）。** 游标推进到"已存的最大 UID"
  之后意味着"这以下都取过了"，所以保留最新的一批会把游标推过从未落库的那一段，
  而增量只往上走、回填只往最小 UID 之下走，两边都够不到那个洞 —— 静默且永久。
  代价只是一次进来 500 封以上时，最新几封要等下一次同步。
- **分页模型仍是"每次按多要 50 行"**，前后端各有一个 `MAX_LIST_ROWS = 1000` 常量，
  `check.ps1` 要求两侧一致：前端太大 → 按钮点了没反应；后端太小 → 后端拒绝前端要的那一页。
  真要做无限滚动就得换成 offset 分页，那是另一个量级的改动。
```

3. §7 ① 的"剩下的一小块"改成已完成，并把 §6 的验证状态如实更新（见下条）。
4. §6 「还没验证的」追加一条：

```markdown
4. **历史回填只在假服务器（loopback）上验过**（604 封与 904 封两个夹具，覆盖"一页到底"和
   "连翻三页"两条路径）。真实 QQ 收件箱按 README 的记录是 149 封（外加两封自测邮件），
   比首次同步的 500 封窗口还小，所以真机上只能验到"到底了"那条路径
   （`回填 INBOX：新增 0 封，缓存共 N 封（已到最早一封）`），**"真的取回更早的邮件"没在真实账号上验过**。
   （若下面的可选步骤真的做了，就照实测的日志行改写这一段并注明日期。）
```

---

## 11. 执行顺序（照着做，每步都有验收）

1. **备份真机缓存**（迁移会动它）：
   ```powershell
   Copy-Item "$env:APPDATA\com.easyemail.desktop\cache.db" "$env:APPDATA\com.easyemail.desktop\cache.db.v1.bak" -Force
   ```
2. 按第 2 节改 `cache.rs` → `cargo test -p mail-core cache -- --nocapture`
   预期：新增 4 个用例通过，既有用例不红。
3. 按第 3、4、5 节加 `imap.rs` / `backfill.rs` / `lib.rs` / `sync.rs` 注释，
   以及 §5.3 那个 `keep_oldest_page` 修复 + 单测。
4. `cargo test -p mail-core` 预期：全绿（含新 6 个 backfill、1 个 `keep_oldest_page` 用例）。
5. 按第 9 节改 loopback 夹具 + 两个用例，单跑：
   ```powershell
   cargo test -p mail-core --test loopback backfill -- --nocapture
   ```
   预期：`2 passed`。若失败，先看是 `page.summaries.len()` 还是 `last_uid` 那一行 ——
   数字对不上说明窗口算错了，`last_uid` 变了说明回填碰了游标（**这是最严重的一种，必须停下来查**）。
6. 按第 6 节改 `src-tauri/src/lib.rs`。`cargo build --workspace` 预期无错无警告
   （**特别检查没有 `unused import: BackfillOutcome`** 这类警告）。
7. 按第 7 节改前端三个文件、按第 8 节改 `check.ps1`。
8. 全量门禁：
   ```powershell
   cargo fmt --all
   .\tools\check.ps1
   ```
   预期：最后一行 `All checks passed.`，并且能看到新那一项 `backfill cannot touch the cursor; page cap = 1000 on both sides`。
   数字预期：`cargo test` 四个二进制合计 **129**（3 + 98 + 24 + 4）。
9. 按第 10 节更新 `README.md` / `HANDOFF.md`（数字用第 8 步实测的），再跑一次 `.\tools\check.ps1`。
10. 删掉 `PLAN.md`。

---

## 12. 不需要动的东西（碰到就是改错了）

* **`sync.rs` 的任何逻辑**（`SyncDriver` / `Fetch` / `advance_cursor` / `absorb`）。回填是**新驱动**，
  不是 `Fetch` 的新分支。
* **`upsert_folder` 的 conflict 子句**：不许出现 `history_complete`。
* **`list_messages` 的语义**：它仍然"先刷新再读缓存"，只是上限变成 1000。
  不要让它变成纯缓存读（刷新按钮 + IDLE 推送依赖它）。
* **`Cache::open` 每次开一条短连接**这条规矩：新命令也照做，不要引入共享连接。
* **`pool_op!` 宏**：不要在 `backfill` 路径上换成"手写 acquire/retry"。
* **`buildSandboxedDocument()` / CSP / 宽度规则 / `to_tokens`**：与本任务无关，一个字都不要碰
  （`check.ps1` 有四项盯着它们）。
* **`src/theme.js`、`src-tauri/src/store.rs`、`secret.rs`、`providers.rs`、`folders.rs`、`mutf7.rs`**：不动。
* **`tauri.conf.json`**：不动（特别**不要**为了调试把 `--remote-debugging-port` 加回去还留在里面）。
* **不要给图片加 `max-height`、不要把 FTS5 tokenizer 换成 trigram、不要把 Gmail 可达性告警改成硬失败。**
* **不要在增量同步里"顺手"把 `history_complete` 标上。** 看起来可以：`Fetch::Newest` 取回来的条数
  少于 500 就说明整个文件夹都拿到了。但服务器完全可能在 `UID SEARCH` 与 `UID FETCH` 之间 expunge 掉几封，
  于是 `len() < limit` 会在"其实还有更早的邮件"时把文件夹标成已到底 —— 那是**永远取不回来**的邮件。
  `OlderPage.exhausted` 之所以要单独算（数 `SEARCH` 的结果，而不是数取回来几封），就是为了躲开这一类猜测。
  代价只是每个文件夹第一次按"加载更多"多花一次 `SEARCH`。
* **不要把 `keep_oldest_page` 改回"保留最新的一批"。** 那是丢邮件（见 §5.3）：游标表示
  "这以下都取过了"，保留最新的一批等于把游标推过从未落库的那一段。
* **不要把 `historyDone` 直接当成"按钮该隐藏"。** 它只说明"服务器上没有更早的了"；
  页面上还有多少行是另一回事（刷新会把页缩回 50 行）。`renderListFooter` 里那两个条件缺一不可。
* **不要试图"修"翻页时列表边界偶尔重排。** 列表按日期排序、回填按 UID 走，两者顺序本来就可以不一致
  （UID 小、日期新是合法的），所以跨越边界时可能有一行移位。这是分页模型固有的，不是回填的 bug。

---

## 13. 可选验收（真实账号，人工/GUI）

只有在**愿意花时间**时才做；不做就在 HANDOFF 里如实写"没验过"。
（这一步**只能人来做**：要真实账号、要真的点界面。别为了自动化把
`--remote-debugging-port` 加回 `tauri.conf.json` —— 加了必须删干净。）

1. 迁移前先记一笔（只读，不动数据）：
   ```powershell
   python -c "import sqlite3,os; p=os.path.join(os.environ['APPDATA'],'com.easyemail.desktop','cache.db'); c=sqlite3.connect('file:'+p+'?mode=ro',uri=True); print('user_version', c.execute('pragma user_version').fetchone()[0]); print('cols', [r[1] for r in c.execute('pragma table_info(folders)')]); print('messages', c.execute('select count(*) from messages').fetchone()[0])"
   ```
   预期（迁移前）：`user_version 1`，`cols` 里**没有** `history_complete`，`messages` 是当前缓存数量。
2. 构建并启动，日志重定向：
   ```powershell
   .\target\debug\easy-email.exe --console > target\app.log 2>&1
   ```
3. 在界面上点收件箱 → 连点「加载更多」直到出现"已经到最早的一封了。"。
   预期：
   - 前几次点是**瞬时的**（页面还在缓存窗口里，一次往返都不花）；
   - 最后一次会出现日志行（README 记的 QQ 收件箱是 149 封，所以"新增 0 封"）：
     ```
     [easy-email] 回填 INBOX：新增 0 封，缓存共 149 封（已到最早一封）
     ```
   - 然后「加载更多」按钮消失、出现那句提示；再点「刷新」（页缩回 50 行）后按钮**应该还在**，
     因为缓存里还有几百行没显示完 —— 这正是 §7.3(f) 那条规则在起作用。
4. 迁移结果核对（应用还开着也可以读，只读打开）：
   重复步骤 1 的命令，预期：`user_version 2`、`cols` 里含 `history_complete`、
   `messages` 数量与步骤 1 记录的一致。
   数量对不上就是迁移把缓存清掉了 —— **停下来**。
5. 关掉应用，确认 `cache.db.v1.bak` 可以删掉。
