# 18 协议扩展与后端抽象（JMAP / Graph / Gmail API / POP3 / CalDAV）

> **目标**：把"还能接哪些协议、分别值不值得做"一次说清，并给出**后端抽象**，让后续接入不必重写同步引擎。
> 同时明确**不做**的协议及其理由，避免以后有人再花时间试一遍。
>
> 覆盖 ID：PROTO-01…09、CAL-01…05、CONTACT-07/08。
> 事实依据：A1 §三/§四（JMAP 生态、Gmail API 的 CASA 成本、Graph SDK 风险与推送限制、EWS 死期、POP3 语义、CalDAV crate 现状、iCalendar 陷阱）。

---

## 1. 目标与非目标

**目标**
1. **可插拔后端**：IMAP / POP3 / JMAP / Graph 都实现同一个 `MailBackend` trait，同步引擎与 UI 不感知差异（能力位决定 UI 降级）。
2. 给出每个协议的**已验证事实 + 工作量 + 依赖 + 里程碑**，一次决策到位。
3. 明确 **不做** 的协议与理由（EWS、ActiveSync、Sieve、NNTP 等）。
4. 日历**分两步**：先"邀请只读 + RSVP + 议程"，后"完整日历网格 + 双向同步"。

**非目标**
- 不自建推送中转（红线 6）：Gmail/Graph 的推送需要公网 HTTPS 端点 ⇒ 我们**用轮询**，不做服务端。
- 不做 CalDAV **服务端**（只做客户端）。
- 不承诺"所有协议一开始就支持全部能力"（POP3 天生没有旗标/文件夹）。

## 2. 用户可感知行为

| 场景 | 用户看到 |
|---|---|
| 加一个 Fastmail 账号 | （M3）识别为 JMAP 可用 → 优先用 JMAP（更快、原生会话串）；不可用则退回 IMAP |
| 公司租户禁用了 IMAP | 明确提示"你的组织禁用了 IMAP。可以请 IT 打开，或用自己的应用 ID 走 Graph（M3）" |
| 加一个只支持 POP3 的老邮箱 | 提示"POP3 不支持文件夹、旗标与已读状态同步，邮件只保存在这台电脑上"；相关按钮隐藏而不是假装可用 |
| 收到会议邀请 | 邮件顶部显示会议卡片（标题/时间/地点/组织者），"接受/暂定/拒绝"一键 RSVP；正文里原始的 .ics 仍可下载 |
| 查看"议程" | （M3）侧栏"日历 → 议程"：未来 7 天的会议列表；**不做**日/周网格（后置） |
| 联系人同步 | （M3）CardDAV 账号只读拉取 + 本地编辑回写（有冲突提示） |

## 3. 后端抽象（本次设计最重要的结构性决定）

```rust
/// 一个账号的邮件后端。实现：ImapBackend（v1）、Pop3Backend（v1）、
/// JmapBackend（M3）、GraphBackend（M3）。
#[async_trait::async_trait]
pub trait MailBackend: Send {
    fn capabilities(&self) -> BackendCapabilities;
    fn account_id(&self) -> &str;

    async fn list_folders(&mut self) -> Result<Vec<MailboxInfo>>;
    async fn select(&mut self, folder: &str) -> Result<FolderStatus>;

    /// 取头部：按**后端自己的标识区间**取（IMAP 是 UID 区间，JMAP/Graph 是 opaque 游标）
    async fn fetch_headers(&mut self, folder: &str, range: MsgRange, keys: HeaderKeys) -> Result<Vec<MessageSummary>>;
    /// 取整封（正文 + 附件字节，07 的 spool 消费它）
    async fn fetch_raw(&mut self, folder: &str, id: &MessageId) -> Result<Vec<u8>>;
    /// 取单个 part（附件按需下载；不支持的实现返回 Unsupported 让调用方降级为整封）
    async fn fetch_part(&mut self, folder: &str, id: &MessageId, part: u32) -> Result<Option<Vec<u8>>>;

    async fn store_flags(&mut self, folder: &str, ids: &[MessageId], change: FlagChange) -> Result<u32>;
    async fn move_messages(&mut self, folder: &str, ids: &[MessageId], dest: &str) -> Result<u32>;
    async fn copy_messages(&mut self, folder: &str, ids: &[MessageId], dest: &str) -> Result<u32>;
    /// 返回后端给出的新标识（IMAP 拿不到就返回 `MessageId::Unknown`，见 A1-F43）
    async fn append(&mut self, folder: &str, raw: &[u8], flags: &[Flag], internal_date: Option<i64>) -> Result<MessageId>;
    async fn expunge(&mut self, folder: &str, ids: &[MessageId]) -> Result<u32>;

    /// 服务器端搜索（09 的回退路径）
    async fn search(&mut self, folder: &str, q: &SearchQuery) -> Result<Vec<MessageId>>;
    /// 一次"变化探测"（04 §5.4）：IMAP=IDLE/STATUS、JMAP=push/state 轮询、Graph=delta 轮询、POP3=STAT/LIST
    async fn poll_changes(&mut self, folders: &[String], window: Duration) -> Result<Vec<FolderChange>>;
    /// **不要写成 `logout(self: Box<Self>) where Self: Sized`** —— 那会把方法从 vtable 里摘掉，
    /// `Box<dyn MailBackend>` 上根本调不到（第二轮评审）。用普通方法 + `&mut self`。
    async fn logout(&mut self) -> Result<()>;
}

/// **标识必须是不透明的（第二轮评审的架构修正）**：
/// 原 trait 全用 `u32` UID，而同一份文档里的 JMAP/Graph 客户端用的是 `String` id，
/// `FolderStatus` 又返回 uidvalidity/uidnext（IMAP 专有），POP3 还得把 UIDL 哈希成 u32 才能塞进去 ——
/// 这正是 D12 要避免的"同步引擎 ↔ IMAP 耦合"，到 M3 会变成一次 trait 破坏式改动。
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum MessageId { Uid(u32), Opaque(String), Unknown }
/// 同步锚点：IMAP 用 (uidvalidity, uidnext, highestmodseq)，JMAP/Graph 用不透明 state 串
#[derive(Clone, Debug)]
pub enum SyncAnchor {
    Imap { uidvalidity: u32, uidnext: u32, highest_modseq: Option<u64> },
    Opaque { state: String },       // JMAP state / Graph deltaLink
    Pop3 { uidl_seen: u64 },        // POP3 没有游标，只能记"见过多少个 UIDL"
}
/// 区间也按后端表达：IMAP 是闭区间，JMAP/Graph 用锚点
#[derive(Clone, Debug)]
pub enum MsgRange { Between(MessageId, MessageId), NewerThan(SyncAnchor), Newest(usize) }

#[derive(Clone, Copy, Debug, Default)]
pub struct BackendCapabilities {
    pub folders: bool, pub flags: bool, pub move_: bool, pub copy: bool,
    pub append: bool, pub server_search: bool, pub server_threads: bool,
    pub labels: bool, pub unread_count: bool, pub quota: bool,
    pub partial_fetch: bool, pub push: PushKind, pub max_append_bytes: Option<u64>,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PushKind { None, PollOnly, Idle, Native }
```

**能力位如何影响 UI（这是抽象的收益，也是约束）**
| 能力 | 缺失时的 UI 降级 |
|---|---|
| `folders=false`（POP3） | 隐藏"移动到文件夹/新建文件夹"；收件箱就是全部；显示"仅本地"徽标 |
| `flags=false`（POP3） | 隐藏旗标/已读同步（本地仍可标记，但注明"不会同步到服务器"） |
| `labels=false` | 隐藏标签功能（Gmail 走 IMAP 时标签映射为关键词，能力位要看实际探测） |
| `partial_fetch=false` | 附件下载走"整封取回"（既有 v0.2 行为），UI 提示"大附件会占用较多内存" |
| `push=Idle` | 显示通知延迟（04 §5.4 的 L1/L2/L3 说明） |
| `server_search=false` | 隐藏"在服务器上继续搜索"入口（09） |
| `max_append_bytes` | 发送/上传前预检（07 §3；A1-F3） |

> **实现纪律**：`sync/` 与 `src-tauri` 只允许通过该 trait 访问邮件服务器；
> 直接 `use crate::net::imap::ImapClient` 的地方只允许在 `net/imap.rs` 与它的单测里（门禁可查）。

## 4. 各协议事实与决策（一次说清）

| 协议 | 已验证事实（A1） | 我们要不要做 | 工作量 | 里程碑 |
|---|---|---|---|---|
| **IMAP + SMTP** | 2026 仍是主力；EWS 死但 IMAP 明确不受影响；各服务商能力差异大（IDLE/UIDPLUS/MOVE/APPEND/SEARCH UTF-8） | **主力** | 已有 | M1（加固） |
| **POP3** | `UIDL` 是唯一稳定标识；`DELE` 只在 `QUIT` 生效；`TOP n k` 可廉价预览；无文件夹/旗标/服务器已读/搜索；`RETR` 有点填充；`STLS` 必须；crate 都是 0.1.x ⇒ **自己写 ~400 行** | **做**（`accounts.kind='pop3'`，本地账号） | S（4-7 天） | M1 末 / M2 初 |
| **JMAP** | `jmap-client` 0.4.x（Stalwart，小众但活跃）；Fastmail/Apache James/Stalwart/Cyrus 支持；**Gmail 与微软都不支持**；不需要 OAuth broker | 做，但**后置** | M（4-8 周） | M3 |
| **Gmail API** | 需要受限 scope ⇒ **每年 CASA**（第三方报价 $675-5400/年）；`historyId` 会过期；**push 需要公网 Pub/Sub** | **v1 不做**（IMAP+OAuth2 已够）；若做了，收益是"标签/会话原生 + 更省流量" | M + 合规 | 视 P2 决策 |
| **Microsoft Graph** | IMAP+OAuth2 仍可用 ⇒ **不是必需**；但①有些租户禁用 IMAP ②**M365 没有 CalDAV/CardDAV，日历/联系人只能走 Graph** ③delta 同步更省；SDK（`graph-rs-sdk` 3.0.x）**docs.rs 构建失败、8 个月无文档** ⇒ 用 `reqwest` 手写 ~8 个端点；**变更通知需要公网端点** ⇒ 轮询 | 做，但**后置**，且先接日历/联系人 | L（Thunderbird 用了约 4 个月） | M3 |
| **EWS** | 微软：**2026-10 起全局开始禁用、2027-04 完全禁用**（A1-F27） | **绝不做**（云上必死；本地部署用 IMAP+Autodiscover 即可） | — | 永不 |
| **CalDAV / CardDAV** | `libdav` 0.11（ISC，NGI 资助）可用；`fast-dav-rs` 更强但 LGPL+单人+8 star ⇒ 不用；**principal 发现各家不同**；iCloud 需应用专用密码 | 做（CardDAV 先只读，CalDAV 后置） | M + M（同步与冲突） | M3 |
| **iCalendar / 邀请** | `calcard` 0.3.14（Apache/MIT，含 RRULE 展开）；`icalendar` crate **别用来展开 RRULE**；SEQUENCE 大小是正确性要求；拒绝/取消要按 SEQUENCE 判 | 做（**邀请只读 + RSVP + 议程**） | S-M（解析展示）；+M（RSVP 让 Google/Exchange 满意） | M3 |
| **日历网格（日/周/月）** | 重叠事件布局、全天行、拖拽、时区正确 ⇒ **M（2-3 个月）**，是前端布局引擎工程 | **后置**（v1.5+），不阻塞"能回邀请"这个 80% 价值 | M | M3+ |
| **LDAP / AD 目录** | 企业自动补全的唯一现实来源（也是 S/MIME 证书的来源） | M3 评估 | M | M3 |
| **ActiveSync (EAS)** | 许可受限、生态衰退、实现成本高 | **不做** | — | 永不 |
| **Sieve / ManageSieve** | 服务端过滤，协议支持参差 | **不做**（本地规则引擎 10 覆盖大多数需求；01 §6 已否决服务端 Sieve UI） | — | 永不 |
| **NNTP / 其它** | 与产品无关 | 不做 | — | — |

## 5. 接口与实现要点（函数级）

### 5.1 POP3 后端（自己做，不用 0.1.x crate）
```rust
pub struct Pop3Client { /* TcpStream + TLS + 缓冲 */ }
impl Pop3Client {
    pub async fn connect(ep: &Endpoint, cred: &Credential) -> Result<Self>;     // STLS 必须；USER/PASS 或 XOAUTH2
    pub async fn stat(&mut self) -> Result<(u32 /*count*/, u64 /*bytes*/)>;
    pub async fn list(&mut self) -> Result<Vec<(u32 /*msg#*/, u64 /*size*/)>>;
    pub async fn uidl(&mut self) -> Result<Vec<(u32, String)>>;                 // **唯一稳定标识** → 去重键
    pub async fn top(&mut self, msg: u32, lines: u32) -> Result<Vec<u8>>;       // 廉价预览（列表 + 摘要）
    pub async fn retr(&mut self, msg: u32) -> Result<Vec<u8>>;                  // 点填充反转义 + CRLF 归一
    pub async fn dele(&mut self, msg: u32) -> Result<()>;                       // 只是标记
    pub async fn quit(self) -> Result<()>;                                      // 到这里才真正删除
    pub async fn noop(&mut self) -> Result<()>;
}
/// 保留策略（用户可见设置，默认"在服务器保留副本"）
#[derive(Serialize, Deserialize, Clone, Copy, Debug)]
pub enum PopRetention { KeepOnServer, DeleteAfterDays(u32), DeleteImmediately }
```
**语义映射**：POP3 账号 = `accounts.kind='pop3'` + 一个收件箱；`MessageSummary.uid` 用 `UIDL` 的哈希稳定化
（不能直接用序号，序号跨会话不稳定）；**不写 `pending_ops` 的服务器动作**（POP3 没有），
本地标记只在本地生效并在 UI 注明。

### 5.2 JMAP 后端
```rust
pub struct JmapClient { /* session, api_url, account_id, state strings */ }
impl JmapClient {
    pub async fn connect(ep: &Endpoint, cred: &Credential) -> Result<Self>;
    pub async fn session(&mut self) -> Result<JmapSession>;              // capabilities + accounts
    pub async fn mailboxes(&mut self) -> Result<Vec<JmapMailbox>>;
    /// 增量：Email/queryChanges + Email/get（state 字符串）
    pub async fn changes(&mut self, since_state: &str) -> Result<JmapChanges>;
    pub async fn email_get(&mut self, ids: &[String], props: &[&str]) -> Result<Vec<JmapEmail>>;
    pub async fn email_set(&mut self, patch: JmapEmailPatch) -> Result<()>;   // 旗标/移到邮箱
    pub async fn email_submit(&mut self, draft: JmapDraft) -> Result<String>; // 发信（Submission）
    pub async fn blob_upload(&mut self, bytes: &[u8], mime: &str) -> Result<String>;
}
```
**要点**：JMAP 有**原生会话串**（`Email/thread`）⇒ 会话视图直接用服务器结果（比客户端线程串更准）；
`state` 字符串是增量同步的锚点，过期就全量重同步（与 Graph delta 同一套回退逻辑）。

### 5.3 Microsoft Graph 后端（只做必要端点）
```rust
pub struct GraphClient { /* reqwest + token provider（复用 05） */ }
impl GraphClient {
    pub async fn me(&mut self) -> Result<GraphUser>;
    pub async fn mail_folders(&mut self) -> Result<Vec<GraphFolder>>;
    /// 增量：初始全量 → @odata.nextLink 翻页 → 保存 @odata.deltaLink；410/syncStateNotFound → 全量重来
    pub async fn messages_delta(&mut self, folder_id: &str, delta: Option<&str>) -> Result<GraphDelta>;
    pub async fn message_raw(&mut self, id: &str) -> Result<Vec<u8>>;        // $value 取 MIME
    pub async fn move_message(&mut self, id: &str, dest_folder: &str) -> Result<()>;
    pub async fn send_mail(&mut self, mime: &[u8]) -> Result<()>;            // POST /me/sendMail（或 /me/messages + send）
    pub async fn events_delta(&mut self, calendar: &str, delta: Option<&str>) -> Result<GraphDelta>;   // 日历（M3）
}
/// 轮询策略（没有服务端就轮询）
pub struct GraphPollPolicy { pub interval: Duration /* 默认 2 min */, pub backoff_on_429: Duration /* 默认 5 min */ }
```
**要点**：①scope 需要在 05 的基础上加 `Mail.ReadWrite`/`Mail.Send`（或 `Mail.ReadBasic` 做只读）；
②`deltaLink` 会过期（410）⇒ **必须**有全量重同步回退（与 JMAP 共用一套"状态锚点失效"处理）；
③推送要公网端点 ⇒ **不订阅，轮询**；④有些租户禁 IMAP 但允许 Graph ⇒ 这类账号是我们的加分项。

### 5.4 会议邀请与日历
```rust
pub struct CalendarEvent { pub uid: String, pub sequence: u32, pub method: IcalMethod, pub summary: String,
                           pub start: EventTime, pub end: EventTime, pub all_day: bool, pub location: Option<String>,
                           pub organizer: Option<Attendee>, pub attendees: Vec<Attendee>, pub recurrence: Option<RecurrenceInfo>,
                           /// **第二轮评审补**：A1-F21 的正确性清单里有 EXDATE/RDATE/THISANDFUTURE，
                           /// 但原结构体一个都没有 —— 漏掉它们会出现"已取消/已改期的实例仍显示为有效"，
                           /// 正是 §7 禁止的那种失败。
                           pub recurrence_id: Option<EventTime>,   // 覆盖单个实例
                           pub exdates: Vec<EventTime>,            // 被排除的实例
                           pub rdates: Vec<EventTime>,             // 额外增加的实例
                           pub this_and_future: bool,              // 本次修改影响其后所有实例
                           pub raw_ics: String }
pub enum IcalMethod { Request, Reply, Cancel, Counter, Publish, Unknown }
/// 从 MIME 里挖邀请：注意它常在 multipart/alternative 里出现两次 ⇒ 按 (UID, SEQUENCE, DTSTAMP) 去重
pub fn extract_invites(raw: &[u8]) -> Result<Vec<CalendarEvent>>;
/// 冲突与覆盖规则：SEQUENCE 小的忽略；CANCEL 取消 ≤ 它的实例；RECURRENCE-ID 覆盖单个实例
pub fn merge_invite(store: &mut InviteStore, ev: &CalendarEvent) -> MergeOutcome;
/// RSVP：生成 METHOD:REPLY，发回 ORGANIZER（Google 有时省略 mailto:，要兜住）
pub fn build_rsvp(ev: &CalendarEvent, partstat: PartStat, me: &str) -> Result<Vec<u8>>;
pub async fn send_rsvp(ctx: &AppCtx, ev: &CalendarEvent, partstat: PartStat) -> Result<()>;
/// 展开：**绝不物化无限 RRULE**，按可见窗口惰性展开；DTSTART;TZID 锚定墙上时间（跨 DST 不漂移）
pub fn expand_occurrences(ev: &CalendarEvent, window: (i64, i64), tz: &chrono_tz::Tz) -> Result<Vec<Occurrence>>;
```
**明确不做**：日/周/月网格、拖拽新建、CalDAV 双向日历同步 —— 都排 M3+（A1-F23 的工作量证据）。

### 5.5 CardDAV（联系人）
```rust
pub struct CardDavAccount { pub id: String, pub url: String, pub username: String, pub sync_token: Option<String> }
impl CardDavAccount {
    pub async fn discover(&mut self) -> Result<Vec<AddressBook>>;      // principal 发现（各家不同，A1-F19）
    pub async fn sync(&mut self, since: Option<&str>) -> Result<CardDavDelta>;   // RFC 6578 WebDAV-Sync（libdav）
    pub async fn put(&mut self, href: &str, vcard: &[u8], etag: Option<&str>) -> Result<String>;
    pub async fn delete(&mut self, href: &str, etag: Option<&str>) -> Result<()>;
}
```
**冲突策略**：以 `ETag` 为准（If-Match）；冲突时**不覆盖**，留两侧并提示（联系人比邮件更容易丢）。

## 6. 状态机与降级

### 6.1 后端选择（加账号时）
```
provider/auth（05）→ 选后端：
  1. provider 声明 jmap 且探测成功 → JmapBackend        （M3）
  2. provider 声明 graph 且 IMAP 不可用 → GraphBackend   （M3，先在设置里显式选择）
  3. provider 只支持 POP3 → Pop3Backend                  （M1 末）
  4. 否则 → ImapBackend                                  （M1 默认）
探测失败不静默降级：明确告诉用户"这个账号不支持 X，将使用 Y，代价是 Z"
```

### 6.2 状态锚点失效（JMAP `state` / Graph `deltaLink` 都会过期）
```
增量同步返回 410/syncStateNotFound/invalid-state：
  ├ 标记该文件夹"需要全量重同步"
  ├ 保留本地数据（不删），只重新拉取增量基准
  ├ 重新计算一次"服务器有而本地没有 / 本地有而服务器没有"的差集
  └ 若差集过大（> 20% 封数）→ 走 03 的 invalidate + 重建（并提示用户）
```

### 6.3 能力缺失时的 UI 降级
```
capabilities().folders == false  → 隐藏文件夹树（POP3 账号显示"收件箱（仅本地）"）
capabilities().flags   == false  → 旗标/已读按钮加 tooltip"不会同步到服务器"
capabilities().labels  == false  → 隐藏标签入口
capabilities().move_   == false  → 隐藏"移动到…"与拖放
capabilities().partial_fetch == false → 附件下载提示"将取回整封邮件"
```

## 7. 失败模式

| 触发 | 用户看到 | 处理 |
|---|---|---|
| 租户禁用 IMAP（M365） | "你的组织禁用了 IMAP。可以请 IT 打开；或用自己的应用 ID 走 Graph（M3）" | 05 §6 + 18 §4 的路线图 |
| Graph deltaLink 过期 | 无感（后台全量重同步），若差集大则提示"正在重新同步这个文件夹" | §6.2 |
| Graph/JMAP 限流（429） | 无感（退避） | 复用 04 的退避与配额账本 |
| JMAP 服务器不支持某能力（如 `Email/queryChanges`） | 降级为全量查询 + 本地 diff | 能力探测 |
| POP3 服务器不支持 `UIDL` | **明确拒绝**该账号并建议用 IMAP（没有稳定标识就无法保证不重复） | 探测后拒绝 |
| POP3 用户既想保留服务器副本又想多端同步 | 不适用（POP3 无多端语义） | UI 明说；建议改 IMAP |
| .ics 里时区缺失/错误 | 会议卡片显示"时间按本机时区解释" | A1-F21 的显式选择 + 提示 |
| 同一封邀请收到两次（alternative 里重复） | 无感 | (UID, SEQUENCE, DTSTAMP) 去重 |
| 已取消的会议 | 卡片显示"已取消"（不能让用户以为是有效会议） | SEQUENCE 规则 |
| CardDAV ETag 冲突 | "服务器上的这张名片被别处改过，保留哪一个？" | 不覆盖策略 |

## 8. 性能预算

| 指标 | 预算 |
|---|---|
| Graph 增量轮询（1 分钟间隔，无变化） | ≤ 1 请求/分钟/账号，响应 ≤ 30 KB |
| JMAP 增量（无变化） | ≤ 1 请求/5 分钟（state 比对） |
| POP3 收信（100 封，仅头部） | ≤ 20 s（`TOP` 批量） |
| 邀请解析（单封） | ≤ 20 ms |
| 议程视图（未来 30 天，100 事件） | ≤ 100 ms |
| RRULE 展开（窗口内，100 次重复） | ≤ 10 ms |
| 后端抽象开销 | 每封 ≤ 5 µs（trait 动态分派可忽略） |

## 9. 测试计划

| 层 | 用例 |
|---|---|
| 单元 | POP3 点填充/CRLF/UIDL 稳定化；邀请去重与 SEQUENCE 规则（REQUEST/REPLY/CANCEL/COUNTER 各一条 + "低 SEQUENCE 忽略"）；RRULE 展开跨 DST（"每个工作日 09:00" 在切换日仍 09:00）；`RECURRENCE-ID` 覆盖单个实例；全天事件 DTEND 排他 |
| 假服务器 | 扩展 loopback：POP3 假服务器（含 STLS、`TOP`、`UIDL`、`QUIT` 才删除）；Graph/JMAP 用本地 HTTP 假服务（deltaLink 过期 → 410 → 全量） |
| 能力降级 | 每个 `BackendCapabilities` 字段为 false 时的 UI/引擎行为（测试名约定 `without_*`） |
| 真机 | 至少：IMAP（QQ/163/Gmail/Outlook）、POP3（任一支持的服务商）、JMAP（Fastmail 若有账号）、Graph（若企业租户可得） |
| 日历 | 用真实邀请样本（Google/Outlook/Fastmail 各 3 封）验证解析与 RSVP 往返 |

## 10. 门禁新增项

1. **协议访问收口**：`sync/` 与 `src-tauri/` 中不得出现 `use crate::net::imap::`（只允许 `MailBackend`）。
2. **不做 EWS**：源码/文档中不得出现 `ews` / `exchange.asmx` 的实现代码（正则）。
3. **能力位驱动 UI**：前端不得出现"账号 kind == pop3 就隐藏按钮"这类硬编码（必须查 capabilities）。
4. **RRULE 不物化**：日历代码不得出现按"无界循环展开无限重复"的写法（单测断言窗口外不展开）。
5. **邀请 SEQUENCE**：单测覆盖"低 SEQUENCE 的 REQUEST 不覆盖高 SEQUENCE 的已存事件"。

## 11. 逆向反驳（红队）

**反驳 1：不做 Graph，某些企业用户直接不能用（租户禁 IMAP），这是产品缺口。**
- 证据：确实存在（Thunderbird 加 Graph 就是为了这批人）。
- 缓解：①**清晰的错误指引**（告诉用户找 IT 开 IMAP，或用自己的应用 ID）；②BYO 租户 ID（05 §11 反驳 1）
  让企业可以自己控制应用注册；③Graph 排在 **M3 首位**（因为它同时解锁 M365 日历/联系人）；
④**不承诺 v1 支持**，并写进 README 的已知限制。
- 让步：如果实测发现禁用 IMAP 的租户比例很高（需要真实用户反馈），把 Graph 提前到 M2 末。

**反驳 2：JMAP 只服务几个百分点，值得为它设计 trait 吗？**
- 证据：trait 的成本主要在"把同步引擎与 IMAP 细节解耦"，这**本来就该做**
  （也让 04 的测试可以用假后端而不必起 TCP）。
- 让步：**JMAP 后端本身**可以一直不做；trait 与 POP3 后端先落地（POP3 是真实需求，JMAP 是可选收益）。

**反驳 3：加日历会不会把我们拖成 PIM 产品（联系人 + 日历 + 任务），从此维护面爆炸？**
- 证据：A1-F23 明确说"日历网格是 2-3 个月的前端布局工程"。
- 改设计：**严格分两步**：M3 只做"邀请只读 + RSVP + 议程"（覆盖"我要回邀请"这 80% 价值）；
  网格、双向 CalDAV、任务全部后置，且**只有用户明确要求才做**。
- 让步：CardDAV 只读优先（不做双向），避免联系人冲突处理的无底洞。

**反驳 4：POP3 是技术倒退（没有文件夹/旗标/多端），做它是为了讨好老用户，值吗？**
- 证据：A1-F28 的结论是"便宜（S 级）且能消除一类硬性反对"。
- 缓解：①定位为**兼容层**，UI 明确降级与说明；②默认"在服务器保留副本"，避免用户以为邮件被搬走；
③文档里建议"如果你的邮箱支持 IMAP，请用 IMAP"。
- 让步：如果实现中发现 POP3 的边界情况（编码、超长行、服务器不发 `UIDL`）导致的成本超过 S，
  就降级为"仅本地导入 + 不支持 POP3 账号"。

**反驳 5：`MailBackend` trait 会不会变成"最低共同分母"，把 IMAP 特有的好东西（CONDSTORE/QRESYNC/THREAD）藏起来？**
- 缓解：①trait 里保留 `capabilities()` 与可选方法（返回 `Unsupported` 时调用方降级）；
  ②**不追求"所有后端行为一致"**，而是"调用方明确知道自己在用什么能力"；
  ③IMAP 特有优化（CONDSTORE/QRESYNC）作为 `ImapBackend` 内部的实现细节，不改 trait。
- 让步：如果某个能力对某后端极其重要（例如 Gmail 标签），就在 trait 上开一个明确的可选方法，
  而不是靠 `capabilities()` 猜。

**反驳 6：Graph 轮询（2 分钟）会不会错过即时性，让"推送"卖点失效？**
- 证据：邮件对多数人不是 IM。IMAP 账号仍是 IDLE 秒级；Graph 账号的延迟是"分钟级"，
  与移动端 Gmail 在后台的行为相当。
- 缓解：设置里可把 Graph 轮询调到 30 s（用户自担配额风险）；文档写明差异。

## 12. 未决问题

| # | 问题 | 影响 | 建议 |
|---|---|---|---|
| X1 | Graph 是否提前到 M2 | 企业可用性 | 先用 M3；有真实需求再提前（反驳 1 的让步） |
| X2 | JMAP 是否真的要做 | 覆盖面 | 做 trait 与 POP3；JMAP 待有用户提出 |
| X3 | 日历网格是否永久不做 | 产品完整性 | 先只做邀请+议程；若用户要网格再评估（M3+） |
| X4 | CardDAV 是否双向 | 联系人一致性 | 只读优先，M3+ 再评估双向 |
| X5 | S/MIME 走 Graph/LDAP 取证书的必要性 | 企业加密 | 与 14 联动；无 GAL 就不承诺企业加密（A1-F18） |
| X6 | 是否支持 CalDAV 的 free/busy 查询 | 排会体验 | 后置（需要 CalDAV 排期协议支持，libdav 覆盖度待验证） |

## 13. 验收清单

- [ ] `MailBackend` trait 落地：IMAP 与 POP3 两个实现，`sync/` 与 `src-tauri` 中无直接 IMAP 引用（门禁验证）。
- [ ] POP3：`UIDL` 稳定去重、`QUIT` 才删除、保留策略三档可测；服务器无 `UIDL` 时明确拒绝。
- [ ] 能力位驱动 UI 降级：POP3 账号看不到文件夹/旗标/移动，且提示准确（真机截图验收）。
- [ ] 邀请：REQUEST/REPLY/CANCEL/COUNTER 四类样本解析正确；RSVP 往返被 Google/Outlook 接受（真机）。
- [ ] RRULE 跨 DST 与 `RECURRENCE-ID` 覆盖用例通过；无限重复不被物化。
- [ ] Graph/JMAP 留出接口与假服务器测试（即使后端未实现，契约测试要在）。
- [ ] `check.ps1` 五项新增检查通过。

> **章节名对照（模板 13 节）**：数据结构→§3 后端抽象+§4 事实表；接口→§5 接口与实现要点。
> 内容齐全，只是节名与模板不完全同名（见 design/README.md 的说明）。
