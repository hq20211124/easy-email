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
  ├ build_to_spool 逐块读文件（256 KB 块）→ base64 → 写 spool（**建 spool 这一步的峰值 ≈ 1 MB**；真正的峰值在发送那一步，见 §8）
  ├ 发送前校验 sha256：变了 → AttachmentChanged（让用户选：发新版本 / 取消）
  └ spool 在"sent"后删除；failed 保留（重试要用），超 7 天自动清理
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
| 生成 MIME（spool 到磁盘） | 60 MB 邮件 ≤ 3 s；**峰值内存 ≈ 2× 邮件大小**（见下） |
| 单封大小上限（默认） | `min(服务商上限, 25 MB)`；超限在**添加附件时**就拦住 |
| 发送（含 TLS 握手） | ≤ 2 s + 传输时间 |
| 撤销窗口结束后入队延迟 | ≤ 500 ms（**靠 enqueue / 窗口到期 / 网络恢复时主动唤醒**，不是等 5 s tick —— 评审指出固定轮询达不到这个预算） |

**为什么会是 2× 而不是"流式"**（诚实记录，A1-F45 实测结论）：
lettre 0.11.23 的 `Body` 是 `Vec<u8>`，**没有文件/Reader 形式的 body**，也没有 `message_iter` 之外的低层流式入口
（`AsyncSmtpConnection::message_iter` 是公开的，但用它就得自己接管 MIME 组装、连接池与 TLS）。
所以 M1 的现实上限是：
1. **我们自己的 spool**：用 `mail-builder`（1.0.0，显式 CTE 与自定义头，A1-F45）把 MIME **流式写到磁盘**，
   峰值内存只与单块（256 KB）有关；
2. **发送**：读 spool → `AsyncTransport::send_raw(&Envelope, &bytes)` —— 这一步会再占一份内存（= 邮件大小），
   于是"峰值 ≈ 2× 邮件大小"；25 MB 上限下约 50 MB，可接受；
3. **M3 若要更大的附件**：自己实现 SMTP DATA 的流式写入（`message_iter` 或自研 SMTP 客户端），
   届时移除单封上限。**不要**在没有实测的情况下承诺"1 GB 附件"。
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
