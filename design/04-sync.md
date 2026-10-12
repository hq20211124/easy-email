# 04 同步引擎 v2

> **目标**：把"最新 500 封 + 按需回填"升级成**用户可选窗口、全程可暂停可取消、断网自愈、
> 不为了一条推送把服务器惹毛**的同步引擎；并让"邮件已经在本地、可以搜、能离线改"这件事
> 在 10 万封规模上依然成立。
>
> 覆盖 ID：SYN-01…10、SYN-14/15/16、ACC-14（账号级策略）、MIG-05（从服务器全量重同步）。
> 上游：02（调度与线程模型）、03（表与事务）。下游：08（列表/会话）、09（索引）、13（通知）。

---

## 1. 目标与非目标

**目标**
1. **窗口可选**：全部 / 最近 N 年 / 每文件夹最近 N 封 / 仅按需（SYN-01），逐账号、逐文件夹可覆盖。
2. **零丢信**：任何中断、限流、重编号都不产生空洞（SYN-02）。
3. **可掌控**：进度可见、可暂停、可取消、可限速（SYN-07）。
4. **礼貌**：连接数、并发、请求速率都在服务商脾气之内（SYN-08）。
5. **自愈**：断网、休眠、换网、服务器重启后自动收敛（SYN-09）。
6. **多设备一致**：旗标/已读/删除以服务器为准，本地意图用离线队列补偿（SYN-14）。
7. **新邮件亚秒级可见**（A1-F41 的实测结论：IDLE 的 `EXISTS` 是能拿到的；这条把"50 秒延迟"从产品限制变回实现缺陷）。

**非目标**
- 不做"服务端全文搜索"。服务器 SEARCH 只作为本地没命中时的**回退**（09）。
- 不做 P2P/局域网内同步（多设备只经服务器）。
- 不在 M1 做 QRESYNC/CONDSTORE 的完整实现：先做**能力探测 + 记录 modseq**，优化放 M3（SYN-15）。

## 2. 用户可感知行为

| 场景 | 用户看到 |
|---|---|
| 首次加账号 | 立刻出现最新 200 封（≤ 5 s，随网络）；顶部细进度条"正在同步 1,240/8,731"，可点开看细节 |
| 想全都要 | 设置 → 该账号 → 同步："下载范围：全部 / 最近 2 年 / 最近 1000 封 / 仅按需"。改成"全部"后提示"预计下载 3.2 GB、约 4 小时（可随时暂停）" |
| 正在同步 | 可以关窗口、可以切文件夹读信、可以搜索已有内容；进度条在状态栏，点开可暂停/继续/取消 |
| 断网 | 状态栏"离线（显示本地邮件）"，所有读与整理照常，写信进发件箱；恢复后自动补发 + 补齐 |
| 服务器限流 | 不弹错；日志里退避；状态栏显示"服务器繁忙，稍后重试（1 分钟后）" |
| 文件夹被服务器重编号 | 提示"该文件夹被服务器重新编号，正在重建本地副本（12/30）"，不丢服务器邮件 |
| 只想让本地小一点 | 设置 → 存储：改窗口或配额，出现"将删除本地 8,412 封正文（服务器不受影响）"+ 确认 |
| 多账号 | 每个账号独立进度、独立窗口、可单独"暂停同步"（通知仍可保留） |

## 3. 数据结构

新增/使用（定义见 03，这里给语义）：

```rust
/// 同步窗口（账号级默认，文件夹可覆盖）
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
pub enum SyncWindow {
    All,               // 全量
    Years(u16),        // 最近 N 年（默认 2）
    Count(u32),        // 每文件夹最近 N 封
    OnDemand,          // 只取最新一页，其余按需回填（v0.2 行为 + 回填）
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub struct SyncPolicy {
    pub window: SyncWindow,
    /// 正文预取：None 不预取 / Opened 只取打开过的 / Window 窗口内全部（默认）/ All
    pub body_prefetch: BodyPrefetch,
    /// 附件预取：None（默认）/ SmallOnly{max_bytes} / Window
    pub attachment_prefetch: AttachmentPrefetch,
    /// 限速：None 不限 / Pace{ headers_per_min, bodies_per_min }
    pub bandwidth: BandwidthPolicy,
    /// 只在这些时间同步（例如"仅白天"），空 = 任意时间
    pub active_hours: Option<(u8, u8)>,
    /// 计费网络（Wi-Fi 才自动同步）
    pub wifi_only: bool,
}

/// 任务断点：窗口回溯与按需回填都往"更早"走，与 folders.last_uid（前进游标）正交
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct JobCursor {
    /// 下一次向更早取件的上界（取 uid < before_uid）
    pub before_uid: u32,
    /// 已确认到底
    pub bottom_reached: bool,
    /// 窗口回溯的停止条件快照（避免用户中途改设置导致语义漂移）
    pub stop: StopCondition,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub enum StopCondition {
    UidBelow(u32),          // All：取到 uid == 1（或空区间）
    OlderThan(i64),         // Years：日期早于该时间戳即停
    ReachedCount(u32),      // Count：本地该文件夹已达 N 封
}

/// 服务器能力探测结果（每账号+host 缓存进 kv，避免每条连接都探测）
///
/// **第二轮评审删掉了四个字段**：`thread_references` / `list_status` / `namespace` / `qresync`。
/// 原因：这些响应在 imap-proto 0.17 里**没有语法**，服务器一旦回应就会让连接被判为解析失败
/// （A1-F42）⇒ 我们**永远不会发**这些命令，探测它们等于自找断连。
/// 能力位只保留"我们会真的用到"的。
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct ImapCapabilities {
    pub id: bool, pub idle: bool, pub uidplus: bool, pub r#move: bool,
    pub condstore: bool, pub sort: bool, pub utf8_accept: bool,
    pub special_use: bool, pub compress: bool,
    /// UID SEARCH 支持 CHARSET UTF-8（服务器端搜索回退要用，见 09）
    pub search_utf8: bool,
    /// STATUS 可用性（多文件夹监听要用；部分国内服务器不支持 ⇒ 探测一次后永久降级）
    pub status_ok: bool,
    /// **EHLO/SELECT 快照里要记住的 SMTP/IMAP 限额**（07 §4 的发送前预检只用这份快照，
    /// 不在撤销窗口里偷偷建连接 —— 第二轮评审）
    pub smtp_utf8: bool, pub smtp_8bitmime: bool, pub smtp_size: Option<u64>,
    /// 探测到的时间（能力可能随服务器升级变化，30 天过期重探）
    pub probed_at: i64,
}
```

`sync_jobs`（03 已定义）承担"可暂停/可续跑"；`index_queue` 承担正文/附件索引；
`folders.last_uid` 只由**增量（前进）**路径写。

## 4. 接口（函数级）

### 4.1 IMAP 原语（`mail-core::net::imap`，在既有实现上扩展）

```rust
impl ImapClient {
    /// 一次性探测能力（登录后调用一次；结果缓存到 kv）
    pub async fn probe_capabilities(&mut self) -> Result<ImapCapabilities>;

    /// 前进增量：uid > after_uid 的头部（既有，保持"截断保留最旧一批"的修复）
    pub async fn fetch_headers_since(&mut self, mailbox: &str, after_uid: u32, limit: usize) -> Result<HeaderPage>;

    /// 最新窗口（既有）
    pub async fn fetch_newest_headers(&mut self, mailbox: &str, limit: usize) -> Result<HeaderPage>;

    /// **闭区间**取件：uid ∈ [low, high] 的头部。窗口回溯的主力原语（新增）
    ///
    /// 为什么不用 SEARCH：`UID SEARCH ALL` 要返回全部 UID（10 万封 ≈ 600 KB 文本），
    /// 每翻一页问一次不可接受；闭区间既便宜又不需要知道总数。
    /// 两端都显式给出，因此**不会**踩到"`<n>:*` 起点超过最大 UID 时返回最后一封"的 RFC 陷阱。
    /// 返回的页若为空，说明该区间内没有邮件（对 All 窗口即"到底了"）。
    pub async fn fetch_headers_range(&mut self, mailbox: &str, low: u32, high: u32) -> Result<HeaderPage>;

    /// 按需回填（既有，SEARCH 版）：能精确回答"是否到底"，用于 OnDemand 模式
    pub async fn fetch_older_headers(&mut self, mailbox: &str, before_uid: u32, limit: usize) -> Result<OlderPage>;

    /// 只读探测其它文件夹是否有变化（不 SELECT，代价极低；用于多文件夹监听）
    pub async fn status(&mut self, mailbox: &str) -> Result<MailboxStatus>;   // uidnext, messages, unseen

    /// 有界 IDLE 窗口；返回后由调用方 DONE + SELECT/STATUS 比对
    pub async fn idle_window(self, window: Duration, mailbox: &str) -> Result<(Self, u32)>;

    /// 服务器端搜索（回退用，见 09）
    pub async fn search_uids(&mut self, mailbox: &str, criteria: &SearchCriteria) -> Result<Vec<u32>>;

    /// 取单封（正文）：保持"整封取回 + 解析"的既有做法
    pub async fn fetch_message(&mut self, uid: u32) -> Result<MessageDetail>;
}
```

### 4.2 调度器（`mail-core::sync::scheduler`）

```rust
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum JobPriority { UserAction = 0, NewMail = 10, WindowSync = 20, BodyPrefetch = 30, Index = 40, AttachmentPrefetch = 50, Maintenance = 60 }

pub struct JobRequest {
    pub account_id: String,
    pub folder: Option<String>,
    pub kind: JobKind,           // Initial | Incremental | WindowBackfill | BodyPrefetch | Index | RebuildThreads | AttachmentPrefetch
    pub priority: JobPriority,
    pub reason: JobReason,       // User | Idle | Timer | Resume | Retry
}

#[derive(Clone)]
pub struct SyncScheduler { /* Arc<Inner> */ }

impl SyncScheduler {
    pub fn new(budget: NetBudget) -> Self;
    /// 入队（幂等）：同 (账号,文件夹,kind) 已有排队/运行中的任务则直接返回它的 id
    pub async fn request(&self, req: JobRequest) -> JobId;
    pub async fn cancel(&self, id: &JobId) -> bool;
    pub async fn pause(&self, id: &JobId) -> bool;
    pub async fn resume(&self, id: &JobId) -> bool;
    pub async fn pause_account(&self, account_id: &str);
    pub async fn resume_account(&self, account_id: &str);
    pub async fn set_policy(&self, account_id: &str, policy: SyncPolicy);
    /// UI 用：所有任务的快照（进度、阶段、速率、错误、下次重试）
    pub async fn snapshot(&self) -> Vec<JobSnapshot>;
    /// 常驻调度循环：必须在 `tauri::async_runtime` 上下文里 spawn（见 HANDOFF §3）
    pub async fn run(self: Arc<Self>, ctx: Arc<AppCtx>);
}

pub struct NetBudget {
    /// 每账号并发网络作业上限。**第二轮评审修正公式**：
    /// `per_account_jobs = max(1, max_connections - idle_holders)`（默认 idle_holders = 1）。
    /// 因为 `providers.rs` 里网易系是 `max_connections = 2` ⇒ 2 - 1 = 1（原设计的"默认 2"
    /// 会得到 2 作业 + 1 IDLE = 3 > 2，直接违反服务商限制，也会让 §10.3 的一致性门禁当天失败）。
    /// **只给 1 条连接的账号**（部分自建/企业）：同步期间**暂停 IDLE**，或降级为 L3 轮询 —— 必须显式选一条，
    /// 不能留 undefined。
    pub per_account_jobs: usize,
    /// 全局并发作业上限（默认 4；避免多账号一起抢带宽）
    pub global_jobs: usize,
    /// 每账号令牌桶：请求/秒（默认 20，可配）
    pub req_per_sec: u32,
    /// 单次命令超时（默认 60 s）
    pub command_timeout: Duration,
    /// 单连接账号的策略（`max_connections == 1` 时生效）
    pub single_connection_policy: SingleConnPolicy,   // SuspendIdleWhileSyncing | PollOnly
}

pub struct JobSnapshot {
    pub id: JobId, pub account_id: String, pub folder: Option<String>, pub kind: JobKind,
    pub state: JobState, pub phase: &'static str, pub done: u64, pub total: u64,
    pub rate_per_sec: f32, pub eta_secs: Option<u64>, pub error: Option<ApiError>,
    pub next_retry_in_ms: Option<u64>,
}
```

### 4.3 同步任务主体（一个任务的完整流程）

```rust
/// 每个任务的入口；内部按 kind 分派，统一处理暂停/取消/进度/错误。
pub async fn run_job(ctx: Arc<AppCtx>, job: JobRow, cancel: CancelToken, progress: ProgressSink) -> Result<JobOutcome>;

/// 首次同步：先给"最新一页"，再按窗口回溯（两阶段，用户 5 秒内可用）
pub async fn initial_sync(ctx: &AppCtx, account: &AccountConfig, folder: &str, job: &JobCtx) -> Result<()>;

/// 前进增量：uid > last_uid（既有逻辑，纳入任务框架）
pub async fn incremental_sync(ctx: &AppCtx, account: &AccountConfig, folder: &str, job: &JobCtx) -> Result<SyncOutcome>;

/// 窗口回溯：按 StopCondition 一批批往更早取（区间取件，不碰 last_uid）
pub async fn window_backfill(ctx: &AppCtx, account: &AccountConfig, folder: &str, job: &JobCtx) -> Result<()>;

/// 正文预取：按策略批量取正文并落库 + 入索引队列（与"打开邮件"共用同一个函数）
pub async fn body_prefetch(ctx: &AppCtx, account: &AccountConfig, job: &JobCtx) -> Result<PrefetchReport>;

/// 取正文并落库（唯一入口：手动打开、预取、导出都走它，保证行为一致）
pub async fn fetch_and_store_body(ctx: &AppCtx, account: &AccountConfig, key: &MsgKey, opts: BodyFetchOptions) -> Result<BodyRow>;

/// 把同步结果落库（**一批一个事务**：头部 + 游标 + 索引入队 + 会话计数）
pub async fn commit_page(db: &Db, account_id: &str, folder: &str, page: &FetchPage, indexed_hint: IndexHint) -> Result<CommitReport>;
```

### 4.4 监听（`mail-core::sync::watch`）

```rust
pub struct WatchSet { pub account_id: String, pub folder: String, pub interval: Duration }

/// 一个账号一条连接的监听循环：IDLE（选中文件夹）+ 窗口结束后 STATUS 其它文件夹
pub async fn watch_account(ctx: Arc<AppCtx>, account: AccountConfig, initial: Vec<String>, cancel: CancelToken) -> Result<()>;

/// 一次"探测周期"：返回发生变化的文件夹（供调度器入队增量同步）
pub async fn probe_cycle(client: &mut ImapClient, selected: &str, watching: &[String], idle_window: Duration) -> Result<Vec<FolderChange>>;

pub struct FolderChange { pub folder: String, pub messages_before: u32, pub messages_after: u32, pub uidnext_before: u32, pub uidnext_after: u32 }
```

### 4.5 网络状态与自愈

```rust
#[derive(Clone, Debug)]
pub enum NetState { Online, Degraded { since: i64, reason: String }, Offline { since: i64, reason: String } }

pub struct NetMonitor { /* Arc<Inner> */ }
impl NetMonitor {
    pub fn new() -> Self;
    pub fn state(&self) -> NetState;
    pub fn subscribe(&self) -> tokio::sync::broadcast::Receiver<NetState>;
    pub async fn run(self: Arc<Self>, ctx: Arc<AppCtx>);   // 轮询 + 失败驱动 + 网络变化事件
}
```

## 5. 状态机与主流程

### 5.1 任务生命周期
```
queued ──scheduler picks──▶ running ──batch ok──▶ running … ──done──▶ done
   ▲                          │  │
   │                     pause│  │cancel
   │                          ▼  ▼
   └──resume (job 保留游标)── paused   cancelled
                              │
                         失败(可重试) ──退避──▶ queued(next_attempt_at)
                         失败(不可重试) ──▶ failed（UI 显示原因 + 重试按钮）
```
**取消/暂停的语义**：只在**批次边界**生效（一批头部 500 或正文 50），批次内不打断；
每批都是一个事务，所以任何时刻取消都不会留半写。

### 5.2 首次同步（两阶段，目标"5 秒可用"）
```
1. connect + probe_capabilities（缓存）
2. LIST → folders 落库（含 role 本地化，既有 folders.rs）
3. SELECT → uidvalidity/uidnext/exists
4. 阶段 A（关键路径）：fetch_newest_headers(200) → commit_page → emit mail:new/folders ready
   → 前端此时已可列表 + 读信（读信按需取正文）
5. 阶段 B（后台任务）：按 SyncPolicy.window 做 window_backfill（区间取件，批量 1000）
   → 同时 body_prefetch（默认 Window：窗口内正文，限速 20 封/秒）
   → 索引跟随（09）
6. 全部完成后写 folders.last_sync_at，托盘更新未读
```
**为什么阶段 A 是 200 而不是 500**：首屏只需要一页；其余交给有进度、可取消的后台任务，
比"一次拉 500 再显示"体验好，也避免大文件夹首屏等待。

### 5.3 增量（前进）与窗口（后退）的边界规则
| 路径 | 游标 | 写入字段 | 备注 |
|---|---|---|---|
| Initial | 无 | `last_uid = max(uid)`、`total_count` | 只走最新窗口 |
| Incremental | `last_uid` 前进 | `last_uid = max(uid)` | **截断必须保留最旧一批** —— ⚠️ **这是要修的 bug，不是"既有修复"**（第二轮评审核对代码：`imap.rs` 现在 `split_off(len-limit)` 保留的是**最新**一批，而 `advance_cursor` 把游标推到最大已存 UID ⇒ 一次来 500 封以上就会在 `last_uid+1` 与保留批之间留下**永久空洞**）。修法：`summaries.truncate(limit)`（或把请求区间收窄成 `low=last_uid+1, high=min(low+limit-1, uidnext-1)`）。**这条已列进 M0**（19 §4） |
| WindowBackfill | `sync_jobs.cursor_json.before_uid` 后退 | 只插行、不动 `last_uid`、到底时 `history_complete=1` | 与回填同规则（既有 PLAN） |
| OnDemand（用户点"加载更早"） | 同上 | 同上 | 保留 SEARCH 版原语以获得精确 exhausted |

**不变式（写进门禁）**：`folders.last_uid` 只能由 Initial/Incremental 写；
WindowBackfill/OnDemand 的代码路径里不得出现写 `last_uid` 的调用。

### 5.4 监听周期（一条连接看多个文件夹）
```
cycle:
  if 有变化待处理 → 交给调度器（不在这里同步）
  1. SELECT <主文件夹>（默认 INBOX），记录 exists/uidnext
  2. IDLE <idle_window>（默认 **120 s，且外层再包一层硬超时**；及时性由 L1 的 NewData 负责，
     这个窗口只是心跳与"连接还活着吗"的兜底）→ DONE
  3. 重新 SELECT（或 NOOP 后读 exists）比对 → 主文件夹是否变化
  4. 对 watching 里其它文件夹逐个 STATUS (UIDNEXT MESSAGES UNSEEN) → 比对缓存值
  5. 变化的文件夹 → emit 内部事件 → 调度器入队 Incremental（优先级 NewMail）
     **⚠️ 入队前必须再查一次持久化的游标**（`folders.uidnext` / `last_uid`），
     而不是拿本轮开始时的快照比：L1 会在收到 `NewData` 时**立刻**触发一次同步，
     等心跳走完再拿旧快照比就会**再触发一次**（第二轮评审指出的双发；
     既有实现 `idle.rs` 是在上报事件的那一刻就刷新基线，设计里要把这个细节写下来）。
  6. 回 1，周期之间加抖动（±10%），避免同一时刻所有账号一起打服务器
```
**分层降级**（本设计对"库能力"的正面回应，**已按 A1-F41 的实测更正**）：
- **L1（默认，就是现在能用的）**：服务器推 `* n EXISTS` → **`IdleResponse::NewData` 在 0.12.0 上确实会到达（实测 ~206 ms）**，
  立刻入队增量同步。**通知延迟 ≈ 亚秒级**。
  ⚠️ 这条推翻了 `HANDOFF.md` §5 与 `imap.rs`/`idle.rs` 注释里的旧结论（那是 0.12.0 之前的观察）。
  **实现第 1 步（0.5 天）**：在 loopback 夹具加断言"推送 `* n EXISTS` 后 1 秒内必须收到 `NewData`"，
  确认后删除那两处错误注释，并把 README 的"推送最多 50 秒延迟"改成"亚秒级"。
- **L2（心跳兜底，仍然保留）**：即使有 L1，也要有周期性 `DONE` + 重新 `SELECT` 比对，
  因为 ①服务器可能不发 EXISTS（部分服务商）②连接可能已经死了但没人发现。
  窗口默认 120 s（比原来的 50 s 更长，因为 L1 已经承担了及时性），
  **必须自己在外面再包一层 `tokio::time::timeout`**：`wait_with_timeout(dur)` 是"每个响应各计一次超时"，
  服务器发 `* OK Still here` 就能无限拖住它（A1-F41）。
- **L3（服务器不支持 IDLE）**：定时 `STATUS`/`NOOP` 轮询（默认 3 分钟）。
- 能力探测决定用哪层；设置页显示"新邮件延迟"的实际情况（L1 时写"即时"，L3 时写"最多 3 分钟"）。
- **绝对不要发的命令**：`THREAD`、`ESEARCH`、`NAMESPACE`、`LIST-STATUS`、`BINARY`、`SETMETADATA` ——
  imap-proto 0.17 没有这些响应语法，**一条这样的响应会让整条连接被关掉**（A1-F42）。
  需要会话串就本地算（08）；需要排序与条件查询用 `UID SEARCH` 的复杂条件（字符串透传，可用）。


### 5.5 断网/恢复

> **实现注记（第 80 轮）**：`pending_ops` 表**从第一个 schema 就存在、且长期无人写入**
> （界面上的"0"是诚实的，功能是缺席的）。本轮补齐存储层：入队（同意图幂等，
> 原子性来自"单写线程"而非从未存在的唯一索引）、到期查询、compare-and-set 认领、
> 退避重试（与 outbox 共用曲线）、崩溃 requeue、取消、以及**只数剩余工作量**的
> `pending_ops_outstanding`（原先 `SyncStatus` 喂的是裸行数，done/cancelled 也算 ⇒ 只涨不跌）。
> **刻意"不猜"的两处**：未知 kind 读成 `None` 且行留着；坏 payload 读成 `None`。
> 仍未接线：重放执行器与 IMAP 侧（mark_read/move/delete 的失败入队）是下一片。
```
command 失败(IO/timeout) → NetMonitor 记 Degraded → 当前任务退避重排
连续失败 ≥ 3 或系统报告无网络 → Offline：
   - 停所有网络作业（保留 IDLE 循环以最快感知恢复）
   - UI 状态栏"离线"，所有本地功能照常
   - outbox/pending_ops 保持排队，不发无用请求
恢复（任一命令成功 或 网络事件）→ Online：
   - 触发一次全账号 Incremental（优先级 NewMail）
   - flush outbox（先）→ flush pending_ops（后，保证"先发信再改旗标"的直觉顺序）
   - 恢复被暂停的 WindowBackfill / prefetch
```

### 5.6 UIDVALIDITY 变化（服务器重编号）
```
**⚠️ 第二轮评审修正：UIDVALIDITY 要在拿到 SELECT 结果的那一刻就比，而不是等到写库时。**
原设计把它放在 `commit_page` 里，后果是"重编号但 `last_uid` 之上没有新邮件"的增量页是空的，
于是跳过 `commit_page` ⇒ **永远检测不到重编号，文件夹永久陈旧且不报错**。
正确形状（既有 `sync.rs::absorb` 就是这么做的，别在重写里丢掉）：
  1. 任何 SELECT 返回 uidvalidity 后，**先与 `folders.uidvalidity` 比对**（两者非 0 才比）
  2. 不一致 → invalidate_folder（删 messages/bodies/attachments/索引，保留 folders 行；history_complete=0）
     → 入队 Initial → 完成后 emit folders:rebuilt
  3. `FolderChange` 增加 `uidvalidity: u32` 字段（否则监听路径也判断不了）
  4. 若正在读的邮件被重建影响，UI 提示"这封邮件的位置已变化，正在重新定位"
```

### 5.7 配额感知调度（研究结论 A1-F1/F2/F3 直接催生的一节）

**硬事实**：Gmail 的 IMAP **下载上限约 2.5 GB/天**（超了会被限到近乎不可用，24 h 后重置）；
网易企业邮三方客户端的**收取范围默认只有最近 30 天**（不改网页端设置就永远取不到更早的）；
各家的 APPEND 上限从 10 MB（Exchange）到 45 MB（O365）不等。**这些不是我们能优化的工程问题，是配额。**

**设计**
```rust
/// 每服务商/每账号的配额账本（**持久化进 `user.db` 的 `quota_ledger`**，跨重启累计 ——
/// 第二轮评审：放在可删的 cache.db 里，删缓存就会忘掉"今天已经用了多少"，
/// 于是立刻重新超配额下载，正好触发 Gmail 限流）
#[derive(Clone, Debug)]
pub struct QuotaLedger {
    pub account_id: String,
    /// 当日已下载字节（只计正文与原文，头部的量级可忽略但也要记）
    pub bytes_today: u64,
    /// 当日预算（None = 不限；Gmail 默认 2_300_000_000 ≈ 2.3 GB，留 8% 余量）
    pub daily_budget: Option<u64>,
    /// 当日首次请求的时间（用于"本地日"边界；用账号时区或设备时区）
    pub window_start: i64,
    /// 是否已因服务商限流被罚（记录退避到什么时候）
    pub throttled_until: Option<i64>,
    /// 该账号观测到的"取不到更早邮件"的下界（网易 30 天窗口会体现为这个）
    pub server_floor: Option<i64>,
}

pub trait QuotaPolicy { fn budget_for(&self, provider_id: &str, account: &AccountConfig) -> Option<u64>; }
```

**行为**
1. 每个网络批次落库后累加 `bytes_today`；达到预算 → **主动停**（不是等服务器拒绝），
   把 `BodyPrefetch`/`WindowBackfill` 任务转为 `paused` 并设 `next_attempt_at = 次日 00:05`。
2. UI 文案（必须照抄，别改写成技术腔）：
   - 预算用尽：`今日同步额度已用完（Gmail 每天约 2.3 GB）。明天自动继续，也可以在设置里改成"只同步最近 90 天"。`
   - 触到服务器限流：`服务器暂时限制了同步频率（1 小时后重试）。`
3. **加账号向导必须做"体量预检"**（阶段 A 完成后即可估算）：
   用 `UIDNEXT`/`EXISTS` + 已取头部的平均大小估算总字节 →
   `全部大约 8.4 GB：在 Gmail 上按每天 2.3 GB 计约 4 天，首屏现在就能用。`
   并提供三个一键选项：**只要最近 90 天 / 最近 2 年 / 全部（后台慢慢来）**。
4. **网易系的服务端下界**：窗口回溯取到某个日期后再也取不到更早 → 记 `server_floor`，
   并给一次**可操作**的提示：`该邮箱的企业策略只允许第三方客户端读取最近 30 天的邮件。
   如需更早，请在网易网页版设置里把"收取范围"改大后回到这里重试。`
   —— 这条比"同步不到"重要得多：**用户会以为是我们的 bug**。
5. 配额账本进诊断包（`diagnostics_export`），排障时一眼看到"今天是配额停的，不是坏了"。

**门禁新增**：`QuotaLedger` 的累加与"到预算即停"有一个单测（假服务器 + 假限额），
断言**不会**在预算耗尽后继续发请求（防止某条路径绕开账本）。

## 6. 失败模式与降级

| 触发 | 用户看到 | 数据 | 处理 |
|---|---|---|---|
| 命令超时（60 s） | 无感（重试一次） | 无 | 换连接重试（`pool_op!` 既有语义）→ 仍失败则任务退避 |
| 连接被服务器关闭 | 无感 | 无 | 连接池丢弃坏连接；任务重试 |
| 服务器限流（`NO [LIMIT]`/`NO [UNAVAILABLE]`/`BYE too many`） | 状态栏"服务器繁忙，1 分钟后重试" | 无 | 指数退避（1m→5m→15m→1h），并把该账号并发降到 1 |
| 磁盘满 | 横幅 + 暂停同步/索引 | 无（事务回滚） | 空间恢复后自动续跑 |
| 用户中途改窗口（全部→2 年） | 提示"将删除本地 X 封正文"→确认 | 本地删（服务器不动） | 取消正在跑的 WindowBackfill，重算 StopCondition |
| 用户删账号 | 提示"将删除本地 N 封缓存（服务器不受影响）" | 本地删 | 级联删 + 停任务 |
| 超大文件夹（> 50 万封） | 提示"该文件夹 50 万封，建议用'最近 1 年'；全部下载约 X GB" | 无 | 闸值：`Count` 上限保护 + 明确告知 |
| 服务器不支持 UIDPLUS | 无感 | 无 | 删除走 `EXPUNGE`（既有能力探测） |
| 服务器不支持 IDLE | 延迟变大（设置页显示） | 无 | L3 轮询 |
| 正文取回失败（单封） | 读信面板显示"正文读取失败，重试" | 该封无正文 | 记 `index_queue.failed`，不阻塞其它 |

## 7. 迁移与兼容（从 v0.2 演进）

| 现有资产 | 处理 |
|---|---|
| `SyncDriver`（`sync.rs`） | **保留为 Incremental 的实现核心**，外面套任务框架（JobCtx/进度/取消）。driver 的"两个同步阶段 + 一个异步取件"形状不变；**但截断方向必须改**（现在 `split_off` 保留的是最新一批 ⇒ 一次 >500 封会留下永久空洞，见 §5.3 与 M0-5）。 |
| `idle.rs`（有界窗口 + 退避） | 演化成 `watch::watch_account` + `probe_cycle`，增加多文件夹 STATUS 与抖动 |
| `ConnectionPool` / `pool_op!` | 保留。新增"按用途取连接"的封装：`pool.job_client(purpose)`，避免 IDLE 与同步抢同一条 |
| 固定 500 窗口 + 回填 | `SyncWindow::OnDemand` 即旧行为；默认改为 `Years(2)`（P9 决策） |
| `folders.history_complete` | 语义保持（"到底了"），窗口模式下由 `StopCondition` 决定是否继续用 |
| 托盘未读/通知 | 由 `q::folders::counts` 与 `mail:new` 驱动（13） |

## 8. 性能预算

| 指标 | 预算 | 说明 |
|---|---|---|
| 首屏可用（阶段 A 完成） | ≤ 5 s（宽带）/ ≤ 15 s（4G） | 200 封头部 |
| 头部吞吐 | ≥ 300 封/秒 | 批量 1000/事务；实测受 RTT 限制 |
| 正文预取 | ≥ 20 封/秒（默认限速），不限速时 ≥ 60 封/秒 | 解析在写线程之外 |
| 单任务内存 | ≤ 20 MB（一批 1000 头部） | 不把整页 raw 留在内存 |
| 连接占用 | 每账号 ≤ max_connections；IDLE 固定占 1 | 全局上限 4 作业并发 |
| 空闲 CPU | ≤ 0.5%（3 账号） | IDLE 期间不轮询 |
| 10 万封全量（All 窗口） | 头部 ≤ 8 分钟；正文 ≤ 3 小时（可暂停） | 显示 ETA |
| 索引吞吐 | ≥ 20 封/秒 | 09 |

## 9. 测试计划

| 层 | 用例 |
|---|---|
| 假服务器（`loopback.rs` 扩展） | ①区间取件边界（空区间、单封、跨页）②**截断保留最旧一批（对着代码断言：夹具一次给 >limit 封，断言库里是"最旧的那批"且游标停在它上界）** ③UIDVALIDITY 中途变化 ④限流响应 → 退避参数 ⑤IDLE 窗口内 EXISTS（L1）⑥STATUS 变化检测 ⑦取消发生在批次中间（断言无半写）⑧服务器不回 STATUS → 降级 L3 ⑨**一次 EXISTS 推送只触发一次同步**（L1 与心跳不得双发）⑩**UIDNEXT 变小且没有新邮件（重编号）⇒ 必须 invalidate + 重建** |
| 能力矩阵夹具 | 每个能力组合一个服务器变体（无 UIDPLUS/无 IDLE/无 MOVE/无 UTF8），断言走对应降级路径 |
| 真机（`provider_reachability` 扩展为 `provider_capabilities`） | 对 21 家内置服务商**实测**：IDLE、STATUS-while-idle、SEARCH UTF-8、MOVE、APPEND、SMTP 的 8BITMIME/SMTPUTF8/SIZE、并发上限。**不要探测 THREAD/ESEARCH/NAMESPACE/LIST-STATUS**（D7：那些响应会关连接）。产出 `design/appendix-provider-matrix.md`（自动生成） |
| 混沌测试 | 随机断开连接 / 随机 500 / 随机慢响应；断言：本地库最终与服务器一致、无空洞、无重复 |
| 离线队列 | 断网做 200 次混合操作 → 联网 → 断言服务器最终一致（03 §9 同款） |
| 性能 | §8 各项基准，纳入 `tools/bench.ps1` |
| 长稳测试 | 连续 24 h 收信 + 索引 + 多次断网，断言无内存泄漏（RSS 曲线）与无任务泄漏（`job_list` 为空） |

## 10. 门禁新增项

1. **游标不变式**：`sync/*.rs` 中除 `incremental_sync`/`initial_sync` 外不得出现 `set_cursor(`（源码正则；测试区豁免）。
2. **事务边界**：`commit_page` 必须在一个 `db.write` 里完成（源码检查：函数体内只出现一次 `db.write`，且其中包含 `index_queue`）。
3. **连接预算**：`NetBudget` 的默认值表与 `providers.rs` 的 `max_connections` 一致性检查（数值比较）。
4. **取消点**：每个长任务函数必须含 `job.check_cancelled()?`（源码检查：函数体内出现次数 ≥ 批次数）——便宜且能防"加了循环忘了取消"。
5. **能力降级可测（评审后按删减重写）**：`cargo test -p mail-core sync_capabilities` 覆盖**每一个仍存在的能力位**为 false 时的用例
   （`id`/`idle`/`uidplus`/`move`/`condstore`/`sort`/`utf8_accept`/`special_use`/`compress`/`search_utf8`/`status_ok`/`smtp_*`）。
   **注意**：`thread_references`/`list_status`/`namespace`/`qresync` 已从能力表**删除**（它们对应的命令我们永不发送，
   探测或测试等于主动制造断连 —— A1-F42 / D7），所以不要再为它们写 `without_*` 用例（测试名约定 `without_*`）。

## 11. 逆向反驳（红队）

**反驳 1：默认窗口改成"最近 2 年"会让首日同步变得又慢又吓人 —— v0.2 之所以只取 500 封就是怕这个。**
- 证据：两次不同目标（"首屏可用" vs "离线可搜"）被混在一句话里。
- 改设计：**两阶段**（§5.2）：阶段 A 只取 200 封让首屏 5 秒可用；阶段 B 才按窗口回溯，
  并且**默认限速 + 可暂停 + 明确 ETA**。用户感知从"卡住"变成"后台在补，我能先干活"。
- 让步：`Count(1000)` 可能才是大众默认值，`Years(2)` 更适合重度用户 —— 这一条留给 §12 实测/拍板。

**反驳 2：一条连接监听多个文件夹（STATUS）在有的服务器上不可靠/昂贵。**
- 证据：RFC 3501 明确不建议对**已选中**的邮箱用 STATUS；对其它邮箱是允许的，
  但部分服务器（尤其国内）对 STATUS 支持参差。
- 缓解：`probe_capabilities` 里加"STATUS 可用性"探测（试一次，失败则永久降级）；
  刷新间隔对多文件夹用 3-5 倍于主窗口（把成本摊开）；实在不行退回"每文件夹一条 IDLE 连接"，
  由 `max_connections` 决定能同时看几个（并在设置页说明"最多同时监听 N 个文件夹"）。

**反驳 3：`commit_page` 把"头部 + 游标 + 索引入队 + 会话计数"放一个事务，事务会变长、阻塞写。**
- 证据：SQLite 单写者，长事务会让 UI 的旗标写入排队。
- 缓解：批量 500-1000 行、只做必要的 UPDATE；会话计数改成**增量**（按 thread_id 累加，
  不做全表重算）；实测单事务 < 50 ms 为目标，纳入基准。若超标，退路是把"会话计数"改为异步补齐
  （可接受短暂不准，夜检修正）。

**反驳 4：能力探测缓存 30 天，服务器升级/降级会导致行为错配。**
- 缓解：**失败驱动失效**：任何因"能力假设"导致的命令失败（例如发了 MOVE 得到 BAD），
  立刻重探并修正缓存；探测结果在诊断包里可见。这比"每次连接都探测"省得多，且能自纠。

**反驳 5：限速/退避会让"我想立刻收到新邮件"的体验变差。**
- 缓解：优先级机制 —— 用户动作与"新邮件"永远插队；限速只作用于**预取与回溯**等后台工作。
  即"重要的事快，不重要的事慢"，而不是一刀切变慢。

**反驳 6：多账号 × 多文件夹 × 预取，会不会把国内服务器惹到封号？**
- 这是**本项目最大的现实风险之一**（HANDOFF 里 QQ/163 的 `max_connections` 就是证据）。
- 缓解四件事：①硬连接预算（每账号上限来自 providers 表，不接受用户调大超过表值）②全局并发 4
  ③请求令牌桶 + 抖动 ④首次加账号时先跑"轻量探测"确认服务器脾气（并发 1、只取 200 封），
  再逐步升到默认并发。验收里加一条：**真机 21 家各跑 30 分钟不触发限流**（04 §9）。

**反驳 8：配额感知会变成"同步永远做不完"，用户等一个月还没下完 50 GB。**
- 证据（A1-F1）：Gmail 2.5 GB/天，50 GB 就是 20 天以上；这不是设计问题，是配额。
- 缓解：①**首屏优先**（阶段 A 200 封）让人立刻能用；②**重要优先**——按"最近 + 未读 + 有附件"
  排序预取，让用户在配额内拿到最可能需要的邮件；③**诚实告知**（§5.7 的文案）+
  给"只要 90 天"的快捷选项；④本地导入（06）作为"我有旧邮件存档"的补充路径。
- 让步：**不再向用户承诺"全部同步完成"这种话**；UI 用"已缓存 X 封（共 Y 封，后台继续）"。

- 缓解：任务启动时把 stop 条件写进 `sync_jobs.cursor_json`（快照），中途改设置只影响**新任务**；
  用户改窗口时明确弹"当前任务按旧设置跑到 X 或立即按新设置重算？"（默认重算并取消旧任务）。

## 12. 未决问题

| # | 问题 | 影响 | 建议 |
|---|---|---|---|
| Y1 | 默认窗口到底选 `Years(2)` 还是 `Count(1000)` | 首日体验与磁盘 | 二者都实现；默认 `Years(2)`，设置页用"我全都要/最近两年/最近一千封/仅按需"人话呈现（P9） |
| Y2 | IDLE 窗口默认值 | 心跳频率 vs 请求量 | **120 s**（及时性已由 L1 的 NewData 承担，见 §5.4）；设置里可 30-300 s |
| Y3 | 是否在 M1 就做 CONDSTORE（省流量） | 复杂度 | M1 只记录 modseq；**M3 走 CONDSTORE 的 `CHANGEDSINCE`，不发 `ENABLE QRESYNC`**（A1-F43：QRESYNC 没有 API，CONDSTORE 有且够用） |
| Y4 | 多文件夹监听是否 M1 就开 | 服务器风险 | M1 默认只监听 INBOX + 用户可加（上限由 max_connections 决定），M2 放开 |
| Y5 | 正文预取的默认范围（窗口内全部 vs 仅最近 1 年） | 磁盘/带宽 | 默认"窗口内但 ≤ 1 年"，可调（与 03 的 raw 策略联动） |
| Y6 | 附件预取是否默认关 | 磁盘 | 默认关（SYN-05/S5） |

## 13. 验收清单

- [ ] 三种窗口（All/Years/Count）+ OnDemand 都能跑到正确的停止条件，且**中途取消不留半写**。
- [ ] 10 万封夹具：阶段 A ≤ 5 s；All 窗口头部 ≤ 8 分钟；全程 UI 可读可搜（读写不互斥）。
- [ ] 断网 → 操作 → 恢复：服务器最终一致，无重复发送、无幽灵邮件（03 §9 同款断言）。
- [ ] 混沌测试（随机断连/500/慢响应）跑 1 小时，本地与服务器一致，任务队列不泄漏。
- [ ] 21 家真机能力矩阵生成，且每家的降级路径可解释（附录自动产出）。
- [ ] `check.ps1` 五项新增检查通过。
