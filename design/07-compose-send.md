# 07 写信与发信管线（草稿、发件箱、撤销、附件、签名模板）

> **目标**：让"写一封信并把它可靠地送出去"这件事**永不丢**：崩溃不丢草稿、断网不丢发送、
> 服务器拒收能看懂原因、发出去的副本在"已发送"里找得到。
>
> 覆盖 ID：MAIL-01…18、UI-19（打印样式的一部分）、PERF-05（大附件流式）。
> 事实依据：A1-F3（各服务商 APPEND/邮件大小上限）、A1-F1/F2（配额与窗口）。
> 上游：03（drafts/outbox 表）、04（连接预算）、11（收件人补全）。下游：08（引用与转发）。

---

## 1. 目标与非目标

**目标**
1. **受限富文本**：粗体/斜体/下划线/删除线、有序无序列表、链接、引用块、内嵌图片 —— 生成**干净、跨客户端可读**的 HTML，同时永远带一份 `text/plain`（MAIL-01）。
2. **草稿双保险**：本地即时保存（`user.db`）+ 服务器 Drafts 镜像；崩溃/断电后原样恢复（MAIL-03）。
3. **发件箱**：离线入队、退避重试、可见可取消、可手动立即发送（MAIL-04）。
4. **撤销发送**：默认 5 秒可撤回（可关）（MAIL-05）。
5. **发送前预检**：大小/收件人数量/`SMTPUTF8` 需求/附件变化，**在点击发送之前**告诉用户（MAIL-09/15）。
6. **已发送副本可靠**：发完后校验服务器是否自动归档，没有就 `APPEND` 一份（避免"发出去的信在已发送里找不到"）。
7. **协议细节不再踩老坑**：`singlepart(SinglePart::plain(..))`、`references()` 一次性传整条链（既有教训，见 `HANDOFF.md`）。

**非目标**
- 不做所见即所得的 newsletter 排版（表格布局、CSS 类、像素级控制）—— 已明确否决（`01` §6）。
- 不做邮件合并/群发营销、不做发送跟踪像素（那与我们的隐私立场冲突）。
- 不做 S/MIME/PGP 加密（M3，14）。

## 2. 用户可感知行为

| 场景 | 用户看到 |
|---|---|
| 点"写邮件" | 打开写信面板（M2 可独立窗口）；焦点在收件人；右侧无广告、无模板推荐 |
| 打字 | 每 2 秒（或失焦/切窗口）自动保存；标题栏显示"草稿 · 已保存 14:32" |
| 断网时写 | 照常；状态显示"离线，草稿已存本地" |
| 写完点发送（默认 5 秒撤销） | 顶部条"正在发送…（5 秒内可撤销）"；撤销 → 回到写信界面，内容一字不少 |
| 附件 60 MB，服务商上限 45 MB | **点击添加时就提示**："这个邮箱单封上限约 45 MB，当前 62 MB。可以改用云盘链接或拆分发送。"（不是等服务器拒绝） |
| 收件人 200 个 | 提示"该服务商单封收件人上限约 100 个，建议拆分发送或使用密送" |
| 拼错域名（`gmial.com`） | 收件人下方黄字"是不是想输入 gmail.com？"（本地相似度检查，不发请求） |
| 发送失败（SMTP AUTH 被租户关闭） | 发件箱里那一封标红 + "你的管理员关闭了此邮箱的发信权限"（05 §6） |
| 关掉应用再打开 | 草稿与发件箱原样还在；未发完的继续排队 |
| 想稍后发 | "定时发送"（M2）：选时间 → 进发件箱（状态"已排期"） |
| 发完 | 立刻出现在"已发送"（服务器自动归档，或我们 `APPEND` 的副本） |
| 收件人退回 | 退信被解析（M2），原邮件上出现"投递失败：地址不存在"的提示 |

## 3. 数据结构

```rust
/// 收件人（带显示名）
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct Recipient { pub name: Option<String>, pub addr: String }

/// 身份（一个账号可以有多个发件身份，MAIL-18）
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Identity {
    pub id: String, pub account_id: String,
    pub from_addr: String, pub display_name: String,
    pub reply_to: Option<String>, pub signature_id: Option<String>,
    /// 该身份是否允许直接作为 SMTP envelope sender（多数服务商只允许登录地址）
    pub envelope_from_allowed: bool,
    pub is_default: bool,
}

/// 待发送邮件（冻结后可重试）
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct OutgoingMessage {
    pub account_id: String, pub identity_id: Option<String>,
    pub to: Vec<Recipient>, pub cc: Vec<Recipient>, pub bcc: Vec<Recipient>,
    pub subject: String,
    pub body_text: String, pub body_html: Option<String>,
    pub in_reply_to: Option<String>, pub references: Vec<String>,   // 整条链，一次性
    pub attachments: Vec<OutgoingAttachment>,                       // 见 model.rs（路径 + 大小）
    pub inline_images: Vec<OutgoingInline>,                         // {content_id, path, mime}
    pub request_mdn: bool,
    /// 转发时保留的原始头（用于 In-Reply-To/References 之外的可选头，如 X-Forwarded-Message-Id）
    pub extra_headers: Vec<(String, String)>,
}
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct OutgoingInline { pub content_id: String, pub path: String, pub mime: String, pub size: u64 }

/// 生成结果（spool 到磁盘，避免大附件驻留内存）
#[derive(Debug)]
pub enum BuildOutcome {
    Spooled { path: PathBuf, size_bytes: u64, parts: u32, message_id: String },
    TooLarge { limit: u64, actual: u64 },
    AttachmentChanged { name: String, expected_sha: String, actual_sha: String },
    AttachmentMissing { name: String, path: String },
}

/// 发送前预检（UI 直接渲染这些条目，不自己编文案）
#[derive(Serialize, Clone, Debug)]
pub struct ValidationIssue { pub severity: Severity, pub code: IssueCode, pub message: String, pub fix: Option<String> }
#[derive(Serialize, Clone, Copy, Debug)] pub enum Severity { Info, Warning, Blocker }
#[derive(Serialize, Clone, Copy, Debug)]
pub enum IssueCode {
    TooLarge, RecipientCount, EmptySubject, EmptyBody, BadAddress, DomainTypo,
    NonAsciiAddressNeedsSmtputf8, SmtpAuthDisabled, AttachmentChanged, AttachmentMissing,
    NoIdentity, ReplyToMismatch, DuplicateRecipient,
}
pub fn validate(msg: &OutgoingMessage, provider: &Provider, identity: Option<&Identity>) -> Vec<ValidationIssue>;
```

**签名与模板**（`user.db`）

```rust
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Signature {
    pub id: String, pub account_id: String, pub name: String,
    pub text: String,                 // 纯文本版
    pub html: Option<String>,         // 受限 HTML 版（由编辑器产生）
    pub mode: SignatureMode,          // Auto | Manual | None
    pub is_default: bool,
}
#[derive(Serialize, Deserialize, Clone, Copy, Debug)] pub enum SignatureMode { Auto, Manual, None }

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Template {
    pub id: String, pub account_id: Option<String>, pub name: String,
    pub subject: Option<String>, pub text: String, pub html: Option<String>,
    pub variables: Vec<String>,       // 模板里出现的变量名（导入时解析）
}
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct TemplateVars { pub to_name: Option<String>, pub to_addr: Option<String>, pub date: Option<String>, pub account_name: Option<String> }
pub fn render_template(t: &Template, vars: &TemplateVars) -> Result<(String, Option<String>)>;   // 转义规则见 §11
```

**发件箱状态**（`outbox` 表，03 已定义）扩展语义：
`queued`（含撤销窗口/排期）→ `sending` → `sent` | `failed` | `cancelled`；
`scheduled_at`（定时）、`undo_until`（撤销窗口）、`attempts`/`next_attempt_at`（退避）。

## 4. 接口（函数级）

```rust
// ── 组信 ──────────────────────────────────────────────────────
pub struct BuildOptions {
    pub spool_dir: PathBuf,
    pub keep_bcc_header: bool,          // 恒 false：Bcc 只进 RCPT TO
    pub encode_attachments: Encoding,   // Base64 | QuotedPrintable | Auto（按内容嗅探）
    pub max_inline_image_bytes: u64,    // 超过则降级为普通附件（默认 5 MB）
    pub chunk_bytes: usize,             // 流式块大小（默认 256 KB）
}
/// 把邮件写成 MIME 并 spool 到磁盘（**大附件不双份驻留内存**）
pub async fn build_to_spool(msg: &OutgoingMessage, cfg: &AccountConfig, provider: &Provider, opts: &BuildOptions) -> Result<BuildOutcome>;
/// 估算最终编码后大小（base64 膨胀 + 头部），用于"发送前"预检
pub fn estimate_encoded_size(msg: &OutgoingMessage, identity: Option<&Identity>) -> u64;
/// 生成 Message-ID（域名取发件人域；失败退回本机标识）
pub fn generate_message_id(from_domain: &str) -> String;

// ── 发送 ─────────────────────────────────────────────────────
pub async fn enqueue(db: &Db, msg: OutgoingMessage, opts: EnqueueOptions) -> Result<i64>;
pub struct EnqueueOptions { pub undo_window: Duration, pub scheduled_at: Option<i64>, pub draft_id: Option<String> }
pub async fn send_one(ctx: &AppCtx, id: i64, job: &JobCtx) -> Result<SendReport>;
pub struct SendReport {
    pub message_id: String,
    pub accepted: Vec<String>, pub rejected: Vec<(String, String)>,
    pub server_response: String, pub bytes_sent: u64, pub duration_ms: u64,
}
/// 发送后校验"已发送"副本：服务器自动归档了就不动，否则 APPEND 一份
pub async fn ensure_sent_copy(ctx: &AppCtx, account_id: &str, message_id: &str, spool: &Path) -> Result<SentDisposition>;
pub enum SentDisposition { AlreadyFiled { folder: String, uid: u32 }, Appended { folder: String, uid: u32 }, DeferredToQueue, Failed(String) };

// ── 发件箱 ───────────────────────────────────────────────────
pub async fn outbox_list(db: &Db, account_id: Option<&str>) -> Result<Vec<OutboxItem>>;
pub async fn outbox_cancel(ctx: &AppCtx, id: i64) -> Result<()>;         // 撤销/取消（未发送）
pub async fn outbox_retry_now(ctx: &AppCtx, id: i64) -> Result<()>;      // 跳过退避立刻重试
pub fn retry_delay(attempt: u32) -> Duration;                            // 30s→1m→5m→15m→1h→6h

// ── 草稿 ─────────────────────────────────────────────────────
pub async fn draft_save(db: &Db, d: &Draft) -> Result<()>;               // 去抖后调用（前端 2 s）
pub async fn draft_get(db: &Db, id: &str) -> Result<Option<Draft>>;
pub async fn draft_list(db: &Db, account_id: &str) -> Result<Vec<DraftSummary>>;
pub async fn draft_delete(db: &Db, id: &str) -> Result<()>;
/// 镜像到服务器 Drafts（APPEND + 删除上一个由我们创建的副本）；失败不阻塞写信
pub async fn draft_mirror(ctx: &AppCtx, draft_id: &str) -> Result<DraftMirrorReport>;

// ── 回复/转发/引用 ───────────────────────────────────────────
pub struct Quoted { pub text: String, pub html: Option<String> }
pub async fn quote_for_reply(db: &Db, key: &MsgKey, style: QuoteStyle) -> Result<Quoted>;
pub async fn reply_targets(db: &Db, key: &MsgKey, all: bool) -> Result<(Vec<Recipient>, Vec<Recipient>)>;  // (to, cc)
pub async fn forward_payload(ctx: &AppCtx, keys: &[MsgKey]) -> Result<ForwardPayload>;   // 原文 + 原附件（含内嵌图降级规则）

// ── 退信（M2）────────────────────────────────────────────────
pub fn parse_dsn(raw: &[u8]) -> Result<Option<DeliveryStatus>>;
pub struct DeliveryStatus { pub final_recipient: Option<String>, pub action: String, pub status: String, pub diagnostic: Option<String>, pub original_message_id: Option<String> }
```

**SMTP 细节要求**（写进实现约束，含既有教训 + A1-F45 的实测边界）
| 事项 | 要求 |
|---|---|
| 认证 | `AUTH XOAUTH2`（OAuth 账号）或 `AUTH PLAIN/LOGIN`；**绝不在日志里打印 SASL 串**（lettre 支持 XOAUTH2 ✅） |
| 组信 | 用 **`mail-builder` 1.0.0** 自己组 MIME 并流式写 spool（**不用 `MessageBuilder::body()`**；它不写 Content-Type，也不会让我们控制 CTE 与头顺序）；正文用 `singlepart` 等价形式（`text/plain` 与 `text/html` 各一个 part） |
| 发送 | **`AsyncTransport::send_raw(&Envelope, &spool_bytes)`**（字节级原样；只做点填充，**不做 LF→CRLF 重写**⇒ spool 必须 CRLF 正确） |
| 线程头 | `References` **一次性传入整条链**（库的 `references()` 是覆盖不是追加） |
| 8-bit / UTF-8 | 非 ASCII 正文用 `Content-Transfer-Encoding: 8bit` + `BODY=8BITMIME`（lettre 自动加 ✅）；**但服务器不支持 8BITMIME 时 lettre 会直接报客户端错误**（源码确认）⇒ 必须按**能力快照**决定 CTE：没有 8BITMIME 就用 base64/QP（`mail-builder` 一行的事），并给一条失败文案（红线 5） |
| smtputf8 | 地址含非 ASCII：按能力快照预检，不支持则**明确阻止**并给可行动提示 |
| **SIZE 预检** | **做不了**（第二轮评审核对源码：lettre 既不发送 `SIZE=` 也读不到服务器宣告的 `SIZE`，见 A1-F45 的更正）⇒ 预检只用**本地服务商表 + 硬上限**，不要写 `SIZE=` 断言 |
| **APPEND 与 SMTP 是两个上限** | 第二轮评审：Gmail 的 `APPENDLIMIT≈34 MiB` 与它的 SMTP 上限**不是同一个数**，O365 的 45 MB 也不是 `MaxSendSize` ⇒ 服务商表要**两个字段**（`smtp_max_message_bytes` / `append_limit_bytes`），预检用前者、归档副本用后者；文案里的数字取自**生效的那个** |
| **DSN** | **lettre 不支持**（A1-F45）⇒ **不请求投递回执**；退信只能靠**解析收到的退信**（MAIL-14 的 `parse_dsn` 仍然做，因为退信是普通邮件） |
| **PIPELINING / BDAT** | 都不支持（A1-F45）⇒ 批量发送是"一封一个往返"，据此设定发件箱吞吐预期（§8） |
| Bcc | 只进 `RCPT TO`，**绝不写入头部** |
| 头部注入 | subject/display name/address 一律过滤 CR/LF（防头注入），折行按 RFC 2047 |
| APPEND 拿不到 UID | **UIDPLUS 的 `APPENDUID` 被 async-imap 丢弃**（A1-F43）⇒ APPEND 之后必须用 `UID SEARCH HEADER Message-ID "<id>"` 找回 UID；找不到就把 `server_uid` 记为未知，下次同步时对账（草稿镜像与"已发送"副本都适用） |
| 信封发件人 | 默认用账号登录地址；身份若声明 `envelope_from_allowed` 才用身份地址 |


## 5. 状态机与主流程

### 5.1 写信 → 发送
```
openComposer(mode)
  ├ new        → 空正文 + 默认签名 + 默认身份
  ├ reply/all  → quote_for_reply + reply_targets + In-Reply-To/References
  └ forward    → forward_payload（附件转发，内嵌图降级为附件）
        │
        ├─ 本地草稿（2 s 去抖）→ user.db.drafts（脏标记）
        ├─ 每 30 s 或失焦 → draft_mirror 到服务器 Drafts（best-effort）
        └─ 点"发送" → validate()
                ├ Blocker → 就地阻止，指出改哪里（例如超限/无身份/SMTP 未开）
                ├ Warning → 可继续，但需用户确认（例如域名疑似拼错、收件人 120 个）
                └ OK → estimate → build_to_spool → enqueue(undo_window)
                                     │
                       ┌─────────────┴──────────────┐
                  用户点"撤销"                 窗口结束
                       │                            │
                outbox.cancel + 回写信界面      outbox_worker 取件 → send_one()
                                                    ├ 成功 → ensure_sent_copy → 删草稿 → 通知"已发送"
                                                    ├ 可重试失败 → 退避重排（保留 spool）
                                                    └ 永久失败 → failed + 原因 + "重试/另存草稿/复制正文"
```

### 5.2 撤销发送（**语义要诚实**）
- 撤销窗口内邮件**尚未**建立 SMTP 连接；UI 文案是"**正在发送…（N 秒内可撤销）**"，**不说"已发送"**。
- 撤销 = 取消队列项 + 保留草稿 + 删除 spool（或保留以便重发）。
- 窗口可配 `0-30 s`（默认 5）；设 0 表示立即发送（不做假动作）。
- 撤销按钮在窗口结束后消失；此时若发送失败，走正常的失败路径（不是"撤销"）。

### 5.3 发件箱 worker
```
**构建点只有一处（第二轮评审统一）**：`build_to_spool` 在**入队时**执行一次，字节冻结进 spool，
`outbox.spool_path` 记住它；发送时**只读 spool**，不再重新组信。
（只有 spool 丢失/损坏时才重建，且重建路径必须重新校验附件 sha256 —— 于是
 `AttachmentChanged`/`AttachmentMissing` 只在"重建"这一条路上可能出现。）
每 5 s tick（**并且 enqueue / 撤销窗口到期 / 网络恢复时立即唤醒一次**，否则 500 ms 的入队延迟预算达不到）：
  due = outbox where state='queued' and scheduled_at <= now and undo_until <= now and next_attempt_at <= now
        -- 三列都是 not null default 0（03 §3），**不要用 `is null or` 兜**：`null <= now` 恒为 NULL，
        -- 会让"从没重试过"的邮件永远取不到（第二轮评审）
  for item in due (每账号串行，遵守 04 的连接预算):
      state = 'sending'
      → 读 spool → SMTP 发送 → 解析结果
      ├ 2xx 且全部 RCPT 被接受 → sent + ensure_sent_copy
      ├ 部分 RCPT 被拒 → 仍算 sent（记录被拒地址，提示用户）
      ├ 4xx/超时/断线 → 退避重试（保留 spool，attempts+1）
      ├ 5xx（认证/大小/策略）→ failed + 分类原因（不重试，除 421/450 类）
      └ 需要用户决定（附件变了/服务器说太大）→ **`needs_attention`** 状态 + 三条文案
         （"发新版本 / 取消 / 改用链接"），UI 在发件箱里可见（原设计的 `queued|sending|sent|failed|cancelled`
          表达不了这种情况）
  播报：outbox:changed 事件（前端显示"发件箱(2)"）
```

### 5.3.1 崩溃恢复：先查、再重发（第二轮评审修正）
```
启动时对每个 state='sending' 的行：
  1. 用**冻结的 Message-ID** 在已发送文件夹里 `UID SEARCH HEADER Message-ID`（复用 §5.4 的原语）
     或查 sent_index / outbox.sent_message_id
  2. 找到 → 直接判 sent（**不重发**）；找不到 → 才回到 queued
  3. 带大附件的行默认**先暂停**（设置项"崩溃恢复时暂停发件箱"默认对 > 5 MB 的邮件生效），
     由用户确认后再发
为什么必须这么做：原设计说"重复会被 Message-ID 去重吞掉"——那只在 **Gmail 的存储**上成立，
第二次 SMTP 提交对收件人来说就是**又收到一封**；而且暴露窗口不是 2 秒，
是 TLS + DATA 全程（25 MB 附件在慢速上行要几分钟），Windows 更新/手动结束进程正好会撞上。
```

### 5.4 已发送副本（verify-or-append）
```
SMTP 接受后（message_id 已知）：
  0. **先写 `sent_index` 行**（放 `cache.db`，与 D4/03 §3.1 一致 —— 原设计这里写成 user.db 是笔误），
     状态 `pending_file`；**再**做归档判断。这样即使进程在下面任何一步崩掉，
     重启后也知道"这封还没归档"，而不是重复 APPEND。
  1. 若 provider.auto_files_sent == true（Gmail/QQ/163 等）→ **有界轮询**：
     在 Sent（或 Gmail 的 \Sent）里按 Message-ID 搜索，**最多 3 次 / 累计 ≤ 30 s**
        ├ 找到 → AlreadyFiled（写回 sent_index: filed + uid）
        └ 超时/没找到 → 走 2
  2. APPEND spool 到 Sent 文件夹（带 `\Seen` 标志）
        ├ 成功 → **APPEND 不返回新 UID**（`APPENDUID` 被丢弃，A1-F43）⇒ 立刻
        │        `UID SEARCH HEADER Message-ID "<id>"` 找回；找不回就记 `uid=unknown`，
        │        并在下一次同步后按 Message-ID 对账（**不重传**，否则"已发送"会出现重复）
        └ 失败：**区分永久与临时** ——
                · 大小/权限类（`NO Maximum size…`/权限拒绝）→ `SkippedTooLarge`/`SkippedNoPermission`
                  （文案："服务器不接受这么大的副本，已保存在本机"），**不再无限重试**
                · 离线/超时 → 入 pending_ops，稍后重试；UI 显示"已发送（本地副本待归档）"
  3. `sent_index` 是可重建的派生数据（cache.db）；**需要跨重启保留的"待归档"意图由
     `pending_ops`（user.db）承载** —— 两者分工不要混
```
> 为什么这么绕：服务器是否会替我们归档，各家不一致且会变；**发出去的信在"已发送"里找不到**
> 是用户最不能接受的失败之一。但**重复副本**同样不能接受 —— 而"服务器会按 Message-ID 去重"
> 只在 Gmail 成立（Dovecot/Exchange/163/QQ 会老实收下第二份，第二轮评审）。

### 5.5 附件生命周期
```
添加附件（文件选择/拖放/粘贴图片）
  ├ 读元数据（大小/mtime/sha256 增量计算）
  ├ estimate_encoded_size 预检 → 超 provider 上限（A1-F3）→ Blocker + 建议（拆包/云盘/压缩）
  └ 记入草稿（路径 + sha256 + size）
发送时
  ├ 读附件字节 → mail-builder 组 MIME → 写 spool。**这一段不是流式的**：`attachment_part` 一次
  `std::fs::read` 读整个文件（`crates/mail-core/src/smtp.rs:33`），组完再落盘；实测峰值 = 已编码邮件的
  **3.000 倍**（24 MB 附件那封 98.5 MB，`reports/performance.md` P10）。256 KB 分块只发生在**算 SHA-256**
  那一步（`crates/mail-core/src/attach.rs:31`），别把它当成发送路径的内存保证。
  ├ 发送前校验 sha256：变了 → AttachmentChanged（让用户选：发新版本 / 取消）
  └ spool 在"投递成功且服务器副本归档完成"后删除；失败保留（重试要用）。
    **没有"超 7 天自动清理"这一回事**（2026-10-20 核对代码：`Spool` 只有 open/path_for/write/read/remove，
    没有任何按年龄扫描；删它的三个时机是归档完成、用户取消、删掉发件队列那一行）。
    一句 honest 的话：没发出去的信会以明文 `.eml` 一直躺在配置目录里，直到用户自己把它处理掉。
```

## 6. 失败模式与降级

| 触发 | 用户看到 | 数据 | 处理 |
|---|---|---|---|
| 超服务商大小上限 | 添加附件/点发送时就被拦住，给出具体数字与建议 | 草稿保留 | `ValidationIssue{code: TooLarge}`；上限来自服务商库（A1-F3） |
| 收件人过多 | 提示上限与拆分建议 | 草稿保留 | `RecipientCount` |
| 地址含非 ASCII 且服务器不支持 SMTPUTF8 | `Blocker`：这个邮箱不支持非 ASCII 地址，请改用别名 | 草稿保留 | 能力探测 |
| SMTP AUTH 被租户关闭 | 发件箱标红 + "让 IT 打开 SMTP AUTH" | 队列保留 | 05 §6 分类；**不重试**（重试无意义） |
| 离线 | 队列显示"等待网络" | 队列保留 | 04 §5.5 恢复后自动 |
| 服务器 421/450（临时） | 无感（退避） | 保留 | 退避表 |
| 服务器 552（超配额） | "你的邮箱发信额度已满，请稍后再试" | 保留 | 不重试直到用户手动 |
| 发送成功但归档失败 | "已发送（本地副本待归档）" | 本地索引保留 | pending_ops 重试 |
| 草稿镜像失败 | 顶部小字"本地已保存（云端草稿待同步）" | 本地保留 | 静默重试 |
| 应用在 sending 中崩溃 | 重启后该邮件回到 queued（`sending` 视为过期） | spool 在 | 启动自检把 `sending` 重置为 `queued`（幂等：可能重复发送一封，见 §11 反驳 4） |
| 附件文件被删/改名 | `AttachmentMissing`，指出路径 | 草稿保留 | 让用户重新选择 |
| 模板变量缺失 | 以空串渲染 + 提示 | 无 | `render_template` 返回警告 |
| 引用内容里有脚本 | 引用会经过与阅读相同的净化 | 无 | 复用 08 的 `build_document` 净化规则 |

## 7. 迁移与兼容

- **保留既有 `smtp.rs` 的正确做法**（`singlepart`、References 整链、错误分类），只在其上扩展：
  XOAUTH2、**本地上限预检**（不是 `SIZE=`，见 A1-F45 的更正）、SMTPUTF8 检查、spool 流式、已发送归档。
- **v0.2 的 `OutgoingAttachment`（路径 + size）保持不变**，新增 `sha256`（`#[serde(default)]`），
  老草稿能读（缺 sha256 时发送前补算）。
- **草稿从"界面状态"迁移到 `user.db`**：v0.2 没有草稿功能，所以这是纯新增，无迁移负担。
- **`sent` 索引表**（新）：`sent_index(account_id, message_id, folder, uid, local_at)`，用于
  "发件箱 → 已发送"的可追溯性与重复归档判定。
- **服务商库新增字段**：`auto_files_sent`、`append_limit`、`max_recipients`（都可远程更新，05 §7）。

## 8. 性能预算（**按 A1-F45 的现实修正**）

| 指标 | 预算 |
|---|---|
| 打开写信面板 | ≤ 100 ms（含签名渲染） |
| 草稿保存（本地） | ≤ 20 ms |
| 草稿镜像（服务器） | ≤ 1 s（后台，不阻塞 UI） |
| 生成 MIME（spool 到磁盘） | 60 MB 邮件 ≤ 3 s；**峰值内存实测 3.000× 已编码邮件**（24 MB 附件那封 98.5 MB；这一段 2026-10-20 前写的是"≈2× 且建 spool 只占 1 MB"，那是设计意图不是实现，见下） |
| 单封大小上限（默认） | `min(服务商上限, 25 MB)`；超限在**添加附件时**就拦住 |
| 发送（含 TLS 握手） | ≤ 2 s + 传输时间 |
| 撤销窗口结束后入队延迟 | ≤ 500 ms（**靠 enqueue / 窗口到期 / 网络恢复时主动唤醒**，不是等 5 s tick —— 评审指出固定轮询达不到这个预算） |

**为什么实测是 3× 而不是"流式"**（2026-10-20 核对代码重写：这一段此前描述的是设计意图，不是发出去的那条路）：
lettre 0.11.23 的 `Body` 是 `Vec<u8>`，**没有文件/Reader 形式的 body**，也没有 `message_iter` 之外的低层流式入口
（`AsyncSmtpConnection::message_iter` 是公开的，但用它就得自己接管 MIME 组装、连接池与 TLS）。
于是今天真实占内存的是三步，每一步都握着完整一份：
1. **读附件**：`attachment_part` 一次 `std::fs::read` 读整个文件（`crates/mail-core/src/smtp.rs:33`）。
   **没有 256 KB 分块** —— 分块只属于算 SHA-256 那一步（`crates/mail-core/src/attach.rs:31`），
   它保护的是"哈希一个 100 MB 文件不吃掉 UI 的时间预算"，不是发送路径的内存上限。
2. **组 MIME**：`message.formatted()` 再产出一份完整编码字节（base64 之后 ≈ 1.33× 原文件）。
3. **发送**：读 spool → `AsyncTransport::send_raw(&Envelope, &bytes)` 又占一份。

实测 **98.5 MB = 已编码邮件的 3.000 倍**（24 MB 附件那封，`reports/performance.md`；断言放宽到 ≤3.1×），
所以 P10 是**踩线通过、没有余量**。要把它降下来只有一条真路：自己按 part 边编码边写盘
（绕开 `Body: Vec<u8>`），同时保住"`send_raw` 不改行尾"这条字节一致约束（见下）。
M3 若要更大的附件走的仍是同一条路，**届时**才能移除单封上限；**不要**在没有实测的情况下承诺"1 GB 附件"。
补充约束：`send_raw` **不做 LF→CRLF 重写**（只做点填充）⇒ spool 里必须是 CRLF 正确的字节；
而走 lettre 的 builder 路径会被重新编码（改 CTE、折行、补 Date/Message-ID），
**要字节一致就必须自己组 MIME + `send_raw`**（这也是我们选 `mail-builder` 的原因）。


## 9. 测试计划

| 层 | 用例 |
|---|---|
| 单元 | `validate()` 真值表（≥ 20 条：超限/收件人数/空主题/坏地址/域名疑似/SMTPUTF8/附件变化/重复收件人）；`estimate_encoded_size` 与真实编码结果误差 ≤ 5%；`render_template` 转义；`parse_dsn`（3 种真实退信样本） |
| MIME 组装 | 与 `mail-parser` 往返：生成的邮件被自己解析回来，`to/cc/subject/attachments/inline/references` 全部一致（**含非 ASCII 文件名与 GBK 正文**） |
| loopback | 假 SMTP 扩展：支持 XOAUTH2 / 只支持 PLAIN / 拒绝超大（552）/ 部分 RCPT 被拒（250+550）/ **超出本地上限**（lettre 不发送 SIZE，所以断言的是"本地预检拦住了"）；**spool 的字节数断言**（评审：spool **结尾不带 CRLF** —— lettre 自己会补 `\\r\\n.\\r\\n`，否则每封信多发一个空行，M3 做 PGP 签名时会变成签名不一致）；断言错误分类与重试策略 |
| 大附件 | 24 MB 附件：断言 spool 与源文件 sha256 一致、发送后收件方能还原；**峰值 RSS 增量 ≤ 3× 已编码邮件大小**（lettre 的现实：A1-F45 实测 25 MB 附件峰值约 90 MB ⇒ 取 3× 作为上限；原写 2.5× 与实测不符），并在超限时给出拦截而非硬撑 |
| 撤销 | 窗口内撤销 → 断言**零 SMTP 连接**（假服务器断言没有连接）；窗口结束 → 断言确实发出 |
| 离线/重试 | 断网入队 20 封 → 联网 → 全部发出、顺序合理、无重复（用假服务器记录 message-id 去重） |
| 崩溃 | `sending` 中 kill 进程 → 重启 → 断言回到 queued 并最终发出（允许重复，见反驳 4） |
| 已发送归档 | 三种服务商行为（自动归档/不归档/APPEND 失败）各一条用例 |
| 真机 | QQ/163/Gmail/Outlook 各发一封给自己（含 1 个中文名附件 + 内嵌图 + 引用），用网页端核对显示效果 |

## 10. 门禁新增项

1. **Bcc 不进头部**：单测断言生成的 MIME 里没有 `Bcc:`（非常容易回归）。
2. **不用 `MessageBuilder::body()`**：源码检查（防止有人"简化"回旧写法）。
3. **References 整链**：单测断言 `references` 长度 == 输入链长度（防止 `references()` 覆盖回归）。
4. **头注入**：单测把 `\r\n` 注入 subject/display name → 断言被过滤。
5. **撤销窗口语义**：单测 + 假服务器断言"撤销时不产生任何 SMTP 连接"。
6. **附件上限表存在**：`providers.rs` 里 Gmail/O365/Exchange 三条必须有 `append_limit`（防止默认值悄悄失效）。

## 11. 逆向反驳（红队）

**反驳 1：受限富文本会被用户嫌弃（"连个表格都不能插"）。**
- 证据：我们的目标用户是办公邮件，不是营销排版；而且**Outlook 的 HTML 引擎**对现代 CSS 的支持很差，
  富文本做得越花，收件人那边越容易崩。
- 让步：M1 支持粗体/斜体/下划线/删除线/列表/链接/引用/内嵌图；**M2 再评估"简单表格"**（只生成
  `table` + 内联边框，不用 CSS 类）。给出"粘贴时保留格式"的白名单（仅上述标签），其余一律纯文本。

**反驳 2：撤销发送本质是"延迟发送"，会让用户误以为已经发出去了。**
- 缓解：这种"假动作"在 Gmail 里已被广泛接受，但**文案必须诚实**：窗口内显示"正在发送…可撤销"，
  绝不显示"已发送"；窗口可设为 0（真·立即发送）并作为高级选项。
- 让步：窗口期间**不建立连接**（这是"撤销真的有效"的证明），也因此在窗口内该邮件不计入"今日发信量"。

**反驳 3：`verify-or-append` 会给服务器增加一次搜索 + 可能的 APPEND，成本值吗？**
- 证据：Gmail 自动归档、部分企业邮箱不归档、还有的归档到"已发送"以外的文件夹。**不做这一步，
  用户会遇到"我发出去的信在已发送里没有"** —— 这是最难解释的一类投诉。
- 缓解：①只在发送后**一次**、带 3 s 超时；②结果缓存进 `sent_index`，同一 message-id 不重复检查；
  ③服务商库里 `auto_files_sent=true` 的（我们的默认表）仍然校验一次，但找到就立刻返回；
  ④完全离线时直接进队列，不阻塞。
- 让步：提供设置项"发送后总是保存一份副本到已发送（可能重复）"，给极端使用者。

**反驳 4：崩溃恢复可能把同一封邮件发两次。**
- 证据：`sending` 状态在崩溃时无法判定服务器是否已接受（除非用 SMTP 事务日志，成本高）。
- 分析：重复发送一封邮件的代价 < 丢失一封邮件的代价；且真实概率极低（发送窗口通常 < 2 s）。
- 缓解：①spool 保留 + `Message-ID` 固定 ⇒ 若服务器按 Message-ID 去重（Gmail 会），重复会被吞掉；
  ②UI 在"重复发送风险"发生时给出提示（"这封可能在崩溃前已发出，请确认后再重发"）；
  ③设置项"崩溃恢复时默认暂停发件箱"（给谨慎用户）。

**反驳 5：草稿本地为真相 + 服务器镜像，会不会出现"两台机器各写一份，互相覆盖服务器草稿"？**
- 承认：M1 的语义是"**本地为真相**，服务器副本是单向镜像（我们只删自己创建的那份）"。
  如果用户在另一台设备/网页端改了服务器草稿，我们**不自动合并**，只在检测到服务器草稿 UID 变化时
  提示"服务器上有另一份草稿，是否导入？"（M2 做导入，M1 只提示）。
- 让步：不做双向草稿同步（那是"草稿 CRDT"级别的工程，收益低）。
- **技术补充（A1-F43）**：APPEND 之后**拿不到新 UID**（`APPENDUID` 被丢弃），
  所以"我们创建的那份"要靠 `UID SEARCH HEADER Message-ID` 找回来；找不到就不删旧副本
  （宁可多留一份草稿，也不要误删用户刚写的东西）。

**反驳 6：`send_raw` 要求我们自己组 MIME，等于放弃了 lettre builder 的便利，值吗？**
- 证据：只有自己组 MIME 才能拿到两样东西：**字节可控**（不被重新编码成 base64/QP、不被改折行）
  与**显式 CTE**（8bit vs base64）；同时 `mail-builder`（1.0.0）已经把这层做好了（A1-F45）。
- 让步：小邮件（全 ASCII、无附件）仍可以走 builder 路径；但**两条路径会产生不同的字节**，
  这会让"往返一致"的测试与用户预期出现分叉 ⇒ **统一走 `mail-builder` + `send_raw` 一条路**，
  用测试覆盖"生成的字节与预期完全一致"。

**反驳 6：附件 sha256 预检会拖慢"添加大附件"的体验（要读一遍 60 MB）。**
- 缓解：①增量计算 + 大小/mtime 命中缓存（同一文件重复添加不重算）；②在后台线程算，UI 立即显示条目并标注"校验中"；
  ③取消添加时中止计算。
- 让步：> 200 MB 的文件直接拒绝（服务商不可能接受），不再计算哈希。

**反驳 7：把"已发送索引"放在 `user.db` 而不是 `cache.db`，会让它不能被随意删除——值吗？**
- 证据：`user.db` 的定位是"用户数据不可删"（03 §11 反驳 5）。已发送索引是**可重建**的（从服务器 Sent 重新同步）。
- 改设计：**`sent_index` 放 `cache.db`**（可删可重建），只有在 `pending_ops` 里待归档的那条放 `user.db`
  （因为它代表"还没落到服务器的动作"）。这条修正也写回 03 的表归属。

**反驳 8：定时发送（M2）用本地时钟，机器休眠/关机就错过时间。**
- 缓解：①启动时补偿（已过期的排期立即发送）；②休眠唤醒后立即检查；③UI 明说"需要应用在运行"。
- 让步：不承诺"关机也能定时发"（那需要服务器端排期，违反红线 6）。

## 12. 未决问题

| # | 问题 | 影响 | 建议 |
|---|---|---|---|
| C1 | 撤销窗口默认值与范围 | 体验 vs 诚实 | 默认 5 s，可 0-30 s |
| C2 | 富文本是否含表格 | 兼容性 | M2 评估，M1 不含 |
| C3 | 是否默认"总是保存副本到已发送" | 重复 vs 缺失 | 默认自动判定（verify-or-append），设置里可强制 |
| C4 | 崩溃恢复时发件箱是否默认暂停 | 重复发送 | 默认继续（重复概率极低），高级项可改成暂停 |
| C5 | 内嵌图片的降级阈值 | 兼容性 | 单张 > 5 MB 或总计 > 20 MB 时降级为普通附件并提示 |
| C6 | 是否支持"发给自己一份密送"的惯例 | 习惯 | 提供设置项（默认关） |
| C7 | 身份（别名）发送的服务商白名单 | 现实（多数服务商不允许别名作为 envelope sender） | 默认只用登录地址作 envelope，From 头用身份；白名单数据驱动 |

## 13. 验收清单

- [ ] 写一封信（含中文附件名 + 内嵌图 + 引用），发给 QQ/163/Gmail/Outlook 各一次，网页端显示正常。
- [ ] 24 MB 附件的 MIME 生成：spool 与源文件 sha256 一致；**峰值内存 ≤ 3× 已编码邮件大小**（lettre 的现实边界，A1-F45 实测 ~90 MB/25 MB 附件）；
      超过单封上限时在**添加附件阶段**被拦住（不是等服务器拒绝）。
- [ ] 超限附件在**添加时**就被拦住并给出具体数字（不是等服务器拒绝）。
- [ ] 断网写草稿 → 崩溃 → 重启：草稿一字不差；断网发送 → 联网 → 全部送达且无重复。
- [ ] 撤销窗口内点撤销：假服务器断言**零 SMTP 连接**；窗口结束后确实发出。
- [ ] 发送后在"已发送"里能找到（三种服务商行为都要覆盖），且没有重复副本。
- [ ] `Bcc` 不出现在头部；`References` 链完整（回复在收件人客户端里成会话）。
- [ ] `check.ps1` 六项新增检查通过。

---

## 14. 发件箱与「已发送」：一个列表 + 一份状态（**v5，2026-10-31 第三轮反向找茬**）

> **v5 = v4 + 一次"先假设它错"的复核**。v4 修对了 v3 的七处，但留下一条方向性浪费（D0 无条件做，
> 而它对存量邮件永远不生效）与六处硬伤。逐条见 §14.0。v1/v2/v3/v4 的原文结论保留在 §14.13
> （这个仓库的规矩：改了什么、为什么改必须可查）。**仍然只有文档，代码一行没动。**

### 14.0 v5 改了什么（v4 → v5）

| # | v4 的说法 | 为什么站不住 | v5 改成 |
|---|---|---|---|
| 1 | D0（cache v11：列 + 模型 + 部分索引 + 四个填充点）**无条件做** | 主填充点是"同步头部解析"，而增量同步只问游标之上（`imap.rs:618`）；**843 封存量永远不会再走一遍**，计划又明确拒绝回填 ⇒ 升级后历史已发送行**全都没有** `message_id`，D2 的标签只对"升级后新发的信"有效——而那时同步自己就会写行。更要紧的是：`sent_index` 本来就有 `(folder, uid)`，列表行也自带 `(folder, uid)` | **D0 与 D3 捆绑成"分支 C 的条件块"**（§14.4）。A/B 下按 `(account_id, folder, uid, uidvalidity)` 关联；cache v11 只加**两列**（`sent_index.uidvalidity` + `messages.to_addrs`），**不建索引、不动前端状态契约**（§14.4 的 D0a/D0b） |
| 2 | ⓿ 兜底搬移："`spool` 空 ⇒ 没有副本 ⇒ `folder=''`" | 真库 `outbox` id **11/12/14** 全是 `state=sent` 且 `spool_path is null`，而 `sent_index` 分别指向 `Sent Messages` **226/209/207** —— **"spool 空"正是这个 bug 的常态形状，不是"没有副本"的证据**。⓿ 会把已知位置降级成"未知"。另外老行 `sent_message_id` 为空时会撞 `NOT NULL`，⓿ 每轮报错、归档轮次整个跑不动 | 迁移与 ⓿ 合并成**同一个 `settle_sent_row()`**（§14.6.3）：先查 `sent_index` → 再合成键 → 最后才是 `folder=''` |
| 3 | D7 只结 `state='sent'` 的案 | 真库 `id=13` 是 `failed` + `spool_path is null`：`frozen_bytes` 读不到字节 ⇒ `mark_outbox_failed(needs_attention)`（`worker.rs:150-156`）⇒ 点"重试"永远失败，而 D5 的 `n` 把它算作待发 ⇒ **「待发 (n)」永远回不到 0**，用户没有任何动作能让它消失 | D7 扩成**三种终局**（发出去了但没副本 / 发出去了但字节没了 / **没发出去且字节没了**），第 ③ 种给一个「删除这条记录」的出口 |
| 4 | `deleted_marks` 进 cache v11 | 违背 D2 自己的理由（"状态是事实，不能放可删可重建的缓存"）：用户点「清空缓存」⇒ 墓碑消失 ⇒ 下一轮同步**把已删除的信复活** | 搬进 **`user.db` v5**（与 `pending_ops` 同库）；cache 里不再有它 |
| 5 | D5 只写了"待发/失败不进时间流" | 「全部」档里 outbox 行放哪儿**没有定义**：`cancelled` 说"在全部里显示为已撤销"，`queued` 说"不进时间流" ⇒ 同一张列表两种待遇。写"置顶"就是 v1 的行位移，写"按日期插"就是 v1 的两源合并（分页立刻坏） | **两个档 = 两个数据源，永不混排**：`全部` 只读 `cache.messages`，`待发` 只读 `outbox`。可机器断言 |
| 6 | D-1 的分支表只有三格 | 「`SEARCH ALL` 有 226、而 `UID FETCH 210:*` 也没有」这一格没有分支，而两者共用同一套 RFC 区间语义，一起坏完全可能 | 补 **A′**：`UID FETCH 1:* (UID)` 全量取 UID + 本地过滤（真正不依赖区间解释） |
| 7 | `sent_records(folder, uid)` 与 `sent_index(folder, uid)` 重复，且 §14.8 里没有消费者 | 计划刚用"两个对象、各自权威"否掉 v3 合并 `draft_mirrors`，转头又存了一份位置；而"清缓存后列表为空是正确行为"让这张表的持久性无处可用 | 保留，但**写明它是 `sent_index` 的耐久镜像、且唯一消费者是 §14.8 的那句提示**（否则就删列） |
| 8 | ④ 最后删 spool（顺序是对的） | ③ 与 ④ 之间崩溃 ⇒ 文件失去引用，**永远留着**（单封上限 25 MB） | 启动清扫：spool 目录里无 `outbox` 行引用、且 mtime > 24 h 的文件删掉；给断言 |
| 9 | "`msg.to()` 已经在手" | `summarise()` 只调了 `msg.message_id()`（`imap.rs:1160`），**没有调 `to()`**。字段确实零成本可加，但不是"已在手" | 措辞改成"零成本可加"，并写明 `to_addrs` **只含 To**（Cc 在详情里） |

### 14.1 用户看到的问题（真机数据，v5 复核后未改）

| 事实 | 证据 |
|---|---|
| 发件箱那一行是 `sent` | `user.db` outbox id=14，`state=sent`，10-31 17:34 |
| 副本确实归档了 | `cache.db` `sent_index`：folder `Sent Messages`、**UID 226**、写入于 17:35:27 |
| 「已发送」列表里没有它 | `cache.db` 该文件夹只有 UID **202–209**，`folders.last_uid=209` |
| 之后十几次同步都说没有新信 | 日志 17:35–17:59 反复 `同步 Sent Messages：新增 0 封，缓存共 8 封` |

**发件箱说"已发送"，已发送说"没这封信"——两个界面各自都对，用户看到的却是矛盾。**

### 14.2 两条根因（v5：第二条仍是"症状确证、原因未定位"）

1. **归档成功不落列表。** `file_sent_copies`（`worker.rs:435`）在 `Ok(Some(copy))` 分支里只写 `sent_index`（一条指针，`worker.rs:469`），**不写 `messages` 行**，而「已发送」列表只读 `messages`。这一条是确定的，且独立于下面那条。
2. **增量同步取不回刚 APPEND 的那封。** 症状确证：`fetch_headers_since` 发 `UID SEARCH 210:*`（`imap.rs:618` + `since_range`，`imap.rs:84-86`），在 QQ 的 Sent 上返回空。
   **原因已定位（2026-11-01，§14.3.1）：分支 A —— 该服务器的 `SEARCH <n>:*` 区间不可信，同一区间的
   `UID FETCH` 能取到 226。** 修法已按 A 栏落地（`fetch_headers_since` 改用 `UID FETCH … (UID)` 枚举），
   真机复验一次同步新增 17 封、游标落在 226。下面那段"三种可能"保留为当时的推理记录。

### 14.3 D-1：诊断（v4 新增；v5 补第四格 A′）

一次探针：同一连接、同一文件夹，按顺序。

| 步骤 | 命令 | 看什么 |
|---|---|---|
| 1 | `SELECT "Sent Messages"` | 返回的 `UIDVALIDITY` / `UIDNEXT` |
| 2 | `UID SEARCH 210:*` | 有没有 226 |
| 3 | `UID SEARCH ALL` | 有没有 226（只取 UID，不取头部，代价与 2 同级） |
| 4 | `UID FETCH 210:* (UID)` | 有没有 226 |
| 5 | 本地 `folders.last_uid` / `uidvalidity` | 与 1 是否一致 |

三种结论（**v5 补第四格 A′**）：

| 分支 | 现象 | 结论 | 下一步 |
|---|---|---|---|
| **A** | 3、4 有 226，2 没有 | 该服务器的 `SEARCH <n>:*` 区间不可信，`FETCH <n>:*` 可信 | **修根因**：`fetch_headers_since` 改用 `UID FETCH <after+1>:* (UID)` 取 UID 列表（不取头部，代价与 SEARCH 同级），再 `retain(> after)` + `keep_oldest_page` + 按页取头部。一页语义不变，**不再依赖 SEARCH 的区间解释**。D3 不做 |
| **A′** | 3 有 226，**2 和 4 都没有** | 两条命令共用同一套 RFC 3501 区间语义，**区间解释整体不可信**（v5 新增：v4 没有这一格，遇到就只能现场发明） | **修根因**：`UID FETCH 1:* (UID)` 全量取 UID（只有编号，万级邮件几十 KB），本地 `retain(> after)` 后再按页取头部。这条**完全不依赖区间解释**，是 A 的兜底形态。D3 不做 |
| **B** | 2、3、4 都有 226，缓存里没有 | 服务器没问题，是我们这边丢的 | 查三处：`advance_cursor` 是否把游标推过头（`sync.rs:252`）、`absorb` 的 `uidvalidity` 分支是否把行作废（`sync.rs:232-244`）、`retain(> after_uid)` 的入参是否用了**已推进后**的游标。修那一处，D3 不做 |
| **C** | 2、3、4 都有、游标也对，行还是不出现 | 链路另有断点（例如该文件夹不在同步集合里） | 才做 D3（§14.5 标"条件性"），并连带做 D0（§14.4） |

**可证伪**：诊断结论必须落成一条断言或一行日志（形如 `同步 Sent：SEARCH 空 / FETCH 有 226 ⇒ 分支 A`），写进本节。不许"顺手做了 D3 就当诊断过了"。

#### 14.3.1 结论：**分支 A**（2026-11-01 实测，真账号 QQ `Sent Messages`）

探针 `crates/mail-core/tests/sent_uid_probe.rs`（只读：一次 `SELECT` + 三条 `UID` 命令），输出原文：

```
账号        724921217@qq.com
文件夹      Sent Messages（线路名）
SELECT      UIDVALIDITY=1579573363 UIDNEXT=227
本地游标    last_uid=209 uidvalidity=1579573363
目标 UID    226
SEARCH 210:*  → Some(false)
SEARCH ALL             → Some(true)
FETCH  210:*  → Some(true)
同步 Sent Messages：SEARCH 空 / FETCH 有 226 ⇒ 分支 A（SEARCH 的区间不可信）
```

**同一连接、同一文件夹、同一区间，`SEARCH` 说没有、`FETCH` 说有。** 所以 §14.2 的第 2 条根因定位为
**"该服务器的 `SEARCH <n>:*` 区间不可信"**，不再是"症状确证、原因未定位"；**D3 与 D0c 都不做**（§14.5、§14.4）。

修法按 A 栏执行，已在同一提交里完成：`fetch_headers_since` 的"取 UID 列表"那一步从
`UID SEARCH <after+1>:*` 换成 `UID FETCH <after+1>:* (UID)`，`retain(uid > after_uid)` 与
`keep_oldest_page` 一条未动（区间无序 ⇒ 那个过滤器仍然是必需的）。

**修复后的真机复验**（同一账号，同一文件夹，跑一次 `SyncDriver`）：

```
=== 同步 Sent Messages ===
新增 17 封 · 缓存共 25 封 · last_uid 226 · uidvalidity 变了？false
```

改之前这台机器上同一操作反复得到"新增 0 封"（§14.1 的日志）。现在游标落在 outbox 声称的那个 UID 上，
**「发件箱说已发送、已发送里没有它」这个矛盾在数据层已经消失**。界面那一半（D5 的分段控件、状态标签）
仍未做，见 §14.10 的第 3 步。

**回归**：`tests/loopback.rs::incremental_sync_enumerates_the_range_instead_of_searching_it` 断言的是
**协议形状**（区间必须用 `FETCH … (UID)` 枚举、不得用 `SEARCH`），因为 loopback 夹具忠实于 RFC 3501 的区间语义，
**复现不出 QQ 的答案** —— 这正是 §14.11 把"真机出口"与"A 栏离线验收"分开的理由。


**成本对照**：A/B 的修法各约半天；D3 是 D0 + 三步顺序 + 边界表五行 + 一条新写入者。先花半天买一个"可能不需要后面全部东西"的答案。

### 14.4 D0（**v5：拆成"无条件的三件小事"与"分支 C 的一件大事"**）

v4 把 D0 当成一个无条件整块，理由是"**D2 的键、边界的判等、清缓存后重新挂接**"三件事共用 `message_id` 这一列。v5 复核后：前两件**只在分支 C 下才存在**（A/B 下由同步写出行，判等由同步的 `upsert_summaries` 负责），第三件一个已有列就能做。

**cache v11 = 两列 + 一个索引，只有索引是条件性的：**

| 块 | 内容 | 什么时候做 | 成本 |
|---|---|---|---|
| **D0a** | `sent_index.uidvalidity` 一列 + `record_sent_copy` 带上它 | **总是**（A/B/A′/C 都要） | 一条 `add_column`，无索引、无模型、无前端契约 |
| **D0b** | `messages.to_addrs` 一列 + `MessageSummary.to_addrs` + `summarise()` 填充（`msg.to()` 零成本可加）+ `upsert_summaries`/`list_summaries` 带上 | **总是**（否则「已发送」每一行都显示你自己） | 一列 + 一个字段，无索引 |
| **D0c** | `messages.message_id` + `MessageSummary.message_id` + `messages_by_message_id` 部分索引 + 四个填充点 | **仅分支 C** | 10 万行索引（P14 已 171.5%）+ 前端契约 + 四个填充点 |

**A/B/A′ 分支：只做 D0a + D0b。** 状态按 `(account_id, folder, uid)` 关联：

| 需要什么 | 现在有什么 |
|---|---|
| 「这行有没有副本」 | `sent_index` 的 `(account_id, message_id, folder, uid)`（`cache.rs:641`），而列表行自带 `(folder, uid)` ⇒ 直接按 `(account_id, folder, uid)` join |
| 「副本位置未知」 | `sent_index.uid is null` |
| 「UIDVALIDITY 变了，这个 uid 不再是同一封信」 | **cache v11 的唯一改动**：`sent_index` 加 `uidvalidity` 一列，`record_sent_copy` 写入时带上；不匹配 ⇒ 显示"位置未知"，不误认 |

**为什么这就够**：`message_id` 唯一多给的能力是"UID 全变了还能认"，那是低频事件；它的成本是
10 万行索引（P14 已 171.5%）+ 前端契约 + 四个填充点，而收益只覆盖"升级后新发的信"（§14.0 第 1 条）
——那批信本来就由同步直接写出带 `message_id` 的行，等于为一个已经成立的事实付两次钱。

**分支 C：做 D0c，并与 D3 一起做**，内容回到 v4 的四项：

1. **列**：`messages.message_id text`，允许 NULL。
2. **模型**：`MessageSummary` 加 `message_id: Option<String>` 与 `to_addrs: Option<String>`（`#[serde(default)]`）。
3. **索引**：`messages_by_message_id on messages(account_id, message_id) where message_id is not null`（分索引：10 万封里绝大多数是 NULL）。
4. **填充点四个**（同步头部解析 / 归档时 / 详情到达时 / 迁移回填）。

**顺序**：先 `add_column_if_missing`，再建索引。`cache.rs:658` 那段注释就是为这个坑写的——v2 文件走到这里时列还不存在，同批建索引会 `no such column`，并把整个升级一起带走。

**`to_addrs` 的边界（v5 补）**：`summarise()` 现在只调 `msg.message_id()`（`imap.rs:1160`），`msg.to()` **没有被调用过**——不是"已经在手"，是"零成本可加"（`parsed` 已在手，不再发请求）。它**只含 To**：抄送/密送收件人不在列表行上显示，要看去详情。群发时这一行会显示不全，这是接受的代价，不为此再加两列。**它是 D0b，无条件做**——「已发送」每一行都显示你自己，那是第 103 轮就存在的可见缺陷，不该被 D-1 的结论绑住。

**不做**：仍然不从服务器重取全部头部去补这一列（10 万封 = 10 万次头部解析，换来的只是一个暂时不用的键）。

**可证伪**：A/B/A′ 下，`select count(*) from sent_index where uid is not null and uidvalidity = <当前文件夹的>` 必须等于「已发送」里带"副本已归档"标记的行数（同一份夹具，两处数一致）；**变异测试**：把 join 从 `(folder, uid)` 改成只看 `folder` ⇒ 该断言必须变红。分支 C 下沿用 v4 的断言。

### 14.5 七个决定（v5：D0 条件化、D2 换键、D5 定位置规则、D7 扩三种终局）

| # | 决定 | 理由 |
|---|---|---|
| **D1** | **一个列表**：侧栏只有「已发送」；数据**只来自 `cache.db` 的 `messages`**，分页/排序/窗口化不变 | 两库联合查询会让 `message_page(offset)` 的分页重复或漏行。**清空缓存会让这个列表变空——这是它的正确行为**（§14.8） |
| **D2** | **状态存在 `user.db` 的 `sent_records`**，渲染时按当前页的 `(folder, uid, uidvalidity)` 批量查一次（`IN (...)`，≤50） | v4 用 `message_id` 做键，但那一列对存量邮件永远是 NULL（§14.0 第 1 条），等于按一个空集合查。`(folder, uid)` 两边都有、且现在就准；`uidvalidity` 是防"文件夹重建后同一个 UID 指向另一封信"。**`message_id` 只在分支 C 下才成为键**（那时 `messages` 行是我们自己写的） |
| **D3**（**条件性**） | **归档成功即写 `messages` 行**（摘要，UID 来自 `copy.uid`）——**仅当 D-1 判定为分支 C 时执行** | 分支 A/B/A′ 下同步自己就会写出这行，D3 只剩"省一次同步等待"的收益，却要付边界表五行的成本。**收益/成本倒挂，所以不做** |
| **D4** | **删除 = 直接删**（本地行 + 服务器副本），**必须二次确认**；不提供"只删本地" | 用户原话："他们的概念就是删除这个账户下的这封邮件"。实现约束见 §14.9 第 7-9 行 |
| **D5** | **两个档 = 两个数据源，永不混排**：`全部` 只读 `cache.messages`，`待发` 只读 `outbox`；分段控件常驻列表头 | v4 的"cancelled 在全部里显示 / queued 不进时间流"是自相矛盾的：同一张列表两种待遇，而位置规则一个字没写——置顶就是 v1 的行位移，按日期插就是 v1 的两源合并。分成两档之后，"行从哪来"和"排第几"都不再需要回答 |
| **D6** | **归档成功后删除 `outbox` 行**，状态转存 `sent_records` | 否则那张表无限增长，而它每次都要全量读。删除的前提：有副本时"`messages` 行（仅分支 C）+ `sent_index` + `sent_records` 三样都写成功"；没有副本时按 D7 结案，同样要删行 |
| **D7**（**v5 扩成三种终局**） | 三条路都必须**结案**：① `sent` + 无副本（`Ok(None)` / `SkippedTooLarge`）② **`sent` + spool 字节丢了** ③ **`failed` / `needs_attention` + spool 为空**（从未发出，没有副本可谈）。①② 结案 = 搬进 `sent_records`（`folder=''`）+ 删 outbox 行 + 删 spool；**③ 只删行、不写 `sent_records`**（它从来没发出去，不属于「已发送」；写进去等于凭空造一条发送记录） | v4 只结第 ① 种。真库 `id=13` 就是第 ③ 种：`frozen_bytes` 读不到字节 ⇒ 永远 `needs_attention`，而 D5 的 `n` 把它算作待发 ⇒ **「待发 (n)」永远回不到 0**，用户没有任何动作能让它消失。第 ③ 种结案时必须让用户看见（见细则），否则等于把"失败"悄悄删掉 |

**D5 的细则**（v5：加了位置规则与第 ③ 种终局的出口）：

- **位置规则（v5 新增，一句话就能机器断言）**：`全部` 档的行**只**来自 `cache.messages`；`待发` 档的行**只**来自 `outbox`。**任何一档都不混排两个来源**——这就是 v1 被否掉的那件事，v4 用"cancelled 在全部里显示"把它从后门放了回来。
- **行形态：两套渲染分支，同一套定高。** cache 行走现有 `buildChildRow`；outbox 行走现有 `outbox-row` 分支（`app.js:1946`），但**去掉 `height: auto`**——它现在和普通行同处一个窗口化列表，`--msg-height` 必须成立。
- **「这封信怎么了」不再行内展开，改成弹窗**（v4 改，保留）：点行右侧的「记录」按钮，复用 `openModal`（`app.js:5550`），内容从 `sent_records` / `outbox` 取（状态/尝试次数/最后错误/Message-ID）。理由：展开面板放不进 40 px 的定高行；做成弹窗，行几何就只剩一种，窗口化不用为它开特例。
- **点开 outbox 行本身**：不发网络请求，只选中；「重试」只在 `failed` / `needs_attention` 上出现，放在弹窗里。
- **第 ③ 种终局的出口（v5 新增）**：`failed` / `needs_attention` 且 spool 为空的弹窗里，「重试」旁边给**「删除这条记录」**（二次确认，说明"这封信没有发出去，也没有留下内容"）。没有这个出口，`n` 永远不会归零，而用户看到的是一个按了没反应的按钮。
- **空态**：`全部` 档为空走现有空态；`待发` 档为空给一行「没有待发的信」——**不是整块隐藏**，分段控件自己就是入口。
- **推送**：§14.7 的四步走完（② 写 `sent_records` 之后即可）发一个 `sent-state-changed` 事件；前端若正停在「已发送」，重取当前页状态与计数。没有它，用户会看到「待发 (1)」而列表里那行没有标签。
- `n` = `outbox` 里 `queued`/`sending`/`failed`/`needs_attention` 的行数，**不含 `sent`**（`sent` 且 spool 还在 = 信发出去了、副本没归档，那不是"待发"，它也不该永远消失——见 §14.9 最后一行）。
- `cancelled` 行：留在 `outbox`，**在 `待发` 档里**显示为"已撤销"（不在 `全部` 档），**7 天后由启动清理删除**。**但删之前必须先确认草稿还在**（`outbox.draft_id` 指向的 `drafts` 行存在）——spool 里是冻结的那封信，草稿才是用户的字；草稿不在了就把这行标成 `needs_attention` 交给用户，**不许自动删**（`12` §16.1 原则 6：自动清理只能施加在可以重建的东西上）。

### 14.6 数据模型（v5）

#### 14.6.1 `user.db` 新表 `sent_records`

```
account_id   TEXT    NOT NULL
message_id   TEXT    NOT NULL   -- 冻结的 Message-ID；合成键见 14.6.2
folder       TEXT    NOT NULL DEFAULT ''   -- 副本所在文件夹；空串 = 服务器上没有副本（D7）
uid          INTEGER                        -- 服务器给的编号，可空
uidvalidity  INTEGER                        -- v5 新增：没有它，(folder, uid) 在文件夹重建后会指向别的信
state        TEXT    NOT NULL               -- sent / failed / needs_attention / cancelled
attempts     INTEGER NOT NULL DEFAULT 0
last_error   TEXT
synthetic    INTEGER NOT NULL DEFAULT 0     -- 1 = 这封没有真 Message-ID，是迁移补的
created_at   INTEGER NOT NULL
updated_at   INTEGER NOT NULL
PRIMARY KEY (account_id, message_id)
```

`message_id` **必须 NOT NULL**：SQLite 的主键对 NULL 不去重（除非 `NOT NULL`），一列可空的键会让同一封失败的信插出多行。

**`message_id` 在野外不唯一（v5 补）**：转发、再发、"另存为副本"都会复用原信的 `Message-ID`。所以写入一律用
`on conflict do update`，且**只填当前为空的字段**；遇到"两边都非空且不同"的 `folder/uid` ⇒ 保留先到的、记一行日志，
**不覆盖**。理由与 §14.9 第 2 行 ③ 同一条：拿一个不能判等的事实去盖章，比漏记一条更糟。

**`uidvalidity` 为什么进 `sent_records`（v5 新增）**：D2 改成按 `(folder, uid)` 关联之后，没有它就等于按一个会
被回收的地址去认人——文件夹删了重建，同一个 UID 可能是另一封信。它与 `sent_index.uidvalidity`（cache v11 那一列）
必须同源写入，否则两边会各判各的。

`folder=''` 同时表达"没有副本"与"没归档"，所以**不再需要 `no_copy` 列**——D7 结案时它就是空串，和"服务器没有 Sent 文件夹"是同一种事实。

`outbox` 从此只保留**未完成**的行（`queued`/`sending`/`failed`/`needs_attention`/`cancelled`）。

#### 14.6.2 迁移（`user.db` v4 → v5，必须升版本号）

```sql
create table if not exists sent_records (...);            -- 同上（含 uidvalidity）

-- v5 新增：墓碑搬到这里，不再放 cache
create table if not exists deleted_marks (
    account_id  TEXT    NOT NULL,
    folder      TEXT    NOT NULL,
    uid         INTEGER NOT NULL,
    uidvalidity INTEGER NOT NULL,
    message_id  TEXT,
    at          INTEGER NOT NULL,
    PRIMARY KEY (account_id, folder, uid)
);

-- 1) 已经结案的 sent 行：状态搬家，连同已记下的副本位置
insert or replace into sent_records
    (account_id, message_id, folder, uid, uidvalidity, state, attempts, last_error, synthetic, created_at, updated_at)
select o.account_id, o.sent_message_id,
       coalesce(s.folder, ''), s.uid, coalesce(s.uidvalidity, 0), o.state, o.attempts, o.last_error, 0, o.created_at, o.sent_at
  from outbox o
  left join sent_index s on s.account_id = o.account_id and s.message_id = o.sent_message_id
 where o.state = 'sent' and o.spool_path is null and o.sent_message_id is not null;
```

- **`sent_index` 在另一个库（`cache.db`）**，迁移不能 join。上面那条 join 是**概念图**：实际实现是先从 `cache.db` 读出 `(account_id, message_id) → (folder, uid, uidvalidity)` 一张小表，再在 `user.db` 里做匹配。两张表都小（发件数），一次全读可行。
- **只搬 `spool_path is null` 的行**。spool 非空的行下一轮会被归档轮次处理，**处理完必须由 D7 结案**；迁移不该替它结案，也不该把它删掉（删了就是丢信）。
- **没有真 Message-ID 的存量行**（M1a-8 之前入队的）：给合成键 `local+<outbox.id>@easyemail.invalid`，`synthetic = 1`。这些行**不参与**"查看副本"，但**必须活着**——用户的历史不该因为我们加了一列就消失。
- 迁移完删掉被搬走的那些 outbox 行（`state='sent' and spool_path is null`），**不是**删所有 `state='sent'`。
- **可证伪**：迁移前数 `select count(*) from outbox where state='sent' and spool_path is null`，迁移后 `sent_records` 的行数必须等于它。**少一行就是丢信。**
- **可证伪（v5 新增，专打 §14.0 第 2 条）**：迁移后 `select count(*) from sent_records where folder = ''` 必须 **等于**"`outbox` 里 spool 为空且 `sent_index` 里查不到对应 message_id 的行数"。真库上这个数应当是 **0**（11/12/14 三条在 `sent_index` 里都有 226/209/207）。**大于 0 就是又把已知位置写成了未知。**

#### 14.6.3 结案函数 `settle_sent_row()`（v5：迁移与 ⓿ 共用一份实现）

v4 里迁移会 `join sent_index`、会给 synthetic 键，而 ⓿ 两样都没有——两套逻辑必然漂移，而真库证明"spool 空"恰恰是**这个 bug 的常态形状**（11/12/14 三条都有副本位置）。v5 把结案收成**一个函数**，迁移与每轮兜底都调它：

```
settle_sent_row(outbox_row):
  1. message_id = row.sent_message_id
     └ 为空 ⇒ 合成键 local+<row.id>@easyemail.invalid，synthetic = 1
  2. 查 cache.sent_index(account_id, message_id)
     ├ 命中且 uid 非空 ⇒ folder = s.folder, uid = s.uid, uidvalidity = s.uidvalidity
     ├ 命中但 uid 为空 ⇒ folder = s.folder, uid = null（"位置未知"，但**知道它在哪个文件夹**）
     └ 未命中 ⇒ folder = ''（真的是没有副本）
  3. insert or replace into sent_records(...)          -- 主键幂等，冲突规则见 14.6.1
  4. delete from outbox where id = row.id              -- 幂等
  5. spool 文件若存在则删
```

- 第 2 步**必须查**，不许因为"spool 为空"就跳过去：spool 为空有三种来源（归档成功后被删 / 字节丢失 / 从来没建过），只有查过 `sent_index` 才分得清。
- 第 3 步用 `insert or replace` 而不是 `insert`：⓿ 每轮都跑，崩溃后重入是常态。
- **幂等断言**：连跑两次 `settle_sent_row`，`sent_records` 行数不变、`outbox` 行数不变、`folder` 不被第二次调用改坏（尤其不能把第一次查到的 `Sent Messages` 改回 `''`）。

**⓿ 兜底搬移**：每轮归档**开始前**跑一次，对 `state='sent' and spool_path is null` 的残留行调 `settle_sent_row()`。幂等，覆盖"迁移之后才产生"的孤儿（无 Sent 文件夹的账号、超大被跳过的账号，每发一封就产生一条）。

**可证伪**：跑完一轮归档后 `select count(*) from outbox where state='sent'` 必须为 0；且上一条"`folder=''` 的数 == 真没有副本的数"仍然成立。

**spool 孤儿清扫（v5 新增，§14.0 第 8 条）**：启动时扫 spool 目录，删掉**没有任何 `outbox` 行引用、且 mtime > 24 h** 的文件。④ 放在最后是对的（它是"还没归档"的证据），代价就是 ③ 与 ④ 之间崩溃会留下一个失去引用的文件——单封上限 25 MB，不扫就是永久泄漏。**可证伪**：造一个无引用的 spool 文件（mtime 改到 25 h 前）⇒ 启动后被删；mtime 是 1 h 前的 ⇒ **不**被删（避免删掉正在写的那一封）。

### 14.7 四步顺序（v5：⓿ 走 `settle_sent_row`；跨库没有事务，所以顺序就是正确性）

```
⓿ user.db：把 state='sent' 且 spool 为空的行结案（settle_sent_row，幂等）  —— 每轮先跑
① cache.db：写 messages 行（仅分支 C 的 D3）+ 写 sent_index（已有，v5 起带 uidvalidity）  —— upsert，可重入
② user.db ：insert or replace sent_records                         —— 主键幂等
③ user.db ：delete from outbox where id = ?                        —— 幂等
④ 磁盘    ：删 spool 文件                                          —— 最后，它是"还没归档"的证据
```

任一步失败 ⇒ **停在那里，保留 outbox 行与 spool**，下一轮从 ① 重来（全部幂等）。④ 在最后：现在的代码是"先删 spool 再清标记"（`worker.rs:476-477`），中途崩掉就再也找不到那封信了。

**没有副本的三条路**（D7）：不写 ①，直接 ②（`folder=''`）→ ③ → ④。

### 14.8 清空缓存之后会发生什么（v5：给 `sent_records` 一个消费者）

- 列表变空（行在 cache 里，被清了）；`sent_records` 里的状态成为**孤儿**——不显示，也不删（它是事实）。
- 重新同步后，行按 `(folder, uid, uidvalidity)` 重新挂上（A/B 分支），或按 `message_id`（分支 C）。**这就是那一列 / 那个索引存在的全部理由**，除此之外没有别的消费者。
- 面板加一句：「清空缓存后，「已发送」需要重新同步才能看到历史。」
- **v5 新增一句，`sent_records` 的唯一可见消费者**：「已发送」为空、但 `sent_records` 非空时，空态下面补一行小字
  「本机记录显示有 N 封已发送（服务器副本未同步）」。这是**提示**，不是列表来源——它不进分页、不参与排序、不参与计数（`n` 仍然只数 outbox）。
- 为什么非要有这一句：没有它，`sent_records` 的持久性（清缓存后还在）在任何界面上都没有用处，那张表就退化成
  `sent_index` 的一份跨库拷贝——而这正是 §14.0 第 7 条批评的东西。有了这一句，它才有"唯一持有某个事实"的身份。
- **不做**：不把 `sent_records` 当列表来源。那会退回 v1 的两源合并，分页立刻坏。

### 14.9 边界（v5：第 2 行降为分支 C；新增 `failed` 终局与"副本待归档"）

| 情况 | 行为 |
|---|---|
| `copy.uid` 为空（服务器没给编号） | **仅分支 C**：**不写 `messages` 行**（主键要 uid），等同步；`sent_records.uid` 留空，行上标「副本位置未知」。A/B 分支没有「写行」这个动作，这一格自然不存在 |
| **该 `(account, folder, uid)` 已有一行**（**仅分支 C**） | 用 `message_id` 判等，三种结局：① **该行 `message_id` 相同** ⇒ 只补空字段（`coalesce`），不动 `seen`；② **该行 `message_id` 不同** ⇒ **不写**，记日志，`sent_records.uid` 留空并标「副本位置未知」；③ **该行 `message_id` 为 NULL**（老行，无法判等）⇒ **什么都不写**，记日志 + 标「副本位置未知」。理由：③ 走 ① 就等于把我们的 id 盖在一行身份未知的记录上，那比漏写一行更糟。**A/B 分支没有这一行**——行由同步写，判等由 `upsert_summaries` 负责 |
| **没有副本**（无 Sent 文件夹 / `SkippedTooLarge` / spool 字节丢了） | **D7 结案**：`sent_records(folder='', uid=null)` + 删 outbox 行 + 删 spool；行上标「仅本地记录」，不假装已归档。**但结案前必须先查 `sent_index`**（§14.6.3 第 2 步）：真库 11/12/14 三条 spool 为空、副本却在 226/209/207，按"spool 空就当没副本"处理就是把已知写成未知 |
| **`failed` / `needs_attention` 且 spool 为空**（v5 新增） | **D7 第 ③ 种终局**：这封信**从来没有发出去**，所以没有副本可谈，重试也永远不可能成功（`frozen_bytes` 读不到字节，`worker.rs:150-156`）。弹窗里给「删除这条记录」，结案后 `n` 才可能归零。不结案就是「待发 (n)」永远不归零、而按钮按了没反应 |
| 同步先到了，行已存在（**仅分支 C**） | 同第 2 行 ①；`thread_key`/`subject_key` 只在为空时才补。A/B 分支下这一格就是同步的普通 upsert，没有第二个作者 |
| `seen` 谁说了算 | **服务器**。D3（分支 C）写 1 是猜测，同步改回 0 是**纠正**，不是 bug。测试要把这条写死，防止以后有人"修"它 |
| 离线 | 本地记录可见；顶部横幅说明"下面是上次同步的内容" |
| **删除：服务器不可达** | 本地立即消失，进 `pending_ops`（`kind = delete`，已有，`lib.rs:3395-3407`），界面如实说"已从本机删除，服务器副本待联网后删除" |
| **删除：`uid` 为空** | 先按 Message-ID `SEARCH` 定位（`find_uid_by_message_id`，`imap.rs:1020`）；仍找不到 ⇒ 只删本地，**并在确认弹窗里事先说明这一点** |
| **删除：没有 UIDPLUS 的服务器** | `EXPUNGE` 是**文件夹级**语义（`imap.rs:983`），会连带清掉别人标的 `\Deleted`。所以：先 `FETCH (FLAGS)` 数该文件夹的 `\Deleted` 集合，**只有集合恰好等于目标**才 EXPUNGE；否则只标 `\Deleted`，如实告诉用户"已标记删除，服务器清理时生效"，**并把补 EXPUNGE 放进 `pending_ops` 重试**（v4 补：否则这些信永远挂在 `\Deleted` 上） |
| **删除后同步又看见它** | 加 `deleted_marks(account_id, folder, uid, uidvalidity, message_id, at)`，主键 `(account_id, folder, uid)`，**放 `user.db` v5**（v5 改：v4 放进 cache，而 cache 是用户点「清空缓存」就会清掉的那个库——墓碑一消失，下一轮同步就把删掉的信复活，这直接违背 D2 自己的理由）。同步 upsert 前先查，**命中必须同时匹配 `uidvalidity`**（UIDVALIDITY 一变，墓碑自动失效，比 30 天过期精确）；`pending_ops` 成功或放弃时删标记；启动时清掉超过 30 天的标记作兜底 |
| **删除时本地清理** | `cache.delete_message` 现在只动 `search/messages/bodies`（`cache.rs:1359-1385`），**不清 `sent_index`** ⇒「查看副本」留下指向空行的链接。要求：同一次写事务里删掉 `sent_index` 里对应 `(folder, uid)` 的行 |
| **归档成功但 `sent_records` 写失败** | 保留 outbox 行 + spool，下一轮重做（幂等）。**分支 C** 下这一封此时**同时**出现在列表（cache 行已写）与 outbox（`state=sent`、spool 在）——但它**不进「待发」计数**（D5），所以用户不会看到「已发送」和「待发」打架 |
| **发出去了、副本还没归档（`sent` 且 spool 在）**（v5 新增） | 它在 `待发` 档末尾单列一行「已发出，副本待归档 (m)」，**m 不并入 n**。理由：离线时归档轮次根本跑不了，这封信既不进 `待发`（D5 排除 `sent`）、也不进「已发送」（A/B 分支下还没有 cache 行），用户会以为信丢了。它是**事实**（发出去了），只是副本还没到位，必须看得见 |

### 14.10 顺序与依赖（v5：D0 进分支，cache v11 缩成两列）

```
-1. D-1 诊断（§14.3）                                     ✅ 2026-11-01：分支 A（§14.3.1）
 0. cache v11：`sent_index.uidvalidity` + `messages.to_addrs`（D0a/D0b）   ✅ 2026-11-01：两列都落地
 1. user.db v5：sent_records + deleted_marks + 存量行 + settle_sent_row   纯后端，可独立验收
 2a. 【A / A′ / B】修根因（改 `fetch_headers_since` 或游标那三处）        ✅ 2a-A 已做：改用 `UID FETCH … (UID)`
 2b. 【C】D0c（message_id + 部分索引 + 四填充点）+ D3 + D6                 **不做**（诊断结果是 A，不是 C）
 3. 列表合并 + 分段控件 + 状态批查（按 (folder, uid)）+ 事件 + spool 清扫   前端 + 一个新命令
 4. 侧栏 IA（`12` §16）                                    纯展示层，可与 -1..1 并行（见 `12` §16.5）
```

**当前进度（2026-11-01）**：-1、0、2a 完成，下一步是 1（`user.db` v5）→ 3。
2b 因诊断结论为 A 而**永久关闭**，除非哪天在别的服务商上又看到"服务器说有、我们这边没有"。

**第 0 步落地时的两处判断**：

- **D0b 的列早就存在**（`messages.to_addrs` 是 v1 就有的列，正文到达时写 JSON 数组）。所以 D0b 不是
  "加一列"，而是**让头部同步也写它**并把值送到列表行上（`MessageSummary.to_addrs: Vec<String>`，
  `#[serde(default)]`）。写入用 `coalesce(excluded.to_addrs, messages.to_addrs)`，与正文路径同一规则：
  一封没有 `To:` 的信不得抹掉读正文时已经知道的东西。
- **D0a 的 `0 = 未知`是这条列的全部意义**。迁移不给已有行猜当前 `UIDVALIDITY`；`record_sent_copy` 收到 0
  时用 `coalesce(nullif(excluded.uidvalidity, 0), sent_index.uidvalidity)` 保留旧值。
  真库上核过：三条存量行（207/209/226）迁移后 `uidvalidity` 全是 0，正是"未知"而不是"当前代"。

**为什么不能倒过来**：界面依赖的"这一行有没有副本"，在 0 之前只能靠 `outbox` 猜；靠猜就得两源合并，那就是 v1 的老路。

**为什么 D-1 必须在第 2 步之前**：它决定第 2 步走 2a 还是 2b。跳过它，就是默认"必须做 D3"，而那正是 v3 的错误前提；顺着 v4 走，还会连带默认"必须做 D0"——两份钱都白花。

**为什么 D0a/D0b 排在 2a 之前**：A/B 分支的整条展示路径都建立在 `(folder, uid)` 上，没有 `uidvalidity` 就是在按一个会被回收的地址认人；没有 `to_addrs`，列表每一行都显示你自己。这两列是这条路上全部的 schema 改动，也是最便宜的两处。

### 14.11 验收（v5：A 栏按分支拆开，"升级后第一眼"提为该块出口）

**A. 离线可证伪**（`cargo test` / loopback / node 门禁；当前环境 `cargo test --workspace` 有 LNK1104 限制，按第 102 轮记录的办法跑 `-p mail-core --lib`）

- [x] **cache v11**（2026-11-01 已做）：老库打开后 `sent_index` 有 `uidvalidity` 列（已有行的值是 0 = 未知，**不是**被猜成当前值）、`messages` 有 `to_addrs` 列；新同步一封信后该行的 `to_addrs` 非空（loopback：`a_synced_row_carries_its_recipients_without_fetching_the_body`）。
- [ ] 迁移：`count(outbox where state='sent' and spool_path is null)` == 迁移后 `sent_records` 行数（含 synthetic）；**少一行即失败**。
- [ ] 迁移：`count(sent_records where folder='')` == "spool 空且 `sent_index` 查不到"的行数。**真库上应当是 0**（11/12/14 三条有 226/209/207）。大于 0 就是把已知写成了未知。
- [ ] 结案：造一条无 Sent 文件夹的账号，发一封 ⇒ 跑一轮归档 ⇒ `count(outbox where state='sent')` == 0，且 `sent_records` 里有一条 `folder=''`。
- [ ] 结案（v5 新增）：造一条 `failed` + spool 为空的 outbox 行 ⇒ 走「删除这条记录」⇒ `count(outbox)` 减一，`n` 归零，**且不写 `sent_records`**（它从来没发出去，不是"已发送"）。
- [ ] 幂等：同一 Message-ID 归档两次，`messages` 仍只有一行、`sent_index` 仍只有一行；`settle_sent_row` 连跑两次，`folder` 不被第二次调用改回 `''`。
- [ ] **分支 C 专属**：`message_id` 非空行数 ≥ `sent_index` 行数；`sent_index` 每行都能在 `messages` 找到 `(folder, uid)`；判等三结局三条单测；`seen` 权威（D3 写 1 后跑一轮同步 ⇒ 本地变 0，**断言这是期望行为**）；`summarise()` 产出的摘要带 `message_id` 与 `to_addrs`（loopback 断言非空）。
- [ ] 墓碑：离线删除 ⇒ 跑一轮同步 ⇒ 行**没有**回来；`uidvalidity` 变了 ⇒ 墓碑失效、行可以回来；`pending_op` 成功后标记被删；**清空 cache 后墓碑仍在**（它在 `user.db`）。
- [ ] `sent_index` 清理：删除后 `sent_copy(account, message_id)` 返回 `None`。
- [ ] spool 孤儿：无引用且 mtime 25 h 前的文件被删；1 h 前的**不**被删。
- [ ] **变异测试**（去掉即必须变红）：去掉墓碑；去掉 `message_id` 判等（分支 C）；把 `coalesce` 改回赋值；把 ④ 移到 ② 之前；去掉 ⓿ 兜底搬移；**把 `settle_sent_row` 第 2 步（查 `sent_index`）删掉**；把 `deleted_marks` 放回 `cache.db`（清缓存后行复活 ⇒ 红）。

**B. 该块的出口条件（真机，一次，附截图）**

这一条**不是发版条件，是第 2 步的出口条件**——A 栏全在 loopback 上，而 loopback 忠实复现的是 RFC 的区间语义，**不是 QQ 的 SEARCH 行为**。A 栏能证明"我们写了行"，不能证明"用户看到的矛盾消失了"。

- [ ] 发信 → **不点开「已发送」** → 点开「已发送」 → 列表里有那一行，**且 Sent 里只有一封**。
- [ ] **升级后第一眼（v5 新增）**：真库那三条历史（`outbox` 11/12/14）在「已发送」里能点开「查看副本」，分别落到 226/209/207 —— **不是「副本位置未知」**。这一条专打 §14.0 第 2 条。
- [ ] 同一账号下，D-1 的探针日志与分支结论已写回 §14.3（形如 `同步 Sent：SEARCH 空 / FETCH 有 226 ⇒ 分支 A`）。
- [ ] 离线排队时「待发 (1)」可见；关掉横幅后它仍在；点行右侧「记录」能打开弹窗，且行高没变。
- [ ] **断网时发一封** ⇒ 「已发出，副本待归档 (1)」出现在 `待发` 档末尾，**且 `n` 不变**；恢复网络后它消失、出现在「已发送」。
- [ ] 删除：二次确认 → 本地立刻消失 → 服务器 `\Deleted`/EXPUNGE 走通。
- [ ] 三种主题下分段控件与设置弹窗各截一张。

### 14.12 与第 103 轮代码的返工清单（v5 补一条）

| 现在的代码 | 合并后 | 处置 |
|---|---|---|
| `state.expandedOutbox`（`app.js:213`）+ 行内「这封信的记录」 | 行内展开放不进 40 px 的定高行（`--msg-height`），而 outbox 行现在与普通行同处一个窗口化列表 | **改成弹窗**：`openModal` + 数据从 `sent_records` 取。用户价值（能点开看"这封信怎么了"）保留，容器换掉 |
| `.msg.outbox-row { height: auto; overflow: visible }`（第 103 轮刚加） | 行回到同一个窗口化列表，`--msg-height` 的定高假设重新生效 | **删掉这条覆盖**。`height: auto` 会让 `vlist.computeWindow` 的偏移全错 |
| `#btn-outbox` / `#outbox-count`（`app.js:535`、`1530`） | 侧栏没有「发件箱」 | 删元素与事件；计数迁到分段控件 |
| `buildChildRow` 只渲染发件人 | 「已发送」每一行都会显示你自己 | **加收件人分支**：`folder` 是 Sent 时显示 `to_addrs`（v5：来自 **D0b**，无条件做，不等 D-1） |
| 「已发送（服务器上的副本）」 | 仍然成立（它确实是服务器副本） | **保留**；消失的是"发件箱"这个词 |
| 门禁「样式表里的 id 必须存在」 | 上面删掉的 id | 必须同步从 CSS 删，否则这一步会红 |

### 14.13 v1 / v2 / v3 / v4 被推翻的记录（诚实记录，不删）

| 版本 | 说法 | 为什么错 |
|---|---|---|
| v1 | 两个来源**合并**成一个视图 | 行数变化让 `message_page(offset)` 的分页重复/漏行；两库无法在 SQL 层 join |
| v1 | `origin` 三态显示在行上 | 它本身就是"本地/云端"标签，和用户要的东西相反 |
| v1 | 待发/失败**置顶** | 翻页时行位移、看到重复 |
| v2 | D2 按 `Message-ID` 批查 | 那一列不存在（§14.0 第 1 条） |
| v2 | "不覆盖 `seen`" | 守错一侧，同步会覆盖 |
| v2 | 归档即删 `outbox` 行 | 存量 `sent` 行会消失 |
| v2 | "一个事务里"三步 | 跨两个库文件，没有事务 |
| v2 | 待发/失败"过滤视图" | 没有入口的视图等于不存在 |
| v3 | 根因 2"不在本次范围"，用 D3 兜住 | 用户看到的矛盾就是根因 2；打开文件夹本来就会同步（`lib.rs:752`），修好它就够了。用架构绕过 bug = 让丢信级缺陷继续存在（§14.0 第 1 条） |
| v3 | 填充点只有三个 | 最便宜的那个（同步头部解析）没列进去，`msg.message_id()` 已经在手（`imap.rs:1160`） |
| v3 | ③ `message_id` 为 NULL "走 ① 不覆盖" | ① 是 `coalesce` 补空字段，等于给身份未知的行盖章（§14.0 第 3 条） |
| v3 | D6 只在归档成功时删行 | "没有副本"也是结论，不结案就产生谁也看不见的孤儿行（§14.0 第 4 条） |
| v4 | D0 无条件做（列 + 模型 + 部分索引 + 四填充点） | 主填充点走增量同步，而增量只问游标之上 ⇒ 存量 843 封永远不走这条路，`message_id` 恒为 NULL；D2 的键因此是一个空集合（§14.0 第 1 条） |
| v4 | ⓿ 兜底搬移"`spool` 空 ⇒ `folder=''`" | 真库 11/12/14 三条 spool 空而副本在 226/209/207 —— "spool 空"是这个 bug 的常态形状，不是没有副本的证据（§14.0 第 2 条） |
| v4 | D7 只结 `state='sent'` 的案 | `failed` + spool 空的行（真库 id=13）永远 `needs_attention`，`n` 永远回不到 0，重试按钮按了没反应（§14.0 第 3 条） |
| v4 | `deleted_marks` 进 cache v11 | 用户点「清空缓存」⇒ 墓碑消失 ⇒ 已删的信复活；直接违背 D2 自己写的"状态不能放可删可重建的缓存"（§14.0 第 4 条） |
| v4 | `cancelled` 行"在「全部」里显示" | 同一张列表两种来源、两种待遇，位置规则一个字没写；写置顶是 v1 的行位移，写按日期插是 v1 的两源合并（§14.0 第 5 条） |
| v4 | D-1 只有三个分支 | 漏「`SEARCH ALL` 有、`UID FETCH 210:*` 没有」这一格，而两条命令共用同一套 RFC 区间语义，一起坏完全可能（§14.0 第 6 条） |
| v4 | `sent_records(folder, uid)` 与 `sent_index` 重复 | 计划刚用"两个对象、各自权威"否掉 v3 合并 `draft_mirrors`，转头又存了一份位置；而 §14.8 里它没有消费者，持久性无处可用（§14.0 第 7 条） |
| v4 | "`msg.to()` 已经在手" | `summarise()` 只调了 `msg.message_id()`（`imap.rs:1160`），`to()` 从未被调用；字段是零成本可加，不是已在手（§14.0 第 9 条） |
