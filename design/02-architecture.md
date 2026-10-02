# 02 架构、模块地图与接口契约

> **目标**：给出 M1→M3 全部功能都塞得进去、并且十年后还能改的骨架。读完本文应该能回答：
> 代码放哪、谁在什么线程上跑、数据怎么流、前端怎么调后端、错误怎么表达、性能红线是什么。
>
> **非目标**：本文不定义具体功能的行为细节（那是 03→18），也不写实现代码。

---

## 1. 目标与非目标

**目标**
- 支撑 M1 的三件硬需求：**10 万封不卡**、**全离线可用**、**多账号多协议**（IMAP/SMTP 起步，JMAP/Graph 可插拔）。
- 让 v0.2 已验过的资产（连接池、IDLE 泵、托盘、附件、缓存、`pool_op!` 的经验）**演进而非重写**。
- 把"容易再犯的错"变成结构上的不可能（例如：跨 `.await` 持有连接、每行一次事务、把库错误直接抛给用户）。

**非目标**
- 不引入需要长期运营的服务端（红线 6）。OAuth 应用注册、更新清单属于静态托管/用户自有凭据，不算服务端业务。
- 不做插件系统、脚本引擎（规则引擎内置即可）。
- 不追求跨平台三端（Windows 优先；macOS/Linux 的可行性在 §11 只作评估，不排期）。

## 2. 用户可感知行为（架构层面的承诺）

| 场景 | 用户看到 | 架构保证 |
|---|---|---|
| 冷启动 | ≤ 1.5 s 出现上次的邮件列表（10 万封） | UI 先读"列表首页 + 计数"，不等待任何网络；DB 读取走读池，不排队等写 |
| 切文件夹 | ≤ 200 ms 出列表 | 索引齐全的查询（`folder, date desc, uid desc`）+ 分页，不做全表扫描 |
| 滚动 | 60 fps，10 万封不顿 | 虚拟列表只渲染视口；IPC 只传当前页（50-200 条） |
| 搜索 | 输入即出结果（≤ 100 ms） | FTS5 独立于 UI 线程；索引写入批量提交 |
| 断网 | 列表/正文/搜索/写信/整理照常 | 所有读路径不碰网络；写操作先落本地队列（outbox / pending ops） |
| 收新信 | 秒级（受协议限制，见 04） | IDLE 泵只负责"发现变化"，同步由调度器限流执行 |
| 同步大文件夹 | 有进度、可暂停、可取消、可限速 | 同步是"可中断的任务"，不是一条 `async fn` 里跑到底 |
| 出错 | 一句中文说明 + 该怎么办 + 重试按钮 | `ApiError{code,message,hint,transient}`；前端按 code 决定 UI |

## 3. 模块地图（crate 内分层）

**决策**：M1 仍保持**单个 `mail-core` crate**（当前 ~4 千行，预计 M1 后 ~9 千行，Rust 完全吃得下），
但**强制模块分层**：依赖只能向下，不许反向，也不许跨层横向调用（`net` 不许碰 `db`）。
超过 ~2 万行或编译时间 > 60 s 时，再按下面注释的切分线拆 crate（切分线已经用 `┃` 标出）。

```
crates/mail-core/src/
  ├─ 0 基础层（无依赖）
  │   model.rs        共享可序列化类型（跨 IPC/DB/协议）
  │   error.rs        MailError 分类 + user_hint()（既有传统：错误必须可行动）
  │   i18n.rs         后端产生文案的 key（面向用户的中文串集中在这里，便于 en 化）
  │   mutf7.rs / folders.rs / providers.rs   既有工具与内置库
  │   secret.rs       Windows 凭据管理器封装（既有）
  ├─ 1 存储层  ┃ mail-store
  │   db/mod.rs       Db 句柄（写线程 + 读池，见 §4）
  │   db/schema.rs    全量 DDL（v3 起以迁移文件为准）
  │   db/migrate.rs   迁移框架（前向、单事务、失败回滚、迁移前备份）
  │   db/queries/*.rs 每张表的读写函数（消息/会话/文件夹/标签/草稿/队列/联系人/规则）
  │   db/fts.rs       FTS5 索引维护（增量、重建、优化）
  │   db/quota.rs     存储统计与清理策略
  │   db/heal.rs      自检与修复（完整性、游标校正、索引重建）
  ├─ 2 网络层  ┃ mail-net
  │   net/imap.rs     IMAP 客户端（既有演化：能力探测、CONDSTORE、THREAD、APPEND、MOVE）
  │   net/smtp.rs     SMTP（XOAUTH2、DSN、8BITMIME、SMTPUTF8、大附件流式）
  │   net/oauth.rs    OAuth2（PKCE、刷新、撤销）
  │   net/discover.rs 自动发现（ISPDB/autoconfig/SRV/Autodiscover）
  │   net/transport.rs TLS/代理/证书策略（native-tls + 代理）
  │   net/pool.rs     连接池（既有，扩展为按用途/账号配额）
  ├─ 3 引擎层  ┃ mail-engine
  │   sync/scheduler.rs  全局调度与预算（谁可以用几条连接、什么优先级）
  │   sync/job.rs        一次同步任务的状态机（可暂停/取消/续跑）
  │   sync/cursor.rs     游标与窗口策略（gap-free）
  │   sync/prefetch.rs   正文/附件预取队列
  │   sync/watch.rs      IDLE 泵与重连退避（既有 idle.rs 演化）
  │   outbox/worker.rs   发件队列（重试/退避/撤销窗口/定时）
  │   rules/engine.rs    规则匹配与动作执行（同步后触发）
  │   search/indexer.rs  索引作业（批量、可暂停、可重建）
  │   search/query.rs    查询语法解析 → SQL/FTS 表达式
  ├─ 4 业务层
  │   compose/*.rs   组信（纯文本/富文本/签名/模板/引用/附件流式）
  │   contacts/*.rs  通讯录与自动补全
  │   calendar/*.rs  （M3）.ics 解析、RSVP、CalDAV
  │   crypto/*.rs    （M3）PGP/S-MIME
  │   accounts/*.rs  账号生命周期、导入导出、迁移
  └─ 5 门面
      lib.rs          re-export + 高层用例（use case）函数，供 src-tauri 调用
src-tauri/src/
  lib.rs / commands/*.rs   薄命令层：参数校验 → 调 mail-core → 映射 ApiError
  tray.rs / notify.rs / update.rs / win_integration.rs / diagnostics.rs
web/  （前端：Vite + TS + Svelte 5，见 §7.1）
```

**分层规则（写进门禁）**：`net` 不得 `use crate::db`；`db` 不得 `use crate::net`；
`sync` 可以同时用两者；`src-tauri` 不写业务逻辑（只做参数校验、线程切换、序列化）。

## 4. 并发模型：谁在什么线程上跑

### 4.1 决策 A：数据库改为"单写线程 + 读池"（`Db` 句柄）

**为什么必须改**：现在的规矩是"每个操作开一条短连接、逐行 autocommit"（见 `cache.rs`）。
10 万封首同步 = 10 万次事务提交；正文索引 = 每封一次写。这在规模上直接不成立，
而"每条命令开一条连接"的写法又无法把一批写放进一个事务里。

**新形状**

```rust
/// **两个句柄的集合**（第二轮评审新增：原设计只说"`Db` 有两个句柄"，但没有任何类型承载它）。
/// 为什么不是一个 `Db` 挂两个库：①`ATTACH` 会把两个库绑在一条连接上，而我们需要
/// `user.db`=full、`cache.db`=normal 的**不同 pragma**（实测：ATTACH 下 `pragma user.synchronous`
/// 其实也能生效，所以这不是硬理由）；真正的理由是 ②备份/隔离/清理/迁移都要**按库独立操作**
/// （删缓存、隔离损坏库、分库 VACUUM），而 ③**没有任何写路径需要跨库原子性**（见 03 §11 反驳 7 的改定）。
#[derive(Clone)]
pub struct DbSet { pub cache: Db, pub user: Db }
impl DbSet {
    /// 两个库各自迁移；任一失败都不启动（保留旧库）。
    pub fn open(cache_path: &Path, user_path: &Path) -> Result<DbSet>;
    /// 按账号路由写句柄：`origin <> 'server'`（本机存档 / POP3）与用户意图表走 user。
    pub fn writer_for(&self, kind: AccountKind) -> &Db;
    /// 读句柄：搜索要同时读两个库（各自查询后按 (date,uid) 归并，不做跨库 JOIN）。
    pub fn readers(&self) -> [&Db; 2];
}

/// 单个库的句柄（写线程 + 读池）
#[derive(Clone)]
pub struct Db { /* Arc<Inner> */ }
impl Db {
    pub fn open(path: &Path, opts: DbOptions) -> Result<Db>;
    pub fn open_shared_memory(name: &str) -> Result<Db>;   // 仅单连接单测；并发测试用临时文件库（见 §4.1 失败表）
    pub async fn read<T, F>(&self, f: F) -> Result<T>
    where F: FnOnce(&Connection) -> Result<T> + Send + 'static, T: Send + 'static;
    pub async fn write<T, F>(&self, f: F) -> Result<T>
    where F: FnOnce(&Connection) -> Result<T> + Send + 'static, T: Send + 'static;
    pub async fn write_retry<T, F>(&self, f: F) -> Result<T>
    where F: Fn(&Connection) -> Result<T> + Send + Sync + 'static, T: Send + 'static;
    pub fn blocking_read<T>(&self, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T>;
    pub fn stats(&self) -> DbStats;
}

pub struct DbOptions {
    pub read_conns: usize,       // 默认 3（每库）
    pub busy_timeout_ms: u64,    // 默认 5000
    pub wal: bool,               // 生产 true；测试可 false
    pub slow_query_ms: u64,      // 默认 100
    pub synchronous: Sync,       // **按库设**：user.db=Full、cache.db=Normal（15 §11 反驳 3）
    pub encryption: Option<Encryption>,   // SEC-11 预留（M3 评估 SQLCipher）
}
/// **写线程的运行时（第二轮评审补，这个项目在这里被烧过）**：
/// 写线程是 **`std::thread` + `std::sync::mpsc`**，调用方用 `tokio::sync::oneshot` 收结果；
/// **`Db` 内部不得 `tokio::spawn`** —— 因为 `Db::open` 会在 `setup()` 里被调用，
/// 而那里没有 Tokio 运行时上下文（`HANDOFF.md` §3：`tokio::spawn` 会 panic，表现为"启动即消失"）。

```

**为什么闭包要求 `'static` 而不是像 `pool_op!` 那样被借用**（这是本设计里最容易误判的一点）：
`pool_op!` 之所以写成宏，是因为闭包必须**在原地**被 await 两次、借用调用者的局部变量
（见 `HANDOFF.md` §3）。而这里的闭包是**被 move 到另一个线程**上执行的，`'static + Send`
不是限制而是事实描述：所有参数必须自己带进去，所有结果必须自己带出来。
换句话说，**它不会被 `for<'a>` 那条约束咬到**，因为它根本不需要借用调用者的栈。

**失败与降级**
| 触发 | 行为 | 用户可见 |
|---|---|---|
| `SQLITE_BUSY`（写冲突） | `busy_timeout` 内重试；`write_retry` 额外重试 2 次（指数退避） | 通常无感；持续失败则提示"本地数据库正忙" |
| **`SQLITE_LOCKED`（第二轮评审补）** | **`busy_timeout` 不覆盖它**：共享缓存/内存库下"另一条连接正在写别的表"会报 `database table is locked`。生产用文件库 + WAL 不会出现；**测试用内存共享库时会出现** ⇒ 并发测试必须跑在临时文件库上（见 §10） | 无（仅测试环境） |
| 写线程 panic | actor 捕获 panic → 进程内重启写线程并重建连接；同时写 `diagnostics` | 提示"本地缓存需要修复"，并提供"重建缓存"按钮（03） |
| 读连接损坏（磁盘错误） | 该读连接丢弃重建；连续错误则标记库损坏 → 引导备份+重建 | 明确弹窗，不静默降级成空列表 |
| 迁移失败 | 中止启动迁移、保留旧库、提示可回滚 | "升级失败，已保留旧数据"（REL-07） |

**观测与预算**：写线程队列深度 > 200 或单次写 > 500 ms 记为异常（诊断包里出现）；
慢读（> 100 ms）计数进 `DbStats.slow`。

### 4.2 后台任务清单与预算

| 任务 | 数量 | 触发 | 预算（默认） | 可暂停 | 备注 |
|---|---|---|---|---|---|
| `idle_pump` | 每账号 1 条连接 | 账号启用 | 只做 IDLE + 重新 SELECT 比对；窗口 **120 s**（与 04 §5.4、D6 一致；及时性由 L1 的 `NewData` 负责） | 否 | 受 `max_connections` 限制，占 1 条 |
| `sync_job` | 每文件夹 ≤1，全局并发 ≤ `min(3, 账号配额-1)` | 手动/定时/IDLE 检测到变化 | 单文件夹 <= 2 条连接；页大小 500 头 | 是 | 大文件夹分片、可续跑 |
| `index_job` | 全局 1（按账号排队） | 新邮件落库后 | 每批 50 封正文；CPU 预算"低优先级" | 是 | 可限速（SYN-04） |
| `outbox_worker` | 全局 1 | 有排队邮件/网络恢复 | 每封一次尝试，退避 30s→1m→5m→15m→1h | 是 | 撤销窗口内的邮件不进发送 |
| `rules_runner` | 全局 1 | 同步落库后（增量） | 每封 < 5 ms，批量 200 | 是 | 只对新增邮件跑 |
| `notifier` | 全局 1 | 新邮件/规则命中 | 聚合 2 s 去重 | — | 免打扰策略在 13 |
| `update_check` | 1 | 启动后 + 每 6 h | 一次 HTTP | — | 可关；只在设置页显示 |
| `vacuum/optimize` | 1 | 空闲 + 每日一次 | 只在系统空闲时 | — | FTS optimize、DB 增量 vacuum |

**全局连接预算表**（防止把 QQ/163 惹毛）：每账号 `max_connections` 是硬上限，
`idle_pump` 占 1，其余给按需操作；同一账号同时最多 2 个网络作业（同步 + 发信除外）。

## 5. 数据流（主链路）

```
             ┌──────────── 前端 (WebView2) ────────────┐
             │  list/reader/compose/settings/...       │
             └───▲──────────────┬──────────────────────┘
        events   │              │ invoke(cmd, args)  (JSON)
                 │              ▼
             ┌───┴──────── src-tauri 命令层 ───────────┐
             │ 参数校验 → 调 mail-core 用例 → ApiError  │
             └───▲──────────────┬──────────────────────┘
                 │              ▼
   ┌─────────────┴─────── mail-core 门面（用例） ──────┴────────────┐
   │  accounts / sync / compose / search / rules / contacts        │
   └───┬───────────────┬────────────────┬─────────────────┬────────┘
       │               │                │                 │
       ▼               ▼                ▼                 ▼
   ┌───────┐      ┌─────────┐     ┌──────────┐     ┌────────────┐
   │  Db   │      │  net    │     │ indexer  │     │ outbox     │
   │ 写线程│      │ IMAP/SMTP│    │ (search) │     │ (send)     │
   │ +读池 │      │ /OAuth  │     └────┬─────┘     └─────┬──────┘
   └───┬───┘      └────┬────┘          │                 │
       │               │               │                 │
   cache.db       服务器            FTS5 表          服务器 SMTP
```

**关键约定**
- 网络层**只产出**数据结构（`MessageSummary`/`MessageDetail`/`FetchPage`…），不写库；
  写库只发生在引擎层，且必须批量化 + 事务化。
- 索引由"落库后的变更集"驱动（`pending_index` 表），不是同步流程里的副产品 —— 这样
  "同步被取消/中断"不会留下半索引，重启后索引队列自己接着跑。
- 前端**只通过命令与事件**与后端交互，没有共享内存、没有本地存储的真相（localStorage 只存 UI 偏好）。

## 6. 接口契约

### 6.1 错误：从 `String` 升级为 `ApiError`

```rust
#[derive(serde::Serialize, Debug, Clone)]
pub struct ApiError {
    pub code: ErrorCode,        // 稳定枚举，前端据此决定 UI（i18n key、是否显示重试）
    pub message: String,        // 面向用户的中文说明（已分类，绝不含库内部包装）
    pub hint: Option<String>,   // "该怎么办"
    pub transient: bool,        // 是否值得重试（前端显示"重试"按钮）
    pub context: serde_json::Value, // 机器可读上下文：account_id/uid/folder/http 状态等
}

#[derive(serde::Serialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    Auth, AuthExpired, Network, Timeout, Tls, Proxy,
    ServerRejected, RateLimited, NotFound, Conflict,
    Storage, StorageCorrupt, Migration, Index,
    Quota, AttachmentTooLarge, Unsupported,
    Cancelled, Offline, Internal,
}
```
- 每个 `code` 在前端有默认文案（i18n），`message` 用于覆盖/补充细节。
- 既有 `MailError::user_hint()` 保留，映射层负责 `MailError → ApiError`（一张表，可单测）。
- **门禁新增**：单元测试遍历 `ErrorCode` 全枚举，要求每个 code 都有非空前端文案（防止漏翻译）。

### 6.2 命令命名与形状

- 命名：`域_动作`（`message_page`、`draft_save`、`sync_folder_now`），与前端 `api/` 层一一对应。
- 列表一律分页：`{ offset: u32, limit: u32 } → { items: Vec<T>, total: u32 }`。
  **彻底废弃"把 limit 越加越大"的分页模型**（它导致 1000 行天花板的临时方案，见既有 PLAN）。
- 批量操作统一形状：`message_batch(ops: Vec<MessageOp>) → BatchReport { ok: u32, failed: Vec<(uid, ApiError)> }`。
- 长任务统一形状：`任务启动命令 → { job_id }`，进度走事件 `job:progress`，`job_cancel(job_id)`。
- 所有时间戳统一 `i64` Unix 秒（UTC）；所有 ID 统一字符串（账号、文件夹用线路名，消息用 `(account,folder,uid)` 三元组序列化）。

### 6.3 命令清单（分组；详细签名在各自文档）

| 域 | 命令 | 文档 |
|---|---|---|
| 服务商/发现 | `provider_list` `provider_suggest` `discover_probe` `account_build` | 05 |
| 账号 | `account_list` `account_get` `account_create` `account_update` `account_delete` `account_test` `account_pause` `account_resume` `account_reorder` `account_export` `account_import` | 05 |
| OAuth | `oauth_begin` `oauth_complete` `oauth_status` `oauth_sign_out` | 05 |
| 同步 | `sync_folder_now` `sync_account_now` `sync_status` `sync_pause` `sync_resume` `sync_cancel` `sync_set_window` `sync_more_history`（既有 `load_more_messages` 的正式化） | 04 |
| 文件夹 | `folder_list` `folder_refresh` `folder_create` `folder_rename` `folder_delete` `folder_empty_trash` `folder_stats` | 04,08 |
| 列表 | `message_page` `thread_page` `message_get` `message_get_raw` `message_neighbors` | 08 |
| 操作 | `message_mark_seen` `message_set_flagged` `message_add_label` `message_remove_label` `message_move` `message_copy` `message_delete` `message_archive` `message_snooze` `message_batch` | 08,10 |
| 搜索 | `search_query` `search_suggest` `search_index_status` `search_index_rebuild` `search_saved_list` `search_saved_save` `search_saved_delete` | 09 |
| 写信 | `draft_list` `draft_get` `draft_save` `draft_delete` `compose_send` `compose_schedule` `compose_undo` `compose_quote` | 07 |
| 附件 | `attachment_pick` `attachment_add_path` `attachment_save_as` `attachment_open` `attachment_preview` `attachment_remove` | 07,08 |
| 队列 | `outbox_list` `outbox_retry` `outbox_cancel` `outbox_send_now` | 07 |
| 身份 | `signature_list` `signature_save` `signature_delete` `template_list` `template_save` `template_delete` | 07 |
| 规则 | `rule_list` `rule_save` `rule_delete` `rule_reorder` `rule_test` `rule_run_now` `spam_mark` `spam_clear` `blocklist_add` `blocklist_remove` `unsubscribe_execute` | 10 |
| 联系人 | `contact_search` `contact_autocomplete` `contact_upsert` `contact_delete` `contact_import` `contact_export` `contact_groups_*` | 11 |
| 通知 | `notify_settings_get` `notify_settings_set` `notify_test` `notify_mute_until` | 13 |
| 设置 | `settings_get` `settings_set` `settings_reset` `storage_report` `storage_clean` `backup_create` `backup_restore` | 03,12 |
| 诊断 | `diagnostics_export` `log_tail` `log_open_folder` `db_heal` `db_stats` | 16 |
| 迁移 | `migrate_detect` `migrate_scan` `migrate_start` `migrate_cancel` `migrate_report` | 06 |
| 应用 | `app_version` `app_update_check` `app_update_install` `app_open_external` `app_reveal_path` | 17 |
| 日历(M3) | `calendar_events` `calendar_rsvp` `calendar_sync` | 18 |

### 6.4 事件清单

| 事件 | 载荷 | 何时 |
|---|---|---|
| `mail:new` | `{ account_id, folder, thread_id, new_count, unread }` | 同步发现新邮件（既有事件升级） |
| `mail:changed` | `{ account_id, folder, uids, change: seen/flag/moved/deleted }` | 本地或服务器旗标变化 |
| `sync:progress` | `{ account_id, folder, phase, done, total, rate }` | 同步/回填/索引进行中（节流 ≥ 200 ms） |
| `sync:error` | `{ account_id, folder, error: ApiError, will_retry_in_ms }` | 失败且会重试 |
| `job:progress` / `job:done` | `{ job_id, kind, done, total, result }` | 长任务（索引重建、迁移、导出、备份） |
| `outbox:changed` | `{ pending, failed, next_attempt_at }` | 队列变化（UI 显示"发件箱(2)"） |
| `index:status` | `{ indexed, pending, indexed_bytes }` | 索引状态变化（节流 1 s） |
| `update:available` | `{ version, notes_url, mandatory }` | 检查到新版本 |
| `settings:changed` | `{ keys }` | 设置被改（多窗口同步） |

**契约纪律**：事件只做"提示刷新"，**不携带真相**（真相永远通过命令读）。这样多窗口、
重连、事件丢失都不会让 UI 显示错误状态。

## 7. 前端架构与 P1 决策（技术栈）

### 7.1 决策：**② Vite + TypeScript + Svelte 5**（P1 已拍板，2026-10-03）

**用户给的两条硬要求**：**美观** + **极致性能**。按这两条打分，结论没有悬念：

| 方案 | 美观（设计系统可落地性） | 极致性能（10 万封列表 / 内存 / 包体） | 结论 |
|---|---|---|---|
| ① 无打包器（现状：原生 JS + 手写 CSS） | 手写 CSS 能好看，但令牌、主题、组件契约全靠纪律；i18n/无障碍手写 | **最轻**（零运行时），但 47 KB 单文件已到维护极限；没有类型 ⇒ IPC 契约靠人记 | ❌ 2 万行 UI 后不可维护 |
| ② **Vite + TS + Svelte 5** | **scoped CSS + CSS 变量令牌天然契合**（样式编译进组件、无运行时）；过渡/动画可编译掉；`svelte:element`/插槽够用 | **编译期生成细粒度更新代码，无虚拟 DOM**；运行时 ~10-15 KB；虚拟列表里"改一行只动一行"；主包可压到 ≤ 300 KB | ✅ **选它** |
| ③ Vite + TS + React | 生态最大（react-window、Ariakit） | 运行时 ~45 KB + 协调开销；10 万行列表要额外调优；内存占用更高 | ❌ 与"极致性能"相悖 |
| ④ Vite + TS + Solid | JSX 心智，性能与 Svelte 同级 | 体积/性能接近，**但**无障碍原语与生态更小，我们本来就要自己写虚拟列表 | 🟡 备选 |

**为什么是 Svelte 5 而不是 Solid（同为高性能）**：①**CSS 是它的第一等公民** —— `美观` 这条要求里
最贵的是"设计系统能不能被机械地守住"，Svelte 的 scoped style + 编译期样式让"令牌单一来源"几乎免费；
②`runes` 的细粒度响应式对"长列表 + 频繁局部更新"（未读点、旗标、同步状态）正是最省的那条路；
③打包器与生态（Vite + `svelte-check`）成熟，Tauri 模板现成；④体积更小。
**保留的退路**：若 M1a 中遇到 Svelte 生态硬缺口（例如某个无障碍原语），允许把**单个组件**换成自研
Web Component（`12` §5 的组件契约是框架无关的），不推翻整体选择。

**随之确定的三件事（避免实现时再讨论）**
1. **不用 UI 组件库**（Svelte Material/Flowbite 之类）：它们带自己的令牌与主题，会与 `12` 的令牌系统打架；
   我们只做"令牌 + 自研组件"，图表/图标也自绘（`12` §5 已按此写）。
2. **不用 CSS-in-JS**：样式走 `tokens.ts → CSS 变量` + 组件 scoped style，运行时零样式开销。
3. **不用 SPA 路由库**：只有 6 个界面，用状态机 + `{#if}` 切换（少一层依赖，也少一层内存）。

**代价（诚实记录）**：多一条 `npm run build` 与 `node_modules`；`check.ps1` 的 6 项前端检查必须改造
（**包括"必须自己跑前端构建"这一条，否则会绿着发出旧 UI**，见 `12` §12 与 `19` §7）。

> 历史选项表保留在下面（供将来复盘"当初为什么没选 React"），但**决策已定**，不再需要拍板。

| 方案 | 优势 | 代价 | 适配度 |
|---|---|---|---|
| ① 无打包器（现状：原生 JS + 手写 CSS） | 零构建、`cargo build` 一步、无 node_modules | 2 万行 UI 后不可维护；无类型；无障碍/虚拟列表/快捷键/i18n 全靠手写；重构风险高 | M2 之后明显吃力 |
| ② **Vite + TypeScript + Svelte 5**（**已选定**） | 编译期模板检查、细粒度响应式（虚拟列表性能好）、运行时极小（~10 KB）、CSS 原生支持、生态够用 | 多一条 `npm run build`；多 node_modules；需要在门禁里加前端构建 | **最平衡** |
| ③ Vite + TS + React | 生态最大（react-window、Ariakit 无障碍原语） | 运行时与内存开销更大，10 万行列表要额外调优 | 可接受 |
| ④ Vite + TS + Solid | 性能与体积接近 Svelte，JSX 心智 | 生态较小，无障碍原语要自己写 | 可接受（备选） |

**性能承诺（写进 15 §4 的预算，实现时照此验收）**：主包（gzip 前）≤ 300 KB；首屏骨架 ≤ 400 ms；
10 万封滚动 60 fps（每帧 DOM 变更 ≤ 40 行）；常驻内存 ≤ 300 MB；切换主题只改 `data-theme`（不重建 DOM）。


### 7.2 前端目录与分层（不依赖具体框架）

```
web/
  src/
    api/            invoke 的唯一出口（类型化命令、ApiError 映射、事件订阅）
    stores/         每个域一个 store：accounts / folders / list / reader / compose /
                    outbox / search / sync / contacts / settings / ui
    components/     纯展示组件（无命令调用）
    screens/        list / reader / compose / settings / wizard / search
    design/         tokens.ts（唯一色彩/间距来源）+ 基础样式
    i18n/           zh-CN.json / en.json + t()
    util/           日期、地址解析、键盘、剪贴板
  index.html
```
**硬规则**
- 组件不许直接 `invoke`（只能经 `api/`）：这样命令改名/加参数只改一处，且门禁能静态检查。
- 列表渲染必须走虚拟化组件（UI-04），禁止 `innerHTML = 全部邮件`。
- 所有面向用户的字符串走 `t()`；后端来的 `ApiError.code` 也映射到 `t()`。
- 主题只认 `data-theme` + CSS 变量（既有做法），不散落硬编码颜色。

### 7.3 从现状迁移的路径（降低 R3 风险）
1. **阶段 0**：搭 Vite 工程 + 设计系统 + 只读的"列表 + 阅读"两屏（旧 UI 继续可用，通过设置项切换）。
2. **阶段 1**：写信/草稿/发件箱迁到新 UI，旧的 `app.js` 里对应部分删除。
3. **阶段 2**：设置、向导、搜索、规则、联系人逐步迁完；旧 `src/app.js` 删除。
4. **门禁**：阶段 0 起，`check.ps1` 同时检查两套前端（`node --check` 旧的、`tsc --noEmit` 新的）。

## 8. 配置与状态文件

| 文件 | 内容 | 是否敏感 | 备注 |
|---|---|---|---|
| `%LOCALAPPDATA%\com.easyemail.desktop\`（**第二轮评审改定：本地数据不放 Roaming**） | `cache.db`、`user.db`、`logs\`、`updates\`、`vault.bin`、`avatars\`、`startup-error.log` | 部分 | **理由**：域环境里 Roaming 配置在登录/注销时会被复制（GB 级邮件缓存会拖垮登录，且可能撞配额）；DPAPI 令牌随漫游到服务器也与"换机器不可解"的说法矛盾 |
| `%APPDATA%\com.easyemail.desktop\`（Roaming，保留） | `settings.json`、`accounts.json` | 否 | 小文件、跨机器可带走；**从 v0.2 迁移**：首次启动时若在旧路径发现 `cache.db`/`accounts.json`，按需搬移并在日志里记一次 |
| Windows 凭据管理器 | 密码/授权码 | 是 | 既有 |
| 更新包 | `%LOCALAPPDATA%\...\updates\` | 否 | 安装后清理 |

**schema_version 三条独立版本线**：`settings_version`、`db_schema_version`、`app_version`。
升级顺序：备份 → 迁移 DB → 迁移 settings → 启动；任一步失败 → 回滚到旧库并提示（REL-07）。

## 9. 性能预算（全局，各文档不得超支）

| 指标 | 预算 | 测量方式 |
|---|---|---|
| 冷启动到列表可见 | ≤ 1.5 s（10 万封、已缓存） | 启动打点 `t_first_paint`（写日志 + 诊断包） |
| 切文件夹 | ≤ 200 ms（10 万封） | 同上，`t_folder_switch` |
| 列表滚动 | 60 fps（视口内 ≤ 40 行 DOM） | 手工 + 长列表脚本 |
| 本地搜索 | ≤ 100 ms（10 万封，含正文索引） | `DbStats.slow` + 基准脚本 |
| 内存常驻 | ≤ 300 MB（10 万封、3 账号） | 任务管理器 + 日志 |
| 空闲 CPU | ≤ 0.5%（3 账号 IDLE） | 任务管理器 |
| 单次 IPC | ≤ 200 KB 载荷、≤ 50 ms 序列化 | 前端打点 |
| 首同步吞吐 | ≥ 300 封/秒（头部，局域网/宽带） | 同步日志 |
| 索引吞吐 | ≥ 20 封/秒（正文解析 + 索引，单核） | 索引日志 |

## 10. 测试与门禁

**测试分层**（详细矩阵见 16）
| 层 | 工具 | 覆盖 |
|---|---|---|
| 单元 | `cargo test` | 解析、分词、规则匹配、迁移、错误映射、纯函数 |
| 假服务器 | 既有 `tests/loopback.rs` + 新增 `tests/loopback_index.rs` | 协议交互、断线、限流、取消、事务原子性 |
| 真机端点 | 既有 `provider_reachability`（扩展） | 内置库的正确性、能力探测 |
| 性能回归 | `tools/bench.ps1`（新） | 冷启动/搜索/滚动/索引吞吐，阈值写死并记录历史 |
| UI 冒烟 | CDP 探针（既有 `tools/inspect-dom.mjs`）+ 新 `tools/smoke.mjs` | 关键路径：启动→列表→读信→写信→发送→搜索 |
| 迁移 | `tests/migrate.rs` | 从 v1/v2 库升级、失败回滚、损坏库自愈 |

**`check.ps1` 新增项（本文负责的部分）**
1. 分层检查：`net/*.rs` 不含 `use crate::db`，`db/*.rs` 不含 `use crate::net`（源码正则，便宜有效）。
2. 命令契约：前端 `api/` 里出现的每个命令名，必须在 `src-tauri` 注册（沿用既有检查，改数据源）。
3. 错误映射：`ErrorCode` 全枚举都有 i18n 文案（跑一个单测即可，门禁调用它）。
4. 事件契约：`Emitter` 的事件名集合 ⊆ 文档 §6.4 清单（防止随手加事件无人订阅）。
5. 前端构建：`tsc --noEmit`（或 `svelte-check`）通过。

## 11. 逆向反驳（红队）

**反驳 1：`Db` 句柄要求闭包 `'static`，会把代码写成一堆 `clone()` 和 `move`，比现在难写。**
- 证据：`pool_op!` 的教训确实说明"借用式闭包"在 Rust 里代价高。
- 但那个场景是**原地 await 两次**；这里是**跨线程执行**，`'static` 是物理事实而非设计选择。
  另外绝大多数调用点本来就只传 ID/切片，`clone()` 的是小数据。
- 让步：为最常用路径提供**免闭包的具名函数**（`db::messages::page(&db, q).await`），
  闭包 API 只作为逃生口。这样 90% 的代码看不到闭包。

**反驳 2：单写线程会成为瓶颈（索引 + 同步 + 旗标都写）。**
- 证据：SQLite 本来只允许一个写者，WAL 下多写者也只会互相 `BUSY`。所以"单写线程"不是限制，
  而是把我们已经在承受的约束显式化，并且能在队列里**排序与合并**（例如把 500 次旗标更新合并成一次事务）。
- 让步：写队列必须可观测（`DbStats`），并把"每封一行"的写改成批处理；临界区里禁止做 CPU 重活（正文解析在写之外）。

**反驳 3：读池 + 写线程的共享内存库陷阱（`:memory:` 每连接独立）会让现有单测大面积失效。**
- 证据：确实是硬事实。
- 让步：`Db::open_shared_memory(name)` 封装 `file:name?mode=memory&cache=shared` + `SQLITE_OPEN_URI`，
  测试统一改用它；迁移期允许旧 `Cache` API 与 `Db` 并存（feature 门控），迁完删除。

**反驳 4：前端换栈（P1）会让 M1 延期数月，用户看到的功能反而变少。**
- 证据：Svelte/React 版本迁移 + 设计系统从零做，确实是 M 级而非 S 级。
- 让步：**分阶段替换**（§7.3），旧 UI 在新 UI 达到功能对等前一直可用；
  且 M1 的功能做在后端，与前端换栈可并行（后端先行）。
- 但要写清楚：如果选 ①（不换），M2 的无障碍/快捷键/i18n/虚拟列表会让 UI 代码失控，
  那时再换成本更高 —— 这是"现在多花 M，还是以后花 L"的选择。

**反驳 5：命令从 ~20 个涨到 ~90 个，会变成"分布式单体"式的一团。**
- 证据：命令多本身不坏，坏的是没有契约（参数形状各异、错误是字符串）。
- 让步：靠三件事压住 —— 命名规范、统一分页/批量/长任务形状、`api/` 单点出口 + 门禁校验。
  同时按域拆文件（`commands/mail.rs`、`commands/sync.rs`…），单个文件 ≤ 400 行。

**反驳 6：事件"只提示不携带真相"会让 UI 频繁全量刷新，反而更慢。**
- 证据：新邮件到达时确实需要重读列表首页。
- 让步：命令层提供**增量读**（`message_page` 带 `since_uid`/`changed_since`），
  事件载荷只给定位信息（account/folder/uid 集合），UI 只重读受影响部分。

**反驳 7：把中文文案集中到 `i18n.rs` 后，"错误必须可行动"的传统会退化成查表。**
- 证据：风险真实。
- 让步：约束是"文案必须包含两段：原因 + 下一步"（单元测试检查包含换行与"请"字之类不靠谱，
  改为：`user_hint()` 返回 `Option<&str>`，且 `ErrorCode` 里 `transient=false` 的 code 必须带 hint——
  用测试钉住这条，而不是钉字符串）。

## 12. 未决问题（需要你拍板或实测）

| # | 问题 | 影响 | 建议 |
|---|---|---|---|
| ~~P1~~ | 前端技术栈（§7.1） | — | **已定：② Vite + TS + Svelte 5**（2026-10-03，见 D2） |
| P2 | OAuth 应用归属（见 01 §7） | M1 门槛 | 立刻开始注册 |
| P3 | 是否保留"单 crate"（§3） | 编译与协作 | 保留，2 万行后再评 |
| P4 | 读连接数（默认 3）是否够 | 10 万封并发搜索与滚动 | 基准脚本实测后定（15） |
| P5 | 是否需要 `libsqlite3-sys` 的 bundled 版本（现有）之外的加密扩展 | SEC-11 | M3 再定 |
| P6 | 多窗口（UI-13）是否用同一进程多 WebView | 状态同步复杂度 | 同进程多窗口，事件广播（12） |

## 14. 附录：公共类型词汇表（跨模块反复出现，但不在任何单一模块里定义的类型）

> 各模块签名里会用到这些名字；**形状在这里统一约定**，实现时放 `mail-core::model` / `mail-core::ctx`，
> 避免每个模块各写一份。模块**私有**的辅助类型（例如 `PgpContext`、`SmimeOutcome`、`AttachmentTextReport`）
> 不在此表，按名字意图就近定义。

| 类型 | 形状 / 语义 |
|---|---|
| `MsgKey` | `struct MsgKey { account_id: String, folder: String, uid: u32 }`（`Clone + Hash + Eq`）；所有"针对某一封邮件"的入口都用它 |
| `AttachKey` | `struct AttachKey { account_id: String, folder: String, uid: u32, part: u32 }` |
| `Page<T>` | `struct Page<T> { items: Vec<T>, total: u32 }`：**所有列表命令的统一返回形状** |
| `PageQuery` | `{ account_id: Option<String>, folder: Option<String>, filter: ListFilter, sort: ListSort, offset: u32, limit: u32 }` |
| `ListFilter` | `enum { All, Unread, Flagged, HasAttachment, Thread, MentionsMe }` |
| `ListSort` | `enum { DateDesc, DateAsc, SizeDesc, Relevance }`（04/08/09 共用） |
| `MessageOp` / `BatchReport` | `enum MessageOp { MarkSeen(bool), Flag(bool), Move(String), Copy(String), Delete, Archive, AddLabel(String), RemoveLabel(String), Snooze(i64) }`；`struct BatchReport { ok: u32, failed: Vec<(MsgKey, ApiError)> }` |
| `AppCtx` | 进程级上下文：`Db`（cache + user 两个句柄）、`ConnectionPool`、`SyncScheduler`、`TokenProvider`、`NetMonitor`、`JobRegistry`、`AppHandle`、设置快照；任务间传 `Arc<AppCtx>` |
| `JobCtx` | 单任务控制面：`job_id`、`check_cancelled() -> Result<()>`、`progress(done, total)`、`pause_point()`（批次边界）、`note(&str)`（进诊断包） |
| `JobId` / `CancelToken` / `ProgressSink` | `JobId(String)`；`CancelToken = Arc<AtomicBool> + Notify`；`ProgressSink` 节流 ≥ 200 ms 上报 |
| 行类型（`AccountRow`/`FolderRow`/`MessageRow`/`BodyRow`/`AttachmentRow`/`DraftRow`/`OutboxRow`/`IndexRow`） | **与 03 的表一一对应**（字段即列名；`i64/u32` 映射在 `db/queries/*` 里做）。**不额外发明领域对象**，避免两套模型 |
| `NotifyActions` | `struct NotifyActions { mark_read: bool, delete: bool, snooze: bool }`（13 §3.2 用到） |
| 已在别处定义的 | `ApiError`/`ErrorCode`（§6.1）、`Db`/`DbOptions`/`DbStats`（§4.1）、`SyncWindow`/`SyncPolicy`/`JobSnapshot`/`NetState`（04）、`ThreadRow`/`ThreadMessage`（08）、`SearchRequest`/`SearchResponse`/`SearchHit`（09）、`Rule`/`RuleConditions`/`RuleAction`（10）、`Contact`/`ContactHit`（11）、`ScanReport`/`ImportPlan`（06）、`OutgoingMessage`/`BuildOutcome`/`Identity`（07）、`MailBackend`/`BackendCapabilities`（18） |

**纪律**：模块若发现需要新的跨模块公共类型，**先加进本表**再使用（否则实现阶段会出现
"三个模块各自定义一个 `ThreadRow`"的经典事故）。

## 15. 验收清单（架构层）

- [ ] `Db` 句柄可被 Tauri 命令直接使用（`Send + Sync`），异步命令里出现 `Cache::open` 的次数为 0。
- [ ] **首同步（10 万封头部）≤ 8 分钟**（批量事务生效；与 04/15 的口径一致 —— 按 ≥300 封/秒算 10 万封需 5.6 分钟，写 5 分钟是自相矛盾），过程中 UI 可读可搜（读写不互斥）。
- [ ] 关闭网络后，全部读命令与写信/整理命令正常返回。
- [ ] 任一后台任务可暂停/取消，取消后本地数据一致（无半写）。
- [ ] `ApiError` 在 20 个代表性失败场景下都给出"原因 + 怎么办"的中文文案，且无裸库错误字符串。
- [ ] `check.ps1` 的五项新增检查通过（§10）。
- [ ] 冷启动/切文件夹/搜索/内存四项预算在 10 万封夹具上达标（15 提供夹具生成器）。

> **章节名对照（模板 13 节）**：数据结构→§3 模块地图+§8 配置状态；接口→§6 接口契约；失败模式→§4.1/§4.2 失败与降级；测试/门禁→§10。
> 内容齐全，只是节名与模板不完全同名（见 design/README.md 的说明）。
