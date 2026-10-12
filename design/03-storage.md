# 03 存储引擎与数据模型 v3

> **目标**：给出一个"能装下 10 万封邮件 + 全文索引 + 草稿 + 发件队列 + 离线操作 + 联系人 + 规则"
> 的本地库，并且**删得掉、修得好、带得走**。
>
> 覆盖 ID：SYN-11 配额清理、SYN-12 自检修复、SYN-13/ MIG-06 备份导出、SEC-11（可选加密）。
> 上游依赖：`02-architecture.md` §4.1（`Db` 句柄：单写线程 + 读池）。下游：04/07/08/09/10/11/16 全部读写本库。

---

## 1. 目标与非目标

**目标**
1. 单库支撑 M1→M3 的全部本地状态（邮件、索引、草稿、队列、联系人、规则、统计）。
2. **可删除、可重建**：删掉 `cache.db` 只丢缓存与本地索引，不丢账号、不丢凭据、不丢服务器上的邮件。
3. 一切写入**批量 + 事务**；一切长任务**可中断、可续跑**。
4. 迁移**前向、可回滚**（失败保留旧库）。
5. 存储占用**可见、可控、可清理**，且清理永不静默删服务器数据。

**非目标**
- 不做通用 ORM、不做跨库抽象（就 SQLite）。
- 不做邮件归档格式（.pst/.olm）的**写入**（导入见 06）。
- SEC-11（整库加密）M3 再定，本文只预留边界（`DbOptions.encryption`）。

## 2. 用户可感知行为

| 场景 | 用户看到 |
|---|---|
| 设置 → 存储 | "本地占用 1.24 GB：邮件头 25 MB、正文 780 MB、附件 120 MB、索引 210 MB、其它 105 MB；最早 2019-03" + 每个账号/文件夹明细 + 清理策略开关 |
| 磁盘吃紧 | 按策略自动清理最旧的**正文与附件**（保留邮件头，列表不塌）；设置页出现"已自动清理 320 MB，详情" |
| 断网时标已读/移动/删除 | 动作立刻生效，行尾出现小云朵"待同步"；联网后云朵消失；同一会话里点一下能看到"3 项待同步" |
| 电脑断电后重启 | 草稿、待同步操作、未完成的同步全部还在，继续跑 |
| 怀疑库坏了 | 设置 → 高级 → "检查本地数据"：给出报告 + "修复"（重建索引/清理孤儿/校正游标），严重损坏时提示"已备份损坏库并重建，请重新同步" |
| 换电脑 | 设置 → 备份：生成 `easy-email-backup-*.db`（含设置与账号，不含密码）；新机器恢复后提示重登 |
| 导出 | 文件夹导出 mbox / 单封导出 .eml；导出结果能被 Thunderbird 直接导入（人工验收项） |

## 3. 数据结构（v3 全量 DDL）

> 约定：所有时间戳 `INTEGER` Unix 秒 UTC；布尔 `INTEGER 0/1`；JSON 字段以 `_json` 结尾；
> 所有表带 `account_id`（除了全局表），方便"删账号=级联删"。

```sql
-- ═══ 元信息 ═══
create table if not exists schema_migrations (
    version    integer primary key,
    name       text    not null,
    applied_at integer not null,
    checksum   text    not null
);
create table if not exists kv (            -- 零散状态：上次 vacuum、窗口位置、灰度标记…
    k text primary key, v text not null
);
create table if not exists metrics (       -- 累计计数，供设置页与诊断包显示
    k text primary key, v integer not null
);

-- ═══ 账号（accounts.json 是真相，本表是镜像，供 join 与统计）═══
create table if not exists accounts (
    id             text primary key,       -- = 邮箱地址小写（既有约定）
    email          text not null,
    display_name   text not null default '',
    username       text not null,
    provider_id    text not null,
    imap_host      text not null, imap_port integer not null, imap_security text not null,
    smtp_host      text not null, smtp_port integer not null, smtp_security text not null,
    needs_id       integer not null default 0,
    max_conns      integer not null default 3,
    enabled        integer not null default 1,
    sort_order     integer not null default 0,
    color          text,                   -- 账号标识色（UI-13/ACC-13）
    sync_window    text not null default 'years',   -- all | years | count | on_demand
    sync_window_n  integer not null default 2,
    signature_id   text,
    created_at     integer not null, updated_at integer not null,
    last_error     text, last_error_at integer
);

-- ═══ 文件夹 ═══
create table if not exists folders (
    account_id     text not null,
    name           text not null,          -- 线路名（modified UTF-7），命令只认它
    display_name   text not null,
    delimiter      text,
    selectable     integer not null default 1,
    role           text,                   -- inbox/sent/drafts/trash/junk/archive/other
    uidvalidity    integer not null default 0,
    uidnext        integer not null default 0,
    highest_modseq integer,                -- CONDSTORE 时可用
    last_uid       integer not null default 0,   -- 增量游标（04 的 gap-free 规则）
    history_complete integer not null default 0, -- 已取到最早一封（既有设计）
    total_count    integer not null default 0,
    unread_count   integer not null default 0,
    sync_enabled   integer not null default 1,   -- 用户可对单文件夹关闭同步
    window_override text, window_override_n integer,  -- 覆盖账号级窗口
    last_sync_at   integer, last_sync_ok integer, last_error text, last_error_at integer,
    primary key (account_id, name)
);

-- ═══ 邮件头（列表的全部来源）═══
create table if not exists messages (
    account_id     text not null,
    folder         text not null,
    uid            integer not null,
    -- **origin 是全库最重要的标记位（第二轮评审新增）**：
    --   server   = 服务器上真有这封（可重新同步、可被清理、可对其发服务器命令）
    --   imported = 从 mbox/eml 导入的本机存档（**不可再生**，禁止清清理、禁止发服务器命令）
    --   pop3     = POP3 收下来的（同上，且没有服务器侧 UID 概念）
    -- 归属：origin <> 'server' 的行写进 user.db（见 §11 反驳 5 的三条规则）
    origin         text not null default 'server',
    thread_id      text,                   -- 会话串（08 负责算法，这里存结果）
    message_id     text,                   -- 规范化后的 <id@host>
    in_reply_to    text,
    refs_json      text,                   -- References 链（原样保存，供重建会话）
    subject        text not null default '',
    subject_norm   text not null default '',  -- 去 Re:/Fwd:/空白，供主题兜底成串
    from_name      text, from_addr text,
    to_json        text, cc_json text, reply_to text,
    date           integer,                -- Date 头（已归一到 UTC）
    internal_date  integer,                -- INTERNALDATE（排序更可靠）
    size           integer,
    seen           integer not null default 0,
    flagged        integer not null default 0,
    answered       integer not null default 0,
    draft          integer not null default 0,
    forwarded      integer not null default 0,
    has_attachments integer not null default 0,
    attachment_count integer not null default 0,
    is_junk        integer not null default 0,
    snippet        text,                   -- 列表预览用（索引器或头部生成，≤ 200 字）
    body_cached    integer not null default 0,
    raw_cached     integer not null default 0,
    indexed_at     integer,                -- 进过 FTS 的时间
    modseq         integer,                -- CONDSTORE
    local_state    integer not null default 0, -- 0=与服务器一致 1=有本地待同步操作
    primary key (account_id, folder, uid)
);
create index if not exists messages_list
    on messages(account_id, folder, date desc, uid desc);
create index if not exists messages_internal
    on messages(account_id, folder, internal_date desc, uid desc);
create index if not exists messages_thread
    on messages(account_id, thread_id, date);
create index if not exists messages_msgid
    on messages(account_id, message_id);
create index if not exists messages_index_pending
    on messages(account_id, indexed_at, uid) where indexed_at is null;
create index if not exists messages_local_state
    on messages(account_id, folder, local_state) where local_state <> 0;
create index if not exists messages_flags
    on messages(account_id, folder, seen, flagged, date desc);
create index if not exists messages_attach
    on messages(account_id, folder, has_attachments, date desc);

-- ═══ 会话（物化，供会话列表与计数；算法见 08）═══
create table if not exists threads (
    account_id   text not null,
    thread_id    text not null,
    subject_norm text not null default '',
    newest_folder text,                    -- 最新一封所在文件夹（列表点击用）
    newest_uid   integer,
    last_date    integer,
    message_count integer not null default 0,
    unread_count integer not null default 0,
    flagged_count integer not null default 0,
    has_attachments integer not null default 0,
    participants_json text,                -- [{name,addr}] 去重，≤ 5 个
    primary key (account_id, thread_id)
);
create index if not exists threads_list on threads(account_id, last_date desc);

-- ═══ 正文与原文 ═══
create table if not exists bodies (
    account_id  text not null,
    folder      text not null,
    uid         integer not null,
    text_body   text,
    html_body   text,
    attachments_json text,                 -- Vec<AttachmentMeta>
    inline_json text,                      -- Vec<InlineImage>（cid → data: 由前端生成）
    raw_zstd    blob,                      -- 原文（zstd），按策略保留；转发/导出/打印要用
    raw_bytes   integer not null default 0,
    fetched_at  integer not null,
    primary key (account_id, folder, uid)
);

-- ═══ 附件：元数据在 messages 侧，字节走内容寻址 ═══
create table if not exists attachments (
    account_id text not null, folder text not null, uid integer not null,
    part       integer not null,           -- mail-parser 的下标（与既有实现一致）
    name       text not null default '',
    mime       text not null default 'application/octet-stream',
    size       integer not null default 0,
    content_id text,
    is_inline  integer not null default 0,
    sha256     text,                       -- 下载后才填
    saved_path text,                       -- 用户另存到磁盘的路径（不复制进库）
    text_state integer not null default 0, -- 0 未尝试 1 已索引 2 不支持 3 失败
    primary key (account_id, folder, uid, part)
);
create index if not exists attachments_sha on attachments(sha256);
create index if not exists attachments_pending on attachments(text_state) where text_state = 0;

create table if not exists blobs (         -- 内容寻址：同一附件多封引用只存一份
    sha256     text primary key,
    size       integer not null,
    bytes_zstd blob not null,
    refcount   integer not null default 0,
    created_at integer not null,
    last_used_at integer not null
);

-- ═══ 标签 / 关键字（IMAP keywords、Gmail 标签）═══
create table if not exists labels (
    account_id text not null, name text not null,
    display_name text not null, color text,
    kind text not null default 'keyword',  -- keyword | gmail | system
    primary key (account_id, name)
);
create table if not exists message_labels (
    account_id text not null, folder text not null, uid integer not null, label text not null,
    primary key (account_id, folder, uid, label)
);
create index if not exists message_labels_by_label on message_labels(account_id, label, folder, uid);

-- ═══ 草稿（本地即时保存 + 服务器 APPEND 同步）═══
create table if not exists drafts (
    id          text primary key,          -- uuid
    account_id  text not null,
    kind        text not null default 'new', -- new | reply | reply_all | forward
    in_reply_to text, refs_json text,
    to_json text, cc_json text, bcc_json text,
    subject     text not null default '',
    body_text   text not null default '',
    body_html   text,                      -- 富文本（MAIL-01）时才有
    attachments_json text,                 -- [{path,name,mime,size,sha256?}]
    signature_id text,
    created_at  integer not null, updated_at integer not null,
    server_uid  integer, server_folder text,   -- APPEND 后在服务器上的位置
    sync_state  integer not null default 0,    -- 0 脏 1 已同步 2 同步失败
    last_error  text
);
create index if not exists drafts_list on drafts(account_id, updated_at desc);
create index if not exists drafts_dirty on drafts(sync_state) where sync_state <> 1;

-- ═══ 发件箱（离线入队、重试、撤销、定时）═══
create table if not exists outbox (
    id          integer primary key autoincrement,
    account_id  text not null,
    draft_id    text,                      -- 发出后可删除草稿
    payload_json text not null,            -- 冻结的发送内容（含附件路径与 sha）
    state       text not null default 'queued', -- queued|sending|sent|failed|cancelled|needs_attention
    attempts    integer not null default 0,
    -- **两列都必须 not null default 0**（第二轮评审）：`null <= now` 恒为 NULL，
    -- 会让"从没重试过"的邮件永远不被取件（作者只在 scheduled_at 上加了 is null 保护）
    next_attempt_at integer not null default 0,
    last_error  text,
    undo_until  integer not null default 0,   -- 撤销窗口（MAIL-05）；0 = 无窗口
    scheduled_at integer not null default 0,  -- 定时发送（MAIL-06）；0 = 立刻
    -- **spool 的落点（第二轮评审补）**：冻结后的字节要能被重启后的重试找到，
    -- 也让 7 天清理有键可依（16 §4.2）
    spool_path  text, spool_bytes integer not null default 0,
    created_at  integer not null, sent_at integer,
    sent_message_id text
);
create index if not exists outbox_pending on outbox(state, next_attempt_at);

-- ═══ 离线操作队列（标已读/移动/删除/加标签/APPEND 草稿）═══
create table if not exists pending_ops (
    id          integer primary key autoincrement,
    account_id  text not null, folder text not null, uid integer,
    kind        text not null,             -- set_seen|set_flag|add_label|remove_label|move|delete|append_draft|expunge
    payload_json text not null,
    state       text not null default 'queued',  -- queued|running|done|failed|conflict
    attempts    integer not null default 0,
    next_attempt_at integer not null default 0,  -- 同上：0 而不是 NULL，否则永远取不到
    last_error  text,
    created_at  integer not null, updated_at integer not null
);
create index if not exists pending_ops_queue on pending_ops(state, next_attempt_at);

-- ═══ 同步任务（可暂停/取消/续跑）═══
create table if not exists sync_jobs (
    id          text primary key,
    account_id  text not null, folder text,
    kind        text not null,             -- initial|incremental|backfill|body_prefetch|index|rebuild_threads
    state       text not null default 'queued', -- queued|running|paused|cancelled|done|failed
    cursor_json text,                      -- 任务自己的断点（与 folders.last_uid 正交）
    done        integer not null default 0,
    total       integer not null default 0,
    -- **第二轮评审补**：配额停摆/退避需要一个落点（04 §5.7 的"次日 00:05 继续"）。
    -- 用 0 表示"立刻可跑"，避免 NULL 比较陷阱（`null <= now` 恒为 NULL ⇒ 任务永远不被取）。
    next_attempt_at integer not null default 0,
    created_at  integer not null, started_at integer, finished_at integer,
    last_error  text
);
create index if not exists sync_jobs_state on sync_jobs(state, next_attempt_at);

-- ═══ 索引队列（正文/附件文本）═══
create table if not exists index_queue (
    account_id text not null, folder text not null, uid integer not null,
    kind       text not null default 'body',   -- body | attachment_text
    priority   integer not null default 100,   -- 数字小的先做（打开过的邮件优先级更高）
    state      text not null default 'queued',  -- queued|running|done|failed|skipped
    attempts   integer not null default 0,
    last_error text,
    queued_at  integer not null,
    primary key (account_id, folder, uid, kind)
);
create index if not exists index_queue_next on index_queue(state, priority, queued_at);

-- ═══ 搜索：保存搜索 + 全文索引 ═══
create table if not exists saved_searches (
    id text primary key, name text not null, query text not null,
    sort text not null default 'date_desc', icon text, sort_order integer not null default 0,
    created_at integer not null
);

-- ═══ 搜索：**表结构以 09 §3 为唯一来源**（第二轮评审：这里原来写的是 standalone 版本，
--     与 09 定案的 external-content 设计冲突，且会导致实测 987 ms/行的删除）═══
-- 摘要：`search_text`（索引源表，rowid = sha1(account|folder|uid) 低位）+ external-content
--       `search`（content='search_text', content_rowid='id', columnsize=0, detail='column'）。
--       **删除/更新必须按 rowid**（实测：按 (account,folder,uid) 过滤是虚拟表全扫 = 987 ms/行；
--       按 rowid 是 3.4 ms），所以 `messages` 需要一列 `search_rowid` 保存映射。
--       完整 DDL 与维护规则见 09 §3/§5.1，**不要在这里另写一份**。

-- ═══ 规则 / 黑名单 / 垃圾 ═══
create table if not exists rules (
    id text primary key, name text not null, enabled integer not null default 1,
    sort_order integer not null default 0,
    conditions_json text not null, actions_json text not null,
    stop_processing integer not null default 0,
    created_at integer not null, updated_at integer not null, last_run_at integer
);
create table if not exists blocklist (
    id integer primary key autoincrement,
    kind text not null,                    -- email | domain
    value text not null, action text not null default 'move_junk', -- move_junk|delete
    created_at integer not null,
    unique(kind, value)
);
create table if not exists allowlist (     -- 发件人白名单（远程内容、垃圾判定共用）
    id integer primary key autoincrement,
    kind text not null, value text not null, purpose text not null, -- remote_images|spam
    created_at integer not null, unique(kind, value, purpose)
);

-- ═══ 通讯录 ═══
create table if not exists contacts (
    id text primary key, display_name text not null default '',
    name_norm text not null default '',        -- 评审补：补全要按它建索引（原来 display_name 没有索引）
    org text, note text, avatar_path text,
    groups_json text, source text not null default 'collected', -- manual|collected|imported|carddav
    merged_into text, merged_at integer,       -- 评审补：合并用软删除，撤销才可能实现（11 §3）
    created_at integer not null, updated_at integer not null
);
create index if not exists contacts_name_prefix on contacts(name_norm);
create table if not exists contact_emails (   -- 自动补全的真正数据源
    email text not null, contact_id text not null,
    -- **第二轮评审修正**：补全的 20 ms 预算在原结构上达不到 —— `lower(email) LIKE ?` 用不上 email 索引，
    -- 且 PK 是 (email, contact_id) ⇒ `Alice@x.com` 与 `alice@x.com` 会是两行（重复建议 + 双倍占上限）。
    email_norm text not null default '',
    name text, sent_count integer not null default 0, recv_count integer not null default 0,
    last_used_at integer,
    primary key (email, contact_id)
);
create index if not exists contact_emails_prefix on contact_emails(email_norm);
create index if not exists contact_emails_recent on contact_emails(last_used_at desc);
create index if not exists contact_emails_freq on contact_emails(sent_count desc, recv_count desc);
-- 规范规则（唯一来源，单测钉住）：email_norm = trim + lowercase（**不做** Gmail 点号/加号折叠，
-- 那会改变寻址语义）；name_norm 见 contacts 表

-- ═══ 通知去重与历史 ═══
create table if not exists notification_log (
    id integer primary key autoincrement,
    account_id text not null, folder text not null, uid integer,
    thread_id text, shown_at integer not null, clicked_at integer
);
create index if not exists notification_recent on notification_log(shown_at desc);
```

**关于 `raw_zstd`**：原文是"转发带附件、导出 .eml、打印、查看源码"的必需品，
但 10 万封 × 平均 60 KB = 6 GB 不可接受。规则：
- 只对**已取过正文**的邮件保留原文；
- 默认只保留 `size ≤ 512 KB` 的原文，且压缩存储（zstd，文本类通常 3-4×）；
- 设置项 `keep_raw`: `smart`（默认）/ `all` / `none`；
- 配额清理时**先删原文**，再删附件字节，再删正文，最后才删邮件头。

## 3.1 每个库的表清单（**唯一权威**；第二轮评审新增，因为各模块各自加表导致漏项）

> 规则：**任何新表都必须先登记到这里**（否则迁移、备份与 02 §14 的类型表都会漏）。
> 迁移与备份脚本直接读这张表，不再靠人肉对照。

| 库 | 表 |
|---|---|
| **`cache.db`**（可清理，服务器数据的副本与派生数据） | `schema_migrations`、`kv`、`metrics`、`folders`、`messages`（`origin='server'` 的行）、`bodies`、`attachments`、`blobs`、`threads`、`search_text`、`search`（FTS）、`index_queue`、`sync_jobs`、`labels`、`message_labels`、`sent_index`、`import_dedupe`、`notification_log` |
| **`user.db`**（不可再生：用户意图 + 本机存档） | `schema_migrations`、`kv`、`accounts`（镜像）、`messages`（`origin<>'server'` 的行）、`bodies`/`attachments`/`blobs`（对应本机存档）、`folders`（本机账号的）、`drafts`、`outbox`、`pending_ops`、`rules`、`rule_runs`、`blocklist`、`allowlist`、`unsubscribe_log`、`contacts`、`contact_emails`、`contact_groups`、`contact_merges`、`saved_searches`、`search_history`、`signatures`、`templates`、`identities`、`import_jobs`、`quota_ledger`（配额账本，见 04 §5.7） |
| **两个库都有** | `schema_migrations`（各自一套版本号）、`kv`（各自的本机状态） |

**注意两点**：①`accounts` 只镜像进 `user.db`（它是配置的派生，见 §11 反驳 1）；
②**配额账本放 `user.db`**（第二轮评审：放在可删的 `cache.db` 里，删缓存就会忘掉"今天已经用了多少"，
于是立刻重新超配额下载，正好触发 Gmail 限流）。


## 4. 接口（函数级）

### 4.1 `Db` / `DbSet`（**定义在 02 §4.1**，此处不重复；要点：两个库两个句柄、写线程是 `std::thread`、不得 `tokio::spawn`）

### 4.2 查询模块（每个域一个模块，函数是"具名 SQL 的入口"，避免调用点写 SQL）

```rust
pub mod q {
    pub mod accounts {
        pub async fn upsert_many(db: &Db, rows: Vec<AccountRow>) -> Result<()>;
        pub async fn list(db: &Db) -> Result<Vec<AccountRow>>;
        pub async fn set_enabled(db: &Db, id: &str, on: bool) -> Result<()>;
        pub async fn set_sync_window(db: &Db, id: &str, w: SyncWindow) -> Result<()>;
        pub async fn delete_cascade(db: &Db, id: &str) -> Result<CascadeReport>;
        /// accounts.json 与镜像行对账（启动时调用，返回修正数）
        pub async fn reconcile(db: &Db, from_json: &[AccountConfig]) -> Result<ReconcileReport>;
    }
    pub mod folders {
        pub async fn upsert(db: &Db, row: FolderRow) -> Result<()>;
        pub async fn list(db: &Db, account_id: &str) -> Result<Vec<FolderRow>>;
        pub async fn get(db: &Db, account_id: &str, name: &str) -> Result<Option<FolderRow>>;
        pub async fn set_cursor(db: &Db, account_id: &str, name: &str, uidvalidity: u32, last_uid: u32) -> Result<()>;
        pub async fn mark_history_complete(db: &Db, account_id: &str, name: &str, uidvalidity: u32) -> Result<()>;
        pub async fn invalidate(db: &Db, account_id: &str, name: &str) -> Result<u64>;   // UIDVALIDITY 变化
        pub async fn set_sync_state(db: &Db, account_id: &str, name: &str, s: FolderSyncState) -> Result<()>;
        pub async fn counts(db: &Db, account_id: &str) -> Result<Vec<FolderCounts>>;      // 未读/总数（托盘角标）
    }
    pub mod messages {
        pub async fn upsert_many(db: &Db, rows: Vec<MessageRow>) -> Result<u32>;          // 一批一个事务
        pub async fn page(db: &Db, q: PageQuery) -> Result<Page<MessageRow>>;
        pub async fn get(db: &Db, k: MsgKey) -> Result<Option<MessageRow>>;
        pub async fn by_uids(db: &Db, account_id: &str, folder: &str, uids: &[u32]) -> Result<Vec<MessageRow>>;
        pub async fn neighbors(db: &Db, q: NeighborQuery) -> Result<Neighbors>;           // 上/下一封（READ-14）
        pub async fn count(db: &Db, q: CountQuery) -> Result<u32>;
        pub async fn apply_flag(db: &Db, keys: &[MsgKey], change: FlagChange) -> Result<u32>;
        pub async fn delete_many(db: &Db, keys: &[MsgKey]) -> Result<u32>;                // 本地删（连带 bodies/fts/attachments）
        pub async fn mark_local_state(db: &Db, keys: &[MsgKey], state: i32) -> Result<()>;
        pub async fn pending_index_batch(db: &Db, limit: u32) -> Result<Vec<MsgKey>>;     // 索引器取活
        pub async fn set_indexed(db: &Db, keys: &[MsgKey]) -> Result<()>;
        pub async fn snippet_candidates(db: &Db, limit: u32) -> Result<Vec<MsgKey>>;      // 无正文时生成列表预览
    }
    pub mod threads { pub async fn upsert_many(..); pub async fn page(..); pub async fn get(..);
                      pub async fn rebuild_all(db: &Db, account_id: &str, ctx: &JobCtx) -> Result<u64>;
                      pub async fn members(db: &Db, account_id: &str, thread_id: &str) -> Result<Vec<MessageRow>>; }
    pub mod bodies { pub async fn put(db: &Db, b: BodyRow) -> Result<()>; pub async fn get(db: &Db, k: MsgKey) -> Result<Option<BodyRow>>;
                     pub async fn trim_raw(db: &Db, policy: RawPolicy) -> Result<u64>; pub async fn iter_for_index(..); }
    pub mod attachments { pub async fn upsert_many(..); pub async fn list_for(..); pub async fn set_text_state(..);
                          pub async fn pending_text_batch(db: &Db, limit: u32) -> Result<Vec<AttachKey>>; }
    pub mod blobs { pub async fn put(db: &Db, bytes: &[u8]) -> Result<String>;   // 返回 sha256，自动 refcount
                    pub async fn open(db: &Db, sha: &str) -> Result<Option<Vec<u8>>>;
                    pub async fn gc(db: &Db, ctx: &JobCtx) -> Result<u64>; }
    pub mod labels { pub async fn ensure(db: &Db, account_id: &str, name: &str, color: Option<&str>) -> Result<()>;
                     pub async fn assign(..); pub async fn unassign(..); pub async fn list_for(..); pub async fn rename(..); }
    pub mod drafts { pub async fn save(db: &Db, d: &DraftRow) -> Result<()>; pub async fn get(..); pub async fn list(..);
                     pub async fn mark_synced(..); pub async fn delete(..); pub async fn dirty(..); }
    pub mod outbox { pub async fn enqueue(db: &Db, item: &OutboxRow) -> Result<i64>; pub async fn due(db: &Db, now: i64) -> Result<Vec<OutboxRow>>;
                     pub async fn mark(..); pub async fn cancel(db: &Db, id: i64) -> Result<()>; pub async fn stats(db: &Db) -> Result<OutboxStats>; }
    pub mod pending_ops { pub async fn push(..); pub async fn due(..); pub async fn mark(..);
                          pub async fn local_pending_count(db: &Db, account_id: &str) -> Result<u32>; }
    pub mod sync_jobs { pub async fn create(..); pub async fn next_queued(..); pub async fn set_state(..);
                        pub async fn progress(..); pub async fn resumable(db: &Db) -> Result<Vec<SyncJobRow>>; }
    pub mod index_queue { pub async fn push(..); pub async fn next(..); pub async fn mark(..); pub async fn depth(db: &Db) -> Result<u32>; }
    pub mod rules { pub async fn list(..); pub async fn save(..); pub async fn delete(..); pub async fn reorder(..); }
    pub mod contacts { pub async fn upsert(..); pub async fn search(db: &Db, q: &str, limit: u32) -> Result<Vec<ContactHit>>;
                       pub async fn autocomplete(db: &Db, prefix: &str, limit: u32) -> Result<Vec<ContactHit>>;
                       pub async fn touch_usage(db: &Db, emails: &[String], sent: bool) -> Result<()>;
                       pub async fn collect_from_message(db: &Db, m: &MessageRow) -> Result<u32>; }
    pub mod saved_searches { pub async fn list(..); pub async fn save(..); pub async fn delete(..); }
    pub mod metrics { pub async fn bump(db: &Db, k: &str, by: i64) -> Result<()>; pub async fn get(db: &Db, k: &str) -> Result<i64>; }
    pub mod kv { pub async fn get(db: &Db, k: &str) -> Result<Option<String>>; pub async fn set(db: &Db, k: &str, v: &str) -> Result<()>; }
}
```

### 4.2.1 `DbSet::writer_for` 的路由表（评审要求：把"哪张表走哪个句柄"写死）

| 写什么 | 句柄 | 说明 |
|---|---|---|
| `folders`/`messages`/`bodies`/`attachments`/`blobs`/`threads`/`search_text`/`search`/`index_queue`/`sync_jobs`/`labels`/`message_labels`/`sent_index`/`import_dedupe`/`metrics` | `cache` | 服务器账号的副本与派生数据 |
| 同上这些表，但 `origin <> 'server'`（本机存档 / POP3 的行） | **`user`** | 不可再生 ⇒ 必须在 user.db；`writer_for(kind)` 按账号 kind 选句柄 |
| `drafts`/`outbox`/`pending_ops`/`rules`/`rule_runs`/`blocklist`/`allowlist`/`unsubscribe_log`/`contacts`/`contact_emails`/`contact_groups`/`contact_merges`/`saved_searches`/`search_history`/`signatures`/`templates`/`identities`/`import_jobs`/`quota_ledger`/`accounts` | **`user`** | 用户意图与不可再生数据 |
| `kv`/`schema_migrations` | 两个库各自 | 各库自己的版本与本机状态 |

**跨库的四处（明确接受"两次顺序写、非原子"）**：①旗标/移动（cache 的 `messages.local_state` + user 的 `pending_ops`）；
②规则命中（cache 的 messages + user 的 pending_ops/rule_runs）；③联系人收集（cache 的 messages → user 的 contacts）；
④发送后归档（cache 的 sent_index + user 的 pending_ops）。四处的共同点是**两边都幂等**，由夜检（16 §4.2）纠正。
### 4.3 索引、配额、自愈、备份

```rust
```rust
pub mod fts {
    /// **必须按 rowid 删/插**（09 §3）：按 (account,folder,uid) 过滤是虚拟表全扫，
    /// 实测 987 ms/行 ⇒ 50 封一批要 36 秒；按 rowid 是 3.4 ms。
    pub async fn upsert_many(db: &Db, rows: Vec<IndexRow>) -> Result<u32>;   // 同一事务内 delete(rowid)+insert
    pub async fn delete_many(db: &Db, keys: &[MsgKey]) -> Result<u32>;        // 经 messages.search_rowid 转 rowid
    pub async fn rebuild(db: &Db, account_id: Option<&str>, ctx: &JobCtx) -> Result<u64>;
    pub async fn optimize(db: &Db) -> Result<()>;                             // **只由用户点"重建索引"触发**
    pub async fn bounded_merge(db: &Db, pages: u32) -> Result<()>;            // 空闲时 `merge 256`（实测固定开销）
    pub async fn stats(db: &Db) -> Result<IndexStats>;
    pub async fn consistency(db: &Db) -> Result<IndexConsistency>;
}

pub struct CleanupPolicy {
    pub max_bytes: Option<u64>, pub max_age_days: Option<u32>,
    pub keep_unread: bool, pub keep_flagged: bool,
    pub attachment_days: Option<u32>, pub raw: RawPolicy,
}
pub struct CleanupPlan { pub steps: Vec<CleanupStep>, pub freed_bytes: u64, pub keeps: CleanupKeeps }
pub mod quota {
    pub async fn report(db: &Db) -> Result<StorageReport>;
    pub async fn plan(db: &Db, p: &CleanupPolicy) -> Result<CleanupPlan>;      // 干跑，永远先干跑

> **实现注记（第 93 轮）**：`crates/mail-core/src/quota.rs` 落地。验收"**干跑数字 == 实跑数字**"
> 不靠两次算，靠**一条谓词**（`cleanup_predicate` 只写一遍，plan 拿它 SELECT/SUM、apply 拿它
> DELETE）——两份 SQL 一定会分叉，用户从那句 toast 里发现。apply 在删之前**再量一次**
> （告诉用户的是实际发生的，不是几毫秒前承诺的）。`Cache::cleanup` 是生产入口：
> **删正文与刷新索引在同一个写事务里**（正文带走它的词，留着那些词的索引行会继续为缓存里
> 已经没有的邮件作答——这是本轮一次返工的结论）。**无上限且无年龄规则 ⇒ 什么都不删**
> （用户没要求过就删缓存，比缓存变大更糟）；未读/有旗标永不清。**零网络**由门禁钉住。

> **命令与留痕注记（第 94 轮）**：`cleanup_apply` 收 **plan 不收 policy**——收 policy 的命令能让
> 调用方直奔删除，"永远先干跑"会变成一次可顺手跳过、且没有东西会因此看着不对的改动。
> 第一版实现正是如此（收了 plan 却自己重算），已重构出 `Cache::apply_plan(plan)`：
> **跑用户刚读过的那一份**，与 `Cache::cleanup` 共用索引刷新。
> 留痕 `cleanup_log`（在 `user.db`，清缓存不带走）记**两个数字**：plan 承诺的与 apply 实测的。
> 它们相等是因为共用同一谓词，所以**不一致的行就是 bug，而这张表是它唯一会显形的地方**。

> **界面注记（第 95 轮）**：侧栏「存储与清理」⇒ 模态（占用/上限/「看看会清什么」/「开始清理」/
> 最近清理/「清空记录」）。**「开始清理」在干跑结果显示前禁用**——这是第 94 轮"命令收 plan"在
> 界面上的回鸣；同一个原则在两层各出现一次是有意的（签名让跳过变难，按钮让跳过变显眼）。
> 界面上必须说清：只删已下载正文缓存、服务器不受影响、**未读与有旗标永不清**；
> 「清空记录」只删记录不动缓存。上限框不接受 0（那等于"永远清光一切"——一次手滑最不该能产生
> 这个值）。**未做每日定时跑**：用户没要求过就自动删缓存，比缓存变大更糟。
    pub async fn apply(db: &Db, plan: CleanupPlan, ctx: &JobCtx) -> Result<CleanupReport>;
    pub async fn vacuum(db: &Db, incremental: bool) -> Result<u64>;
}

pub enum HealMode { Check, Repair, RebuildIndex }
pub struct HealReport { pub integrity: IntegrityResult, pub orphans_removed: u32, pub cursors_fixed: u32,
                        pub threads_rebuilt: u32, pub index_rows: u32, pub duration_ms: u64, pub notes: Vec<String> }
pub mod heal {
    pub async fn check(db: &Db, mode: HealMode) -> Result<HealReport>;
    pub async fn repair(db: &Db, report: HealReport, ctx: &JobCtx) -> Result<HealReport>;
    pub async fn quarantine_and_reset(db_path: &Path) -> Result<PathBuf>;      // 损坏库改名 + 新建
}

pub mod backup {
    pub struct BackupInfo { pub path: PathBuf, pub schema_version: i64, pub accounts: u32, pub messages: u32, pub bytes: u64, pub created_at: i64 }
    pub async fn create(db: &Db, dest_dir: &Path, include_settings: bool) -> Result<BackupInfo>;  // SQLite backup API + 校验
    pub async fn verify(src: &Path) -> Result<BackupInfo>;
    pub fn schedule_restore(src: &Path) -> Result<()>;      // 写 kv 标记，下次启动执行
    pub fn take_pending_restore(app_dir: &Path) -> Result<Option<PathBuf>>;
}

pub mod export {
    pub async fn folder_mbox(db: &Db, account_id: &str, folder: &str, dest: &Path, ctx: &JobCtx) -> Result<ExportReport>;
    pub async fn messages_eml(db: &Db, keys: &[MsgKey], dest: &Path) -> Result<ExportReport>;  // 无原文则先取回
    pub async fn thread_eml(db: &Db, account_id: &str, thread_id: &str, dest: &Path) -> Result<ExportReport>;
}
```

## 5. 主流程与状态机

### 5.1 启动
```
1. **打开两个库**（`DbSet::open(cache_path, user_path)`；各自 WAL/busy_timeout/auto_vacuum）→ 各读自己的 user_version
2. 若有 pending_restore 标记 → 走恢复流程（§5.5）后重启
3. 若任一库 user_version < 目标 → **先备份该库**（分库备份，见 §5.5）→ 逐个迁移（每个一个事务）→ 写各自的 schema_migrations
4. reconcile(accounts.json) → 修正镜像行
5. quick_check（异步，不阻塞 UI）→ 若失败 → 提示 + 建议修复
6. 加载 folders → 启动后台任务（断点续跑：sync_jobs/index_queue/pending_ops/outbox）
```

### 5.2 一次同步落库（与 04 的接口）
```
sync worker 取回一页头部（500 封）
  → db.write(|c| { messages::upsert_many 在同一个事务里插 500 行
                   + update folders.last_uid/total_count
                   + 把需要索引的 uid 批量塞进 index_queue
                   + 重算受影响 threads 的计数 })
  → 提交后 emit sync:progress / mail:new
```
**要点**：线程计数与索引入队**必须在同一事务内**，否则中断会留下"有邮件但永不索引"的悬空。

### 5.3 离线操作（pending_ops）收敛规则
- 用户动作 → 立即改本地（乐观）→ 写一条 `pending_ops`（`local_state=1`）。
  **⚠️ 第二轮评审修正**：这两步**不再声称"同一事务"** —— `messages.local_state` 在 `cache.db`、
  `pending_ops` 在 `user.db`，而双库是**两个独立句柄、不 ATTACH**（§11 反驳 5），跨库事务不存在。
  实际语义是"**两次顺序写，非原子**"：崩溃最坏留下"本地已改未入队"或"已入队本地未改"，
  两边都是幂等的，由**夜检 + 下一次同步**纠正（16 §4.2 的对账项之一）。
  顺序固定为**先写 `pending_ops` 再改本地**：宁可"已入队未改本地"（服务器最终会告诉我们真相），
  也不要"已改本地未入队"（用户以为改了、服务器永远不知道）。

- 队列 worker 按 (账号, 文件夹) 串行发送；成功 → 删 op、`local_state=0`。
- **冲突规则（诚实版）**：旗标/已读采用**以最新用户意图为准的 LWW**：op 带 `created_at`；
  若同步回来的服务器状态 `modseq/date > op.created_at` 且与 op 目标不同，则**以本地意图覆写服务器一次**
  （用户刚点的，用户最大），并在日志里记 `conflict`。删除/移动类操作不可 LWW（会产生幽灵邮件）：
  若服务器上该 UID 已经不存在 → op 直接判定成功；若服务器已把它移到别处 → op 判定为 conflict，
  UI 提示"操作未生效：这封邮件已在其它设备上被移动"。
- 队列连续失败 ≥ 5 次 → 横幅提示 + 设置页可见 + 手动重试。

### 5.4 配额清理
```
每日一次（或启动后 60 s，系统空闲时）：
  report = quota::report()
  if report.total > policy.max_bytes or 有超龄数据：
     plan = quota::plan(&policy)      # 干跑：算出要删什么、省多少
     记录 plan（诊断包可见）
     apply(plan)                      # 顺序：raw → 附件字节 → 正文 → 邮件头（仅当策略允许）
     emit job:done + toast("已清理 X MB")，设置页留痕
```
**永不触碰服务器**；用户可在设置里看到"最近一次清理：时间/释放空间/删了什么（前 20 条）"。

### 5.5 恢复与隔离
```
backup_restore(src):
  verify(src)  → 校验**两个库**的 schema_version ≤ 当前、各自 quick_check 通过、账号数 > 0
  copy 到 app_dir/{cache.db,user.db}.restore  → 写 kv("pending_restore") → 提示"重启完成恢复"
启动时 take_pending_restore():
  **两个库一起改名**（cache.db / user.db → *.pre-restore-<ts>）→ 再把 *.restore 改名回来
  → 各自跑迁移到当前版本；**任一步失败则两个都回滚**（避免出现"新 cache + 旧 user"的错配）
损坏隔离 quarantine_and_reset(**只针对 cache.db**):
  cache.db → cache.db.corrupt-<ts>（保留，用户可手动取回）
  新建空 cache.db → 提示"本地缓存已重建，正在重新同步"（账号/草稿/规则/联系人与**本机存档**都不受影响）
  **绝不隔离 user.db**（它是不可再生的用户数据；user.db 损坏时只能走备份恢复，见 16 §6）
```

## 6. 失败模式与降级

| 触发 | 用户看到 | 数据是否受损 | 恢复 |
|---|---|---|---|
| 磁盘满（ENOSPC） | 横幅"磁盘空间不足，已暂停同步与索引"+ 设置页显示可用空间 | 不丢：写入失败即回滚；队列保留 | 清理后自动续跑 |
| `SQLITE_BUSY` 持续 | 无感（重试）；持续失败提示"本地数据库正忙" | 否 | 自动 |
| 迁移失败 | "升级本地数据失败，已保留旧数据，可继续使用旧版本" | 否（旧库保留） | 回退程序版本或重试 |
| `quick_check` 失败 | 设置页红标 + "修复"按钮 | 可能部分行不可读 | 修复/重建索引/（最坏）隔离重建 + 重同步 |
| 索引行数对不上 | 无感；夜检自愈重插 | 否 | 自动重建差异部分 |
| 附件 blob 缺失（手工删了库里行） | 附件下载按钮重新可取 | 否 | 按需重取（sha 不匹配则更新） |
| 用户手工删了 `cache.db` | 下次启动提示"正在重新同步"（**注意：本机存档/POP3 的邮件不在这个库里**，见 §11 反驳 5 的三条规则） | 服务器账号的本地副本没了（可重取）；`user.db` 里的草稿/规则/联系人与本机存档都在 | 提示 + 建议先用备份 |

## 7. 迁移与兼容

- **版本线**：`db_schema_version`（SQLite `user_version` + `schema_migrations` 表）。v1/v2 的既有库（含 `history_complete`）必须能升到 v3，**不许重建**。
- **迁移清单（v3 起点）**：
  1. `v1→v2`：加 `folders.history_complete`（既有 PLAN 已定）。
  2. `v2→v3`：新建 v3 新表（`schema_migrations`/`kv`/`metrics`/`threads`/`attachments`/`blobs`/`drafts`/`outbox`/`pending_ops`/`sync_jobs`/`index_queue`/`rules`/`blocklist`/`allowlist`/`contacts`/`contact_emails`/`notification_log`/`saved_searches`/`labels`/`message_labels`）；
     给 `messages` 加列（`thread_id`/`message_id`/`in_reply_to`/`refs_json`/`subject_norm`/`internal_date`/`snippet`/`body_cached`/`raw_cached`/`indexed_at`/`modseq`/`local_state`/`is_junk`/`attachment_count`/`forwarded`/`answered`）；
     给 `bodies` 加列（`raw_zstd`/`raw_bytes`/`text_body`）；`accounts`/`folders` 加列；重建 `search`（分词器变更 → 必须重建，见 09）。
     **全部用 `alter table add column ... default`**（SQLite 支持），不重建表；重建 `search` 用 `drop table + create virtual table + 标记全量重索引`。
  3. 之后每次结构变化 = 一个新的迁移函数，附单元测试（从每个历史版本升级到最新）。
- **不可逆点**：`search` 重建（只丢索引，可重建）；`raw_zstd` 清理（可重取）；**没有不可逆的用户数据操作**。
- **兼容纪律**：所有新列有默认值；所有新表 `if not exists`；迁移只在启动时跑，运行中不变 schema。

## 8. 性能预算（存储层）

| 指标 | 预算 | 依据/手段 |
|---|---|---|
| 批量插入（头部） | ≥ 5 000 行/秒（单事务 500 行） | 事务 + 预编译语句 + WAL |
| 列表首页查询（10 万封） | ≤ 30 ms | `messages_list` 覆盖索引 + limit |
| 计数查询（未读/总数） | ≤ 10 ms | 索引 + 维护计数列（不做 count(*) 全表） |
| 会话重建（10 万封） | ≤ 60 s（后台，可中断） | 批处理 + 进度事件 |
| 索引写入（正文） | ≥ 20 封/秒（含解析与分词） | 批 50 封一事务；解析不在写线程 |
| 库体积（10 万封 / 2 年正文窗口） | ≤ 1.5 GB | 见 §3 raw 策略 + zstd |
| `quick_check` | ≤ 5 s（10 万封） | 启动异步执行 |
| `vacuum`（增量） | ≤ 30 s，仅空闲 | `pragma incremental_vacuum`，**前提是建库时就设了 `auto_vacuum = incremental`**（第二轮评审实测：在已有库上再设 `auto_vacuum` 会被静默忽略，`incremental_vacuum` 变成空操作）。**v1/v2 老库没有这个设置** ⇒ 首次升级后安排一次**显式、可中断、用户可见的全量 `VACUUM`**（带自己的预算与进度），否则"已清理 X MB"不会反映到磁盘 |
| 备份（1.5 GB） | ≤ 20 s | SQLite backup API（在线一致快照） |

## 9. 测试计划

| 层 | 用例 |
|---|---|
| 单元（`tests/storage.rs` 新） | 每个迁移函数：从 v1/v2/v3 空库/有数据库升级到最新，断言行列与数据完整；`quota::plan` 的干跑不写库；`heal::check` 能发现人为制造的孤儿/游标错位 |
| 事务原子性 | 注入"插到第 250 行时失败"，断言整批回滚（不能留半页） |
| 离线队列 | 断网做 100 次标已读/移动/删除 → 恢复网络 → 断言服务器最终状态一致、无重复发送、无幽灵邮件 |
| 索引一致性 | 随机删 bodies 行 → 夜检发现 → 自愈后对账一致 |
| 备份/恢复 | 备份 → 改动数据 → 恢复 → 断言回到备份时刻；损坏库（截断文件）→ 隔离重建不崩 |
| 配额 | 造 2 GB 夹具 → 设 500 MB 配额 → 干跑数字与实跑一致；断言**永远不会删服务器数据**（不能有网络调用，用假连接池断言零调用） |
| 性能 | §8 每项一条基准，纳入 `tools/bench.ps1` |
| 迁移回归 | 保留 v1/v2 的真实库样本（脱敏小库）作为夹具，随代码一起提交 |

## 10. 门禁新增项

1. **无 SQL 出现在 `mail-core` 之外**：`src-tauri` 与前端不得含 `select ... from`（防止绕过存储层）。
2. **迁移不可逆操作检查**：迁移文件里出现 `drop table messages|folders|bodies` 直接失败（只允许 drop `search`）。
3. **每个新表必须有 `if not exists`**（源码检查）。
4. **索引必须带 `INDEX_VERSION`**（第二轮评审修正）：原来这条把重建挂在"`tokenize` 字符串变化"上，但 D13 换成 CJK bigram 时改的是 **Rust 侧的 `to_index_text`**，`tokenize` 仍是 `unicode61` ⇒ 门禁不会触发，而索引其实必须重建。改成：索引内容算法（分词、截断阈值、字段集合）任何变化都必须**同时**升 `INDEX_VERSION`，`INDEX_VERSION` 变化 ⇒ 强制后台重建（门禁比对常量）。
5. **rb 测试：`cargo test -p mail-core storage` 必须包含至少一个"迁移失败回滚"用例**（测试名约定 `migration_failure_*`）。

## 11. 逆向反驳（红队）

**反驳 1：账号同时存在 `accounts.json` 和 `accounts` 表 = 两个真相，早晚不一致。**
- 承认。缓解：json 是唯一真相，表是**派生镜像**，启动时 `reconcile` 对账；
  所有写路径只写 json，再由 reconcile 同步进表（而不是两处都写）。
- 让步：如果实现时发现 reconcile 复杂，退路是**表里只存 `id` + 统计用字段**（不含 host/port 等配置），
  减小不一致面。这一点在设计里就允许降级，避免实现期卡住。

**反驳 2：FTS 手动维护一定会不一致（漏索引、重复行）。**
- 证据：既有实现就是手动 delete+insert，且已出现"upsert 两次会不会重复"的担心（旧测试覆盖了）。
- 缓解：把"索引"变成**队列驱动**（`index_queue`）+ 夜检对账（`fts::consistency`）+ 一键重建。
  写路径不再"顺手索引"，而是"登记待索引"。这样中断安全、可观测、可修复。

**反驳 3：`raw_zstd` 会把库吹到几 GB，用户会骂。**
- 证据：10 万封 × 60 KB = 6 GB 上限。
- 缓解：默认只保留 ≤ 512 KB 且压缩；配额清理优先删它；`storage_report` 里单独列出；
  设置项 `keep_raw` 可选 `none`。并把"删了原文会影响什么"（导出/转发需重取）写清楚。

**反驳 4：`pending_ops` 的 LWW 冲突规则会产生"我以为删了，其实没删"。**
- 承认这是本地优先模型的固有代价。
- 缓解：**破坏性操作不 LWW**（§5.3）：删除/移动遇到服务器已变化时判 conflict 并明确告知，
  绝不静默覆盖。UI 上 `local_state` 有可见标记（云朵/感叹号）。

**反驳 5（评审后二次修正）：用户删掉 `cache.db` 会丢草稿和待发送邮件 —— 而 README 现在教用户"不想要就直接删"。**
- 这是**真实的严重问题**：草稿与发件箱属于"用户数据"，不是"缓存"。
- **第二层问题（第二轮评审抓到的更严重的一条）**：`accounts.kind ∈ {local, pop3}` 的账号里，
  邮件**没有任何服务器副本**（导入的本机存档、POP3 收下来的邮件）。如果它们躺在"可删可重建"的
  `cache.db` 里，那么三件日常操作都会变成**不可恢复的数据销毁**：手工删库、§5.4 的配额清理
  （顺序 raw→附件→正文→邮件头）、§5.5 的隔离重建（"新建空库 → 正在重新同步"根本无从同步）。
  这直接违反红线 3。
- **最终归属（三条规则）**：
  1. **`messages` 增加 `origin text not null default 'server'`**（`server | imported | pop3`）——
     这是全库最重要的一个标记位（见 §3 的 messages 定义与 06 §3）。
  2. **`origin <> 'server'` 的邮件与其 bodies/attachments/blobs 一律进 `user.db`**：
     实现上不搞"同表两库"，而是**按账号 kind 选句柄**——本地/POP3 账号走 user 句柄写，
     服务器账号走 cache 句柄写；`repository` 按 `account_id → kind` 路由（见 §4.2 的 `DbSet`）。
  3. **`quota::plan/apply`、`heal::quarantine_and_reset`、`invalidate_folder` 全部按 `origin` 排除**，
     并且"清理缓存"这个用户动作只清 `origin='server'` 的行；**删除本机存档是单独动作，需二次确认并报出封数**。
- README 口径随之改成：**"服务器账号的本地副本可以在设置里一键清理；本机存档（导入的邮件、POP3 邮件）
  是不可再生的，删除需要单独确认。"** —— 不再说"cache.db 可以随便删"。

  - `cache.db`（可删可重建）：folders/messages/bodies/attachments/blobs/threads/search/index_queue/
    sync_jobs/metrics/sent_index/import_dedupe/**labels/message_labels**（都是从服务器可再生的数据）
  - `user.db`（不可删，纳入备份）：drafts/outbox/**pending_ops**/rules/blocklist/allowlist/
    contacts/contact_emails/saved_searches/search_history/**import_jobs**/notification_log/kv/
    signatures/templates
  **归属的两条判据**：①"用户意图/不可再生"的进 `user.db`（**`pending_ops` 属于用户意图**：
  删缓存不该悄悄撤销用户点过的"标已读/移动"）；②"能从服务器或缓存重建"的进 `cache.db`
  （标签分配、索引、会话、游标、统计）。
  **连接方式（评审后改定，比 ATTACH 更简单）**：**两个独立句柄，不做 `ATTACH`**。
  理由：①`pragma synchronous` 是**按连接**生效的（同一连接里的两个库无法一个 `full` 一个 `normal`）；
  ②rusqlite 0.40 没有 `attach()` 封装，附着库还要用 `new_with_names` 才能备份；
  ③**实测各模块的写路径，没有任何一处需要跨库事务**（规则/旗标写 `messages`+`pending_ops`
  横跨两库，但两边都是幂等的，崩溃最坏留下"本地已改未入队"或"已入队本地未改"，都会被下一次同步纠正
  —— 这条降级写进 16 §4.3）。
  **决定：采用双库 + 双句柄**；`user.db` 用 `synchronous=full`（草稿/发件箱不能因掉电丢），
  `cache.db` 用 `normal`（可重建，性能优先）。README 同步改口径：
  **"服务器账号的本地副本可在设置里一键清理；本机存档（导入的邮件、POP3 邮件）不可再生，删除需单独确认。"**
  **POP3/本地账号的特殊规则**：`accounts.kind ∈ {pop3, local}` 的账号**不写 `pending_ops`**
  （服务器上没有可执行的动作），本地旗标/已读只在本地生效，UI 必须注明"不会同步到服务器"（18 §5.1）；
  它们的 messages/bodies/attachments/blobs **按 `origin <> 'server'` 规则写进 `user.db`**（见反驳 5 的三条规则）。
- 这一条是红队最值得记的两次纠正（第一次拆库，第二次发现"本机存档放可删库里"仍是数据销毁）。


**反驳 6：双库会让备份/迁移/`VACUUM` 复杂化。**
- 证据：`VACUUM` 是单库的，备份要分别做 —— 但因为**不 ATTACH**（见反驳 5 的改定），
  复杂度比"附着方案"低：每个库各自 `VACUUM`、各自迁移、各自备份，`Db` 层把两个句柄包成一对。
- 缓解：备份 = 两个库顺序备份 + 一个 manifest（同一时间点；备份期间暂停写入）；
  迁移分库执行（`cache`/`user` 各自一套版本号，同一个迁移器）；恢复时两个一起换。
- **实现前置（实测校正）**：备份要用 `rusqlite::backup::Backup`，它**在 `backup` feature 之后**；
  当前 `crates/mail-core/Cargo.toml` 的 rusqlite 只有 `features = ["bundled"]` ⇒ 实现备份前必须加 `"backup"`。
- **`synchronous` 差异**：两个库各自设（`user.db`=full、`cache.db`=normal）。
  **诚实更正**：评审实测"ATTACH 下按库设 `pragma user.synchronous` 其实也能生效"，所以**这不是**放弃 ATTACH 的理由；
  真正的理由是**备份/隔离/清理/VACUUM 都要按库独立操作**，且没有任何写路径需要跨库原子性。

**反驳 7：`local_state` 冗余（`pending_ops` 里已经有）会不同步。**
- 缓解：它只是**查询优化列**（列表要显示云朵），由 `pending_ops` 的写路径维护
  （**注意：双库下这是"两次顺序写、非原子"**，见 §5.3 的修正）；夜检对账
  （`heal::check` 比对 `pending_ops` 与 `local_state` 的差集）并可自动修复。

**反驳 8：把 bcc 排除在外会影响"发件箱重发"的完整性。**
- 澄清：`bcc` 存在 `outbox.payload_json` 与 `drafts.bcc_json` 里（发送需要），
  只是**不回写进 `messages`**（服务器也不会把 bcc 还给我们）。这是正确的。

## 12. 未决问题

| # | 问题 | 影响 | 建议 |
|---|---|---|---|
| S1 | 双库拆分是否现在做（§11 反驳 5） | 备份/迁移/vacuum 复杂度 | **现在就做**：越晚越贵（草稿已存在用户机器上时再拆要迁移） |
| S2 | `raw_zstd` 的大小阈值（512 KB？）与压缩级别 | 体积 vs CPU | 先用 512 KB + zstd level 3，实测后调（15） |
| S3 | 会话计数是物化还是查询时算 | 列表性能 | 物化 + 增量维护 + 夜检（08 定算法） |
| S4 | SEC-11 整库加密（SQLCipher）是否做 | 依赖与许可证 | M3 评估；本设计预留 `DbOptions.encryption` |
| S5 | 附件字节缓存是否默认开启 | 磁盘 | 默认只缓存**已打开/已保存**的附件；预取是选项（04） |

## 13. 验收清单

- [ ] v1/v2 的真实库样本能升级到 v3，邮件数、游标、`history_complete` 全部保持。
- [ ] 删掉 `cache.db` 后重启：账号、凭据、**草稿、规则、联系人、待发送邮件**全部还在。
- [ ] 10 万封夹具下 §8 的八项预算达标。
- [ ] 注入式失败测试：同步中断、索引中断、迁移失败、磁盘满，四种情况下都不留半写、不丢队列。
- [ ] 配额清理干跑与实跑数字一致，且**零网络调用**（假连接池断言）。
- [ ] 备份/恢复/隔离重建三条路径各有一个可重复的测试脚本。
- [ ] `check.ps1` 的五项新增检查通过。
