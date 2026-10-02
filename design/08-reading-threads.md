# 08 阅读、会话串与附件预览

> **目标**：让"读邮件"这件事达到现代客户端水准：**会话串**、引用折叠、安全的富内容渲染、
> 附件不下载先看、打印与导出、多设备一致的已读/旗标。
>
> 覆盖 ID：READ-01…15、UI-19（打印样式）。上游：03（表）、04（正文取回）、09（搜索定位）。
> 下游：12（视觉与交互）、14（安全策略）。

---

## 1. 目标与非目标

**目标**
1. 会话串（threading）：同一话题只有一行，点开是完整往返，**可手动拆分/合并**。
2. 阅读安全：默认零外部请求（红线 2），同时"显示图片"只点一次就记住该发件人（SEC-03）。
3. 附件：不下载就能看（图/文本/HTML-safe），危险类型明确拦（SEC-06），下载可续、可取消。
4. 打印/导出：打印样式干净（UI-19）；导出 .eml / 整会话。
5. 快速导航：键盘上一封/下一封、会话内跳转、未读跳转。
6. 状态一致：已读/旗标/删除本地立即生效 + 服务器收敛（04 §5.3 的队列）。

**非目标**
- 不做内嵌完整 PDF 阅读器（M3 评估 pdf.js；M2 用系统查看器，见 §4.6）。
- 不做邮件模板渲染引擎（那是写信侧，07）。
- 不做 PGP 解密（M3，14）。

## 2. 用户可感知行为

| 场景 | 用户看到 |
|---|---|
| 打开收件箱 | 列表按会话聚合：`张三(3)`，未读小圆点按"会话内未读数"显示；点开默认展开最新一封 |
| 看长会话 | 每封之间有头像+时间+折叠条；引用部分折叠成"…"（点一下展开）；顶部有"展开全部" |
| 会话里混了不同话题 | 右键 → "拆分此会话"（把选中的邮件拎出去）/"合并到…" |
| 带远程图片的邮件 | 顶部黄条"已拦截 12 张远程图片和 3 个跟踪器"，按钮"显示图片（仅本次）"/"总是显示来自 xxx 的图片" |
| 附件 | 图/文本直接预览；PDF 给"用系统程序打开"；.exe/.js/双扩展名显示红色警告且不预览 |
| 打印 | Ctrl+P 打开打印预览：只印正文与必要的头部，去掉 UI 与远程图（除非已解锁） |
| 导出 | 右键"导出为 .eml"/"导出整个会话"；导出的是原始字节 |
| 键盘 | `J/K` 上下移动，`Enter` 打开，`U` 未读，`S` 旗标，`E` 归档，`#` 删除，`R` 回复 |
| 断网 | 已缓存的正文照常读；未缓存的显示"离线，联网后可读"+ 重试按钮 |

## 3. 数据结构

主要用 03 已定义的表；本模块新增/依赖的字段与视图：

```rust
/// 会话列表行（不是 messages 行）：用于 message_list 的会话视图
#[derive(Serialize, Clone, Debug)]
pub struct ThreadRow {
    pub thread_id: String,
    pub subject: String,
    pub participants: Vec<Participant>,     // 去重，最多 5
    pub newest_folder: String, pub newest_uid: u32,
    pub last_date: i64, pub message_count: u32,
    pub unread_count: u32, pub flagged: bool, pub has_attachments: bool,
    pub labels: Vec<String>,
}

#[derive(Serialize, Clone, Debug)]
pub struct Participant { pub name: Option<String>, pub addr: String }

/// 渲染策略（每次打开邮件构造一次；"总是显示"来自 allowlist 表）
#[derive(Clone, Debug)]
pub struct RenderPolicy {
    pub allow_remote: bool,          // 本次是否解锁
    pub allow_forms: bool,           // 默认 false（sandbox 已禁）
    pub max_inline_bytes: usize,     // 内嵌图片总预算（默认 2 MB，超出只显示前 N 张）
    pub rewrite_links: bool,         // 把 <a href> 改写为可拦截的 click 处理
    pub block_tags: &'static [&'static str],  // script/iframe/object/embed/meta[http-equiv]
}

/// 跟踪器分析结果（SEC-04）
#[derive(Serialize, Clone, Debug)]
pub struct TrackerReport {
    pub remote_images: u32,
    pub unique_domains: Vec<String>,
    pub suspected_pixels: Vec<TrackerInfo>,   // width/height ≤ 2 或不可见样式 + 非 cid:
    pub external_css: u32, pub web_beacons: u32,
}
#[derive(Serialize, Clone, Debug)]
pub struct TrackerInfo { pub url: String, pub domain: String, pub reason: &'static str }

/// 引用折叠（READ-02）
#[derive(Serialize, Clone, Copy, Debug)]
pub struct QuoteFold { pub start: usize, pub end: usize, pub kind: QuoteKind }
#[derive(Serialize, Clone, Copy, Debug)]
pub enum QuoteKind { Blockquote, OutlookHeader, OriginalMessage, GmailQuote, SignatureBlock }

/// 附件预览载荷（READ-05）
#[derive(Serialize, Clone, Debug)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PreviewPayload {
    Image { mime: String, data_base64: String, width: Option<u32>, height: Option<u32> },
    Text { text: String, truncated: bool, language: Option<String> },
    HtmlSafe { sanitized: String },          // 再套一层 sandbox iframe 渲染
    Unsupported { reason: String, open_with_system: bool },
    Blocked { reason: String },              // 危险类型（SEC-06）
    TooLarge { size: u32, limit: u32 },
}
```

**索引**：`threads(account_id, last_date desc)`（已有）、`messages(account_id, thread_id, date)`（已有）、
`messages(account_id, message_id)`（线程归属查找用，已有）。

## 4. 接口（函数级）

### 4.1 会话串算法（`mail-core::threading`）

```rust
/// 归一化主题：去 Re:/Fwd:/回复:/答复:/转发:/[list-tag]/多余空白与大小写差异
pub fn normalise_subject(subject: &str) -> String;

/// 单封邮件的线程归属输入
pub struct ThreadInput<'a> {
    pub uid: u32, pub message_id: Option<&'a str>, pub in_reply_to: Option<&'a str>,
    pub references: &'a [String], pub subject: &'a str,
    pub from_addr: Option<&'a str>, pub date: Option<i64>, pub kind: MessageKind,
}
#[derive(PartialEq, Eq, Debug)]
pub enum MessageKind { Normal, Reply, Forward, Draft }

/// 线程归属结果
#[derive(Debug)]
pub struct ThreadAssignment { pub thread_id: String, pub merged_from: Vec<String>, pub reason: AssignReason }
#[derive(Debug)]
pub enum AssignReason { InReplyTo, References, SubjectUnique, NewRoot }

/// 用数据库做查找（注入以便单测）：按 message_id / subject_norm 找已有会话
pub trait ThreadLookup {
    fn thread_of_message(&self, message_id: &str) -> Option<String>;
    fn threads_by_subject(&self, norm: &str, since: i64) -> Vec<ThreadCandidate>;
    fn root_message_id(&self, thread_id: &str) -> Option<String>;
}
pub struct ThreadCandidate { pub thread_id: String, pub last_date: i64, pub participants: Vec<String> }

/// 核心：为一批邮件分配 thread_id（幂等；同一输入永远同一结果）
pub fn assign(rows: &mut [MessageRow], lookup: &dyn ThreadLookup, opts: &ThreadOptions) -> Vec<ThreadAssignment>;

pub struct ThreadOptions {
    /// 主题兜底合并的时间窗（默认 30 天）
    pub subject_window_secs: i64,
    /// 主题兜底要求至少一个参与者重叠（默认 true；防止"Invoice"大合并）
    pub subject_requires_participant: bool,
    /// 转发是否另起会话（默认 true：Fwd 是新话题）
    pub forward_starts_new_thread: bool,
}

/// 批量落库：分配 + 维护 threads 聚合（一个事务）
pub async fn assign_and_store(db: &Db, account_id: &str, rows: &mut [MessageRow]) -> Result<u32>;
/// 全量重建（历史数据、算法变更后）
pub async fn rebuild_all(db: &Db, account_id: &str, ctx: &JobCtx) -> Result<u64>;
/// 手动拆分/合并（UI：右键）
pub async fn split(db: &Db, account_id: &str, keys: &[MsgKey]) -> Result<String>;
pub async fn merge(db: &Db, account_id: &str, from: &str, into: &str) -> Result<u64>;
```

**算法（确定性、可解释、可单测）**
```
对每封邮件，按顺序尝试：
1) In-Reply-To 指向的 message_id → 该邮件所在 thread（最强信号）
2) References 链**从后往前**找第一个已知 message_id → 其 thread
3) 否则：若 kind == Forward 且 forward_starts_new_thread → 新会话
4) 否则按 subject_norm 在同一账号内找候选：
   - 时间窗内（默认 30 天）且（要求参与者重叠时）至少一个 addr 重叠
   - 恰好 1 个候选 → 并入；多个候选 → 选最近的一个，并把其余标记为"可合并建议"（不自动合并）
   - 0 个候选 → 新建会话
5) thread_id 命名：优先"根邮件的 message_id"；根无 message_id 时用
   `t-<sha1(subject_norm|first_addr|date_bucket)>`，保证确定性（重同步不改名）
```
**为什么要"根 message_id"当 thread_id**：重同步、换设备、重建索引都会得到同一个 id，
不会出现"同步两次会话串全变"的抖动。

**服务器 THREAD 扩展：不要用**（A1-F42/F43）。imap-proto 0.17 **没有 `* THREAD` 的语法**，
服务器一回应就会把整条连接判为解析失败并关闭 ⇒ **发这条命令等于自杀**。
会话串完全本地算（§4.1），而且本地算的更准（跨文件夹、可手动拆合）。
- 唯一值得争取的服务端捷径是 Gmail 的 `X-GM-THRID`：imap-proto **已经解析**它，
  但 async-imap 的 `Fetch` 没有访问器（约 10 行的上游 PR，A1-F46）。拿到后可作为 Gmail 账号的
  **交叉校验/加速**（不改变本地算法，只是把 Gmail 的会话边界作为强提示）。
- `references` 的取法有坑（A1-F46）：`mail_parser::HeaderValue` 是 `Empty | Text | TextList` 三态，
  **只有一个 id 时是 `Text`**；而 `as_text()` 作用在 `TextList` 上**只返回最后一个元素**。
  取 References 链必须三种分支都处理，否则"回复链"会被截断成单个 id（会直接毁掉会话串）。
  另外 `message_id()` **已经剥掉尖括号**，不要再去剥一次。
- 主题兜底优先用 `message_id/thread_name()`（RFC 5256 的 base subject，含多语言 Re:/Fwd: 剥离），
  比自己写正则更稳。

### 4.2 会话与列表查询

```rust
pub struct ThreadQuery { pub account_id: String, pub folder: Option<String>, pub filter: ListFilter,
                         pub sort: ListSort, pub offset: u32, pub limit: u32, pub unread_first: bool }
#[derive(Serialize, Clone, Debug)]
pub struct ThreadPage { pub items: Vec<ThreadRow>, pub total: u32 }

pub async fn thread_page(db: &Db, q: ThreadQuery) -> Result<ThreadPage>;
pub async fn thread_members(db: &Db, account_id: &str, thread_id: &str) -> Result<Vec<ThreadMessage>>;  // 含 folder（跨文件夹）
#[derive(Serialize, Clone, Debug)]
pub struct ThreadMessage { pub folder: String, pub uid: u32, pub summary: MessageSummary, pub body_cached: bool }

/// 会话级操作（等价于对成员逐个操作，但一次事务 + 一条队列记录集合）
pub async fn thread_apply(db: &Db, account_id: &str, thread_id: &str, op: ThreadOp) -> Result<ThreadOpReport>;
pub enum ThreadOp { MarkSeen(bool), Flag(bool), Move(String), Delete, Archive, AddLabel(String), RemoveLabel(String), Snooze(i64) }
#[derive(Serialize, Clone, Debug)]
pub struct ThreadOpReport { pub affected: u32, pub skipped: u32, pub undo_token: Option<String> }
```

### 4.3 渲染（安全）

```rust
/// 把邮件 HTML 变成可放进 sandbox iframe 的文档（既有 buildSandboxedDocument 的正式化）
pub fn build_document(html: &str, inline: &[InlineImage], policy: &RenderPolicy, theme: ThemeVars) -> String;

/// 把 cid: 引用替换为 data:（内嵌图片不受"远程图片"策略影响，因为不产生网络请求）
pub fn resolve_inline(html: &str, inline: &[InlineImage]) -> String;

/// 统计与跟踪器分析（发送给 UI 显示黄条文案）
pub fn analyse_external(html: &str) -> TrackerReport;

/// 链接改写：包成 <a data-ee-href="真实地址" href="#">，点击由外层处理（悬停显示真实域名）
pub fn rewrite_links(html: &str) -> String;
/// 危险 scheme（javascript:, data:text/html, file:, ms-msdt:…）
pub fn is_dangerous_url(url: &str) -> bool;

/// 引用折叠检测（保守：只折叠明确的引用容器与签名块）
pub fn detect_quotes(html: &str) -> Vec<QuoteFold>;

/// 纯文本正文渲染（等宽、保留换行、链接自动识别）
pub fn render_plain(text: &str) -> String;
```

**安全策略（与 14 联动，**第二轮评审修正了其中两处会漏的做法**）**
- `iframe sandbox`（无 `allow-same-origin`、无脚本）+ 独立 CSP（继承规则见既有 README 第 8 条）：
  默认 `img-src data: blob:`；解锁后 `img-src data: blob: https: http:`；永不放宽 `script-src`。
- **发送前必须剥离（不是"可选优化"）**：`<script>`、`<iframe>`、`<object>`、`<embed>`、
  **`<meta http-equiv>`（尤其是 `refresh`）**、`on*=` 属性、`javascript:` / `data:text/html` URL。
  **为什么 `<meta http-equiv="refresh">` 是必须项**：CSP 的 `default-src 'none'` **不限制导航**，
  而 `sandbox` 不带 `allow-top-navigation` 只挡**顶层**导航 ⇒ 一封带 refresh 的邮件在打开时会
  **自动发起外部请求**，直接推翻"默认零外部请求"这条产品承诺（第二轮评审实测的路径）。
- **链接的拦截机制必须换掉（原设计做不到）**：原来写"包成 `<a data-ee-href=… href="#">`，点击由外层处理" ——
  但沙箱里没有 `allow-same-origin`、也没有脚本，**父页面永远收不到那个点击**。改成：
  1. 把 `href` 改写成自定义 scheme：`easyemail-link:<urlencoded 原始 URL>`（`href` 不再是 http/https，
     所以即使被点开也不会直接联网）；
  2. 在 **WebView2 层**拦导航（Tauri 的 `on_navigation` / `FrameNavigationStarting`）：
     - `easyemail-link:` → **取消导航** + 取出真实 URL → 走 §5.3 的确认弹窗（显示真实域名）→ 用系统浏览器打开
     - 其它任何 scheme（`http`/`https`/`file`/`data`/自定义）→ **一律取消**并记日志
  3. 兜底：正文里同时提供"这封邮件里的链接（N 个）"列表，渲染在 iframe **外面**（不依赖沙箱内的交互）
- 「总是显示来自该发件人的图片」写进 `allowlist(purpose='remote_images')`，
  **且只有在认证通过（DKIM/DMARC）时才生效**（14 §4.2 的 `auth_ok`），可在设置里撤销。

### 4.4 附件

```rust
pub struct AttachKey { pub account_id: String, pub folder: String, pub uid: u32, pub part: u32 }

/// 元数据（不取字节）
pub async fn list_attachments(db: &Db, key: &MsgKey) -> Result<Vec<AttachmentRow>>;

/// 预览：小文件直接给载荷；大文件给 TooLarge（并在 UI 上给"仍要预览"）
pub async fn preview(ctx: &AppCtx, key: &AttachKey, opts: PreviewOptions) -> Result<PreviewPayload>;

/// 下载/另存：流式（大附件不双份驻留内存），可取消，带进度事件
pub async fn save_as(ctx: &AppCtx, key: &AttachKey, dest: &Path, job: &JobCtx) -> Result<SaveReport>;

/// 取消保存时删除半截文件（保证不留下损坏文件）
pub fn abort_save(dest: &Path) -> Result<()>;

/// 危险类型判定（SEC-06）
pub fn classify_attachment(name: &str, mime: &str) -> AttachmentRisk;
#[derive(Serialize, Clone, Copy, Debug)]
pub enum AttachmentRisk { Safe, Caution, Dangerous }   // 双扩展名/宏文档/可执行/脚本
```

### 4.5 打印与导出

```rust
/// 打印：生成一个只含正文的打印文档，交给 WebView 打印（保留远程图片策略）
pub fn build_print_document(messages: &[ThreadMessage], bodies: &[BodyRow], policy: &RenderPolicy) -> String;
pub async fn print_thread(app: &AppHandle, account_id: &str, thread_id: &str) -> Result<()>;

/// 导出（03 的 export 模块提供 mbox；这里是单封/会话）
pub async fn export_eml(ctx: &AppCtx, keys: &[MsgKey], dest: &Path, job: &JobCtx) -> Result<ExportReport>;
```

### 4.6 旗舰行为清单（明确到什么程度算做完）

| 能力 | M1 | M2 | M3 |
|---|---|---|---|
| 会话串（自动 + 手动拆/合） | ✅ | | |
| 引用折叠 | ✅ | | |
| 安全渲染 + 远程图片策略 + 发件人白名单 | ✅ | | |
| 跟踪器统计 | | ✅ | |
| 附件：图/文本预览、危险类型拦截、流式另存 | ✅ | | |
| 附件：PDF 内嵌预览 | | ⛔ 用系统查看器 | 评估 pdf.js |
| 附件：Office 预览 | | ⛔ | 评估（转换服务不允许上传，所以只能本地转换，成本高） |
| 打印 + 打印样式 | | ✅ | |
| 导出 .eml / 会话 / mbox | | ✅ | |
| 稍后提醒（Snooze） | | ✅ | 可选的服务器侧移动 |
| 邮件源码/完整头部查看 | | ✅ | |

## 5. 状态机与主流程

### 5.1 打开一封邮件
```
点击 thread → thread_members（本地）
  ├ 若该封 body_cached → db 读 bodies → build_document → 设 iframe srcdoc
  └ 否则 → 入队 fetch_and_store_body（优先级 UserAction）→ 事件 → 渲染；失败给"重试"
渲染后：analyse_external → 黄条（远程图片/跟踪器）→ 用户点"显示图片" → 只重建文档（不重新取正文）
标记已读：本地立即 + pending_ops（可撤销 5 s）
```

### 5.2 会话视图状态机
```
collapsed(全部) ──点击某封──▶ focused(该封展开, 其余折叠)
   │                              │
   ├──"展开全部"──▶ expanded      ├──"显示引用"──▶ 该封引用展开
   └──筛选/搜索命中──▶ 定位到该封（高亮 2 s）并展开
```
- 默认：**最新一封展开**，其余折叠（邮件客户端共识）。
- 会话内未读：打开会话即把可见的未读标记为已读？**不**：只有**展开的那封**标已读（避免误标），
  其余保持未读，直到用户展开或点"全部标为已读"。

### 5.3 附件预览/保存
```
预览：classify_attachment → Dangerous? → Blocked（不取字节）
                                     ├ Safe/Caution → 若 sha 已在 blobs → 直接给载荷
                                     └ 否则取回（流式，≤ 预览上限）→ 存 blobs（refcount+1）→ 载荷
保存：系统保存对话框 → 流式写临时文件 → 完成后原子改名 → 记录 saved_path
      取消/失败 → 删除临时文件（不留半截）
```

## 6. 失败模式与降级

| 触发 | 用户看到 | 数据 | 处理 |
|---|---|---|---|
| 正文取回失败 | 阅读区"正文读取失败（原因）+ 重试" | 无 | `index_queue.failed`，不阻塞列表 |
| 正文超长（> 5 MB HTML） | 提示"这是一封超大邮件，已截断渲染" | 原文仍保留 | 渲染截断 + "查看原文"入口 |
| 会话成员跨账号（同主题在不同账号） | 不合并（thread_id 带账号前缀） | 无 | 设计上按账号隔离 |
| 手动合并造成环（A 并入 B，B 已在 A） | 拒绝并提示 | 无 | merge 前查祖先 |
| 附件损坏/解码失败 | 该附件标"无法预览"，仍可另存原始字节 | 无 | 记日志 |
| 打印时含远程图片且未解锁 | 打印稿里图片位置显示占位框 | 无 | 打印前提示"有 N 张图片被拦截，是否解锁后打印" |
| 已读标记在服务器上失败 | 行尾"待同步"标记；重试 N 次后横幅提示 | 本地已读、服务器未读（会回滚吗？） | **不回滚**：保留本地意图并持续重试（04 §5.3 LWW），用户可见状态不闪 |
| 会话计数漂移（并发同步导致） | 无感 | 计数短暂不准 | 夜检 + 定时（5 分钟）重算受影响会话 |

## 7. 迁移与兼容

- 现有 `message_list`（平铺）→ 会话视图：M1 提供**切换开关**（默认会话）；平铺模式保留（老用户习惯）。
- 现有 `MessageDetail`/`bodies` 表结构保留，新增 `raw_zstd`/`snippet` 等列（03 的 v3 迁移）。
- 现有 `buildSandboxedDocument()` 的 CSS 规则（像素宽度中和、百分比排除、图片不加 max-height）
  **原样保留**并搬进新实现，`check.ps1` 的现有四项检查继续生效。
- 会话串上线时对已有缓存执行一次 `rebuild_all`（后台任务，可暂停）。

## 8. 性能预算

| 指标 | 预算 |
|---|---|
| 会话列表首页（10 万封 → 3 万会话） | ≤ 40 ms |
| 打开会话（≤ 50 封成员，纯本地） | ≤ 150 ms |
| 渲染一封正文（含内嵌图解码） | ≤ 100 ms |
| 会话串离线重算（10 万封） | ≤ 60 s（后台，可暂停） |
| 附件预览（≤ 2 MB） | ≤ 300 ms |
| 附件另存 100 MB | 内存增量 ≤ 8 MB（流式块 256 KB） |
| 滚动会话内 500 封邮件 | 只渲染展开项 + 虚拟化 ≤ 60 fps |

## 9. 测试计划

| 层 | 用例 |
|---|---|
| 单元（`threading`） | 经典线程夹具（含：缺失 Message-ID、循环引用、只有 References、主题兜底、Fwd 另起、同主题不同人、跨文件夹成员、重同步幂等）——**用 JWZ 那套公开测试邮件的等价夹具**自建 |
| 单元（渲染） | 危险标签/属性剥离、`cid:` 替换、远程图片统计（含 1×1 像素识别）、链接改写、危险 scheme、引用折叠的保守性 |
| 单元（附件） | 危险类型判定（`invoice.pdf.exe`、`report.docm`、`.js`、`.lnk`、Unicode 双向欺骗字符文件名） |
| loopback | 打开未缓存正文 → 取回 → 再打开不产生网络调用；附件流式另存与服务器字节一致（既有测试扩展） |
| 真机 | 20 封真实邮件（含 GB2312、多语言、带内嵌图、带远程图、带附件）渲染无异常；打印稿目视检查 |
| UI 冒烟 | 会话展开/折叠、显示图片（本次/总是）、附件预览与另存、打印、导出 |
| 性能 | §8 各项 |

## 10. 门禁新增项

1. **零外部请求默认**：既有四项检查保留；新增"`build_document` 的默认分支不含 https/http"（沿用旧检查思路，改指向新函数）。
2. **线程确定性**：单测断言"同一夹具跑两次得到相同 thread_id 集合"。
3. **危险类型覆盖**：单测枚举 `AttachmentRisk::Dangerous` 的扩展名清单，清单变化必须同步改测试（防止悄悄放宽）。
4. **禁止 `innerHTML` 渲染正文**：前端源码检查——正文只能进 `iframe.srcdoc`，不得 `document.innerHTML = html`。
5. **打印稿不含脚本**：`build_print_document` 的单测断言输出不含 `<script`/`on*=`。
6. **正文 frame 永不导航（第二轮评审新增）**：①净化单测断言输出里没有 `<meta http-equiv`；
   ②CDP 冒烟：打开一封带 `<meta http-equiv="refresh">` 与 `<a href="https://…">` 的邮件，
   **断言没有任何对外请求、frame 的 URL 没有变过**；③源码检查：导航拦截必须同时覆盖
   `easyemail-link:` 与"其它 scheme 一律取消"。

## 11. 逆向反驳（红队）

**反驳 1：自己做线程串一定会出错（把不相关的邮件并到一起，或把同一话题拆成两串），
而错误比"没有线程串"更烦人。**
- 承认风险。缓解：①主信号只用 References/In-Reply-To（确定性）；主题兜底加**时间窗 + 参与者重叠**双约束；
  ②**人工兜底**：右键拆分/合并（很多客户端没有，这是加分项）；③错误可恢复（不删数据，只改 `thread_id`）。
- 让步：主题兜底默认**只做"唯一候选且参与者重叠"**这一种情况，其余不合并 —— 宁可少合并，不可错合并。

**反驳 2：会话视图会破坏既有"点一行=一封邮件"的心智与代码路径（删除/移动/标记）。**
- 缓解：视图层是**聚合**，操作层永远落到具体 (folder, uid) 集合；UI 明确显示"将影响 3 封邮件"；
  M1 保留平铺模式开关；会话操作的实现复用单封操作（同一队列与事务）。
- 让步：删除/移动到垃圾箱这类破坏性操作在会话视图默认**只作用于展开的那封**，
  "整个会话"需要显式选择（避免一键删掉 20 封）。

**反驳 3：附件预览承诺"不下载先看"，但为了预览还是要把字节取回并落库，等于偷偷下载了附件
（用户可能只想知道那是什么）。**
- 证据：确实。而且大附件落库会吃磁盘。
- 改设计：①预览**默认只在内存**（不落 `blobs`），只有用户"另存"或"总是预览该类型"才落库；
  ②预览大小上限（默认 2 MB，图片 10 MB），超限只给"另存"；
  ③磁盘占用在设置页可见（03 quota）。

**反驳 4：`allowlist` 记住"总是显示图片"会削弱隐私默认值。**
- 缓解：白名单按**完整发件人地址**、可在设置里一键清空、在邮件头部有可见提示
  （"已按你的设置自动显示来自 xxx 的图片"，带"改为总是询问"）；白名单不进备份？→ 进备份（属于用户设置）。
- 红线检查：默认永远是拦截；白名单是用户显式行为。

**反驳 5：引用折叠会切掉真实内容（有些邮件正文就写在 blockquote 里）。**
- 缓解：折叠**保守**（只折叠明确的 `blockquote`/Outlook 分隔头/Gmail quote 容器/签名块）；
  第一段永不折叠；折叠状态在会话内记忆；提供"总是展开引用"设置。

**反驳 6：Snooze 只做本地隐藏，别的设备还能看到 —— 用户会觉得"没生效"。**
- 承认。设计明确写：Snooze = **本地稍后提醒**（隐藏 + 到点提醒），不是服务器操作；
  设置项里有一句说明。M3 可选"同时移到 Snooze 文件夹"（服务器可见），但那会污染文件夹列表（Gmail 例外，
  它有原生 Snooze 语义，可作为 PROTO-03 的收益点）。

**反驳 7：跨文件夹会话会让"这封邮件在哪个文件夹"变得模糊，删除时搞不清删的是哪一封。**
- 缓解：会话视图里每封显示所属文件夹标签（复用既有 `.msg-folder` 徽标）；
  删除前确认框列出"将删除：收件箱 2 封、归档 1 封"。

## 12. 未决问题

| # | 问题 | 影响 | 建议 |
|---|---|---|---|
| T1 | 默认视图是会话还是平铺 | 老用户习惯 | M1 默认会话 + 开关；老用户升级时保留其偏好 |
| T2 | 主题兜底合并的时间窗（30 天？）与参与者约束 | 误合并率 | 先用 30 天 + 必须重叠，实测后调 |
| T3 | 附件预览是否需要落库 | 磁盘 | 默认不落库（§11 反驳 3） |
| T4 | 打印走 WebView2 打印对话框还是自绘页面 | 一致性 | 先走系统打印对话框（最省事且用户熟悉） |
| T5 | 会话内"打开即已读"的范围 | 用户预期 | 只标展开项（§5.2） |
| T6 | 是否需要"对话中的附件汇总"视图 | 便利 | M2 加分项，先不做 |

## 13. 验收清单

- [ ] 线程夹具（≥ 12 类边界）全部通过，且两次运行结果一致。
- [ ] 打开一封未缓存正文的邮件：≤ 1 次网络取件，第二次打开零网络调用。
- [ ] 默认零外部请求（抓包）；点"显示图片"后仅该封解锁；"总是显示"按发件人生效并可撤销。
- [ ] 危险附件不预览且有明确原因；另存得到与服务器一致的字节（既有往返测试扩展到 ≥ 20 MB）。
- [ ] 打印稿不含脚本/UI/被拦截的远程图（除非已解锁）。
- [ ] 会话视图下删除/移动的作用范围在 UI 上明确可见。
- [ ] §8 六项预算达标；`check.ps1` 五项新增检查通过。
