# 05 账号与身份（多账号、OAuth2、令牌、自动发现）

> **目标**：让"填个邮箱就能用"在 2026 年的现实下依然成立 —— 包括 Gmail 与微软**必须** OAuth2、
> 国内邮箱**只有授权码**、令牌不能丢也不能明文落盘。
>
> 覆盖 ID：ACC-01…16、SEC-01/14（凭据与 TLS）、PROTO-01（服务商库）。
> 事实依据：[`A1-research-findings.md`](A1-research-findings.md) §五（OAuth2 与国内服务商），**本文所有外部结论都可回溯到那里**。

---

## 1. 目标与非目标

**目标**
1. **一条认证轴**：OAuth2 / 授权码 / 应用专用密码 / 密码，四种都由服务商库声明，UI 不猜。
2. **多账号**：不限数量、可暂停、可排序、可识别色、统一收件箱（ACC-01/02/13/15）。
3. **令牌安全**：绝不进 `accounts.json`、绝不进日志；DPAPI 用户级加密文件；**支持轮换与撤销**。
4. **自动发现**：内置库 → 域名 autoconfig → ISPDB → DNS SRV → Autodiscover → 手动，逐层降级。
5. **失败可解释**：把"管理员没开 SMTP AUTH"、"没生成授权码"、"Google 7 天令牌过期"、
   "企业策略只给 30 天"这四类**最常见且最让人困惑**的失败，变成一句能照做的话。
6. **可绕开**：企业/高级用户可用自己的 OAuth 应用 ID（BYO client_id），这是本设计能落地的关键逃生门。

**非目标**
- 不做扫码登录（微信/QQ 扫码不是开放协议，拿不到）。
- 不做 SSO/Kerberos/NTLM（PROTO-09，M3 评估）。
- 不托管任何 OAuth 中转服务（红线 6；见 §11 反驳 1）。

## 2. 用户可感知行为

| 场景 | 用户看到 |
|---|---|
| 加 Gmail | 输入地址 → 识别出 Gmail → "使用 Google 登录" → 打开**系统浏览器** → 授权 → 回来即开始同步。首次会看到 Google 的"未验证应用"提示，我们提前用一张图告诉他点哪里 |
| 加 163 | 识别出网易 → 三步图文："去网页端开启 IMAP → 生成授权码 → 粘贴到这里"，输入框只收 16 位并自动去空格 |
| 加公司 M365 | 授权后若管理员未同意 → "需要管理员批准：把这段说明发给 IT"（可一键复制） |
| 加 iCloud | 识别出"iCloud 只能用应用专用密码"→ 直接给 Apple 的生成步骤链接 |
| 授权过期 | 横幅"Gmail 登录已过期，请重新登录"（**不是**"同步失败"这种含糊话），一键重登 |
| 想换默认发件账号 | 设置 → 账号：拖拽排序、点色块换色、开关"参与统一收件箱" |
| 企业环境要自己的应用 ID | 设置 → 高级 → "使用我自己的 OAuth 应用"：填 client_id / 租户 ID（BYO） |
| 暂停账号 | 右键账号 → "暂停同步（保留本地邮件）"；仍可读本地，不发任何请求 |
| 移除账号 | 明确区分"只移除本机账号"与"同时删除本地邮件缓存"，默认前者 |

## 3. 数据结构

### 3.1 服务商库扩展（`providers.rs` → 数据驱动）

```rust
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "snake_case")]
pub enum AuthMethod {
    OAuth2,          // 授权码流程 + PKCE（Gmail、M365、Yahoo、Fastmail…）
    AuthCode,        // 服务商生成的"授权码/客户端专用密码"（网易/QQ/腾讯企业邮/阿里邮箱）
    AppPassword,     // 应用专用密码（iCloud 等）
    Password,        // 普通密码（自建、Bridge）
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct OAuthSpec {
    pub issuer: String,                  // 用于显示与去重
    pub auth_endpoint: String,
    pub token_endpoint: String,
    pub scopes: Vec<String>,
    pub redirect: RedirectKind,          // Loopback（默认）| CustomScheme
    pub scheme: Option<String>,          // CustomScheme 时用（如 easyemail）
    pub pkce: bool,                      // 恒 true（本设计要求）
    pub extra_auth_params: Vec<(String, String)>,
    pub tenant: Option<String>,          // 微软："common" / 租户 ID
    /// 该 scope 是否属于 Google 的"受限 scope"（会触发 CASA 与验证），用于给用户/开发者提示
    pub restricted_scope: bool,
    /// **第二轮评审补**：Google 的"桌面应用"凭据会带一个 client_secret，且它的令牌端点
    /// 文档里包含该参数；原设计里没有任何字段能承载它 ⇒ BYO-Gmail 可能一上来就是坏的。
    /// 实现第一步：**先用真实端点探测"只给 client_id + PKCE 能否换到令牌"**，
    /// 结论写进 A1（带日期）。若必须要 secret，就按下面的字段带上：
    /// 内置 ID 的 secret 构建期注入（`option_env!`），BYO 的存在 vault/凭据管理器。
    pub client_secret: Option<SecretRef>,
}
pub enum SecretRef { BuildTime, Vault }

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct CredentialHelp {
    pub headline: String,                // "网易邮箱需要授权码，不是登录密码"
    pub steps: Vec<String>,              // 图文步骤（前端配图）
    pub url: Option<String>,
    pub field_hint: Option<String>,      // "16 位授权码"
    pub validate: Option<CredentialPattern>, // 长度/字符集的前端预校验
    pub warnings: Vec<String>,           // "生成后只显示一次，请先粘贴再关页面"
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct Enablement {
    pub protocol_off_by_default: bool,   // QQ/163/阿里：IMAP/SMTP 默认关闭
    pub admin_may_block: bool,           // 企业邮可能由管理员控制
    pub server_side_window: Option<String>, // 网易企业邮默认只给 30 天（A1-F2）
}

pub struct Provider {
    // …既有字段（id/display_name/region/imap/smtp/quirks/verified/…）
    pub auth: AuthMethod,
    pub oauth: Option<OAuthSpec>,
    pub help: CredentialHelp,
    pub enablement: Enablement,
    pub append_limit: Option<u64>,        // A1-F3：Gmail 34 MiB / O365 45 MB / Exchange 10 MB
    pub daily_download_budget: Option<u64>, // A1-F1：Gmail ≈2.3 GB（留余量）
}
```

**每个服务商的 `auth` 取值（v1 事实表，来自 A1 §五）**

| 服务商 | auth | 备注 |
|---|---|---|
| Gmail / Google Workspace | **`AppPassword`（默认，需 2SV）** / `OAuth2`（BYO；scope `https://mail.google.com/`，**受限**） | **P2 拍板：不自费 CASA ⇒ 不内置 client_id**；Workspace 用户用 `Internal` 客户端免验证（见 §11 反驳 1 与 §3.1.0） |
| Microsoft 365 / outlook.com | `OAuth2`（`outlook.office.com/IMAP.AccessAsUser.All` + `SMTP.Send` + `offline_access`，租户 `common`） | 需多租户应用；企业可能需管理员同意；SMTP AUTH 可能被租户关闭 |
| Yahoo / AOL | `OAuth2` | 需要自己的 client_id 注册 |
| Fastmail / Yandex / Mail.ru / Seznam | `OAuth2` | 各自注册 |
| Zoho | `OAuth2`（自助 Self Client） | 或应用专用密码 |
| iCloud / me.com | `AppPassword` | **没有**第三方 OAuth |
| 163 / 126 / yeah / 网易企业邮 | `AuthCode` | 16 位授权码；协议开关默认关；企业邮三方默认 30 天窗口 |
| QQ 邮箱 / Foxmail | `AuthCode` | 16 位授权码，需短信验证；IMAP/SMTP 默认关 |
| 腾讯企业邮 | `AuthCode` | 16 位授权码 |
| 阿里邮箱（个人/企业） | `AuthCode` | "三方客户端安全密码"16 位，最多 10 个；需管理员开启三方登录 |
| 新浪 / 139 / 自建 / Proton Bridge | `Password` 或 `AppPassword` | 逐家标注 |

> **硬规则**：`auth != OAuth2` 的服务商，UI **绝不显示**"使用 X 登录"按钮。
> 这条由门禁钉住（§10）。

#### 3.1.0 BYO 向导的三步（含 `client_secret` 分支，评审补）

```
第 1 步：打开链接（Google Cloud Console / Entra 应用注册），照图建一个"桌面应用"客户端
第 2 步：粘贴 client_id（微软还要 tenant）
        ├ **若 M0-8 的探测结论是"必须要 secret"** → 多一个输入框：client_secret
        │   （存进 vault 的 `SecretRef::Vault`，与令牌同一保护级别；界面明确写"它只存在本机"）
        └ 若结论是"PKCE 足够" → 不显示这个框（少一步是一步）
第 3 步：点"开始授权" → 走 §4 的回环流程；失败时按 §6 的六条文案给原因与下一步
```
> 为什么把这一步写进设计：BYO 是**没有内置注册号时唯一的合规出路**（A1-F37 禁止借用别人的 client_id），
> 而"要不要填 secret"不能靠猜 —— 它是 M0-8 的探测结论，探测完这条流程才算定稿。

#### 3.1.1 矩阵必须回答两个不同的问题（第二轮评审修正）

原矩阵把"**服务商支持 OAuth2**"与"**我们持有注册号**"混成一列，于是出现两种错：
①把 Yahoo/AOL/Zoho/Fastmail/Yandex 直接标成 `OAuth2` —— 它们每家都要我们**永久持有**一个注册号
（A1-F37 明确禁止借用别人的），而今天它们用应用专用密码就能工作；
②与 `providers.rs` 对不上（**Tuta** 在代码里是 `auth: OAuth2` 且 `supports_imap: false`，
新枚举没有能表达它的取值；**GMX** 在代码里有、矩阵里没有；阿里/腾讯企业邮/网易企业邮
矩阵写 `AuthCode` 而代码里是 `Password`，与 A1-F38 的"三方客户端安全密码"冲突）。

**改法**：矩阵拆成两列，并**逐条覆盖 `providers.rs` 里全部条目**（含 Tuta 的"不支持 IMAP"状态）：

| 服务商 | 服务商支持 OAuth2 | 我们持有注册号 | 生效 `AuthPlan` |
|---|---|---|---|
| Gmail / Workspace | ✅（受限 scope） | ✅（内置，早期 Testing） | `OAuth2`（+ 应用专用密码退路） |
| Microsoft 365 / outlook.com | ✅ | ✅ | `OAuth2` |
| Yahoo / AOL / Fastmail / Yandex / Zoho / Mail.ru / Seznam | ✅ | ❌（每家都要自己注册） | **`OAuth2Unavailable`** → 应用专用密码（拿到注册号后再翻成 `OAuth2`） |
| iCloud / me.com | ❌ | — | `AppPassword` |
| 163 / 126 / yeah / 网易企业邮 | ❌ | — | `AuthCode` |
| QQ / Foxmail / 腾讯企业邮 | ❌ | — | `AuthCode` |
| 阿里邮箱 | ❌ | — | `AuthCode` |
| 新浪 / 139 / GMX / 自建 / Proton Bridge | ❌ | — | `Password`（自建）或 `AppPassword` |
| **Tuta** | ❌（且**不支持 IMAP**） | — | `AuthPlan::Unsupported`（新枚举值；UI 明确说"这个服务商不提供 IMAP，无法作为桌面客户端使用"） |

**门禁改法**：`auth_plan` 快照测试覆盖**全部条目**；规则是"`OAuth2` ⇒ 必须有 `oauth` spec
**且** `Bundled` 时构建期 client_id 必须存在"；`OAuth2Unavailable` 不得渲染登录按钮。


### 3.2 令牌库（`secret.rs` 扩展）

**事实**：Windows 凭据管理器的 blob 上限是 **2,560 字节**（`CRED_MAX_CREDENTIAL_BLOB_SIZE`），
而 **微软 Entra 的 refresh token 常见 1-4 KB** ⇒ 凭据管理器**装不下**，不能作为唯一存储（A1 §五）。

```rust
/// 用户级加密文件：%APPDATA%\com.easyemail.desktop\vault.bin
/// 结构：magic(4) | version(1) | DPAPI blob（内含 zstd/JSON）
/// 加密：CryptProtectData（**用户级**，附应用熵；绝不使用 CRYPTPROTECT_LOCAL_MACHINE）
///       注意：prompt 流程（pPromptStruct）已被微软标记为 2027-02 移除 ⇒ 传 NULL。
#[derive(Serialize, Deserialize, Default)]
pub struct Vault {
    pub entries: std::collections::BTreeMap<String, VaultEntry>,   // key = account_id
}
#[derive(Serialize, Deserialize, Clone)]
pub struct VaultEntry {
    pub kind: VaultKind,               // OAuth2
    pub refresh_token: String,
    /// **第二轮评审补**：微软是轮换式令牌，"先写新再删旧"在单槽结构里根本表达不出来。
    /// 保留上一枚：`invalid_grant` 时先用它重试一次（服务端在宽限期内仍认旧令牌）。
    pub previous_refresh_token: Option<String>,
    /// **CAS 版本号**：并发写入时防止"旧令牌覆盖新令牌"
    pub generation: u64,
    pub scope: String,
    pub issuer: String,
    pub client_id: String,             // 内置或用户自备（BYO）
    pub tenant: Option<String>,
    pub obtained_at: i64,
    /// 已知过期时间（Google Testing 模式是 7 天；微软默认 90 天但会轮换）
    pub expires_hint_at: Option<i64>,
    /// 最近一次刷新失败的原因（**只允许存解析后的 `{error, error_description}` 摘要**，
    /// 且经 `redact()` 截断到 200 字；**绝不存原始响应体** —— 代理/门户的错误页会把请求体
    /// （含 refresh_token 与 client_secret）原样回显，那是第二轮评审抓到的一条真实泄漏路径）
    pub last_error: Option<String>,
    /// 是否在凭据管理器里也放了一份（只有 ≤ 2560 字节时才可能）
    pub cm_backed: bool,
}
pub enum VaultKind { OAuth2 }

pub struct TokenVault { /* 路径 + 内存缓存 + 写锁 */ }
impl TokenVault {
    pub fn open(path: &Path) -> Result<Self>;
    pub fn get(&self, account_id: &str) -> Result<Option<VaultEntry>>;
    /// 原子写：临时文件 + rename。
    /// **第二轮评审补：CAS** —— `expect_generation` 不匹配则拒绝写入（防止并发的旧写覆盖新令牌）。
    pub fn put(&self, account_id: &str, entry: VaultEntry, expect_generation: u64) -> Result<()>;
    pub fn remove(&self, account_id: &str) -> Result<()>;
    pub fn list_accounts(&self) -> Result<Vec<String>>;
    /// 诊断包里只允许出现"有/无令牌 + 过期时间 + 错误摘要"，**绝不含令牌**
    pub fn redacted_report(&self) -> Vec<VaultRedacted>;
}
```

**密码/授权码**继续走 Windows 凭据管理器（既有 `secret.rs`，v0.2 已验证），
因为它们短且天然属于"凭据"；**只有 OAuth 令牌进 vault**。两者都要能被"移除账号"清理干净。

### 3.3 账号配置扩展（`accounts.json`，保持向后兼容）

```rust
pub struct AccountConfig {
    // …既有字段
    /// 认证方式（新增；老配置缺失时按 provider 默认推断）
    #[serde(default)] pub auth: Option<AuthMethod>,
    /// OAuth 客户端来源：内置 or 用户自备（BYO）
    #[serde(default)] pub oauth_client: Option<OAuthClientRef>,
    #[serde(default)] pub enabled: bool,          // 默认 true
    #[serde(default)] pub sort_order: i32,
    #[serde(default)] pub color: Option<String>,
    #[serde(default)] pub in_unified_inbox: bool, // 默认 true
}
pub enum OAuthClientRef { Bundled, Custom { client_id: String, tenant: Option<String> } }
```
> 所有新字段 `#[serde(default)]`；老 `accounts.json` 直接能读（迁移见 §7）。

## 4. 接口（函数级）

```rust
// ── 服务商与发现 ──────────────────────────────────────────────
pub fn providers::all() -> &'static [Provider];
/// 分层发现：内置 → 域名 autoconfig → ISPDB → SRV → Autodiscover → 手动
pub async fn discover(domain: &str, email: &str, opts: DiscoverOptions) -> Result<Vec<DiscoveredConfig>>;
pub struct DiscoverOptions { pub allow_network: bool, pub timeout: Duration, pub ispdb_url: Option<String> }
pub struct DiscoveredConfig { pub source: DiscoverSource, pub imap: Endpoint, pub smtp: Endpoint, pub auth: AuthMethod, pub confidence: u8 }

// ── 账号生命周期 ──────────────────────────────────────────────
pub async fn account_create(store: &Store, cfg: AccountConfig) -> Result<AccountConfig>;
pub async fn account_update(store: &Store, id: &str, patch: AccountPatch) -> Result<AccountConfig>;
pub async fn account_delete(store: &Store, vault: &TokenVault, id: &str, local_data: LocalDataDisposition) -> Result<DeleteReport>;
pub async fn account_pause(state: &AppState, id: &str) -> Result<()>;      // 停任务 + 断连接
pub async fn account_resume(state: &AppState, id: &str) -> Result<()>;

// ── 认证 ─────────────────────────────────────────────────────
pub enum Credential { Password(String), AuthCode(String), OAuth(OAuthTokens) }
pub struct OAuthTokens { pub access_token: String, pub refresh_token: Option<String>, pub expires_at: i64, pub scope: String }

/// 决定该账号该走哪条认证路径（UI 用它渲染表单，不自己判断）
pub fn auth_plan(p: &Provider, acct: &AccountConfig) -> AuthPlan;
pub enum AuthPlan {
    /// 服务商支持 OAuth2，且我们（或用户）**确实持有一个 client_id**
    OAuth2 { spec: OAuthSpec, client: OAuthClientRef, consent_url_preview: String },
    /// **第二轮评审新增的第三态**：服务商支持 OAuth2，但我们**没有**注册号
    /// （A1-F37：不能借用别人的 client_id）⇒ 只能走应用专用密码，
    /// 并在 UI 里诚实说明"这个服务商支持 OAuth，但本版本没有注册号，请用应用专用密码"。
    OAuth2Unavailable { fallback: AuthMethod, why: &'static str },
    AuthCode { help: CredentialHelp },
    AppPassword { help: CredentialHelp },
    Password,
}

/// 授权码流程：起本地回环监听 + 打开**系统浏览器** + 等回调（PKCE S256）
pub struct LoopbackFlow { /* listener, state, verifier, cancel */ }
impl LoopbackFlow {
    /// 绑定 127.0.0.1:0（**SO_EXCLUSIVEADDRUSE**），生成 state 与 PKCE 挑战
    pub fn start(spec: &OAuthSpec, client: &OAuthClientRef) -> Result<(Self, String /*url*/)>;
    /// 等到授权码/错误或超时（默认 5 分钟）。
    /// **第二轮评审修正**：原来是 `wait(self)` + `cancel(self)`，两个都吃所有权 ⇒ 调用方**永远无法**
    /// 在等待时取消（点"取消"只能丢任务，端口与 5 分钟定时器泄漏）。改成 `&self` + 取消令牌。
    pub async fn wait(&self, timeout: Duration, cancel: CancelToken) -> Result<OAuthCode>;
    pub fn cancel(&self);           // 可在 wait 进行中调用（内部 `select!` 监听 cancel）
    pub fn redirect_uri(&self) -> &str;
}
/// **第二轮评审修正**：`OAuthCode` 必须把 PKCE 的 `code_verifier` 一起交出去 ——
/// 否则 `exchange_code` 根本拼不出符合 RFC 7636 的令牌请求（Google/微软会回 `invalid_grant`），
/// 而门禁只查了 `code_challenge` 与 `SO_EXCLUSIVEADDRUSE`，会放行一个"绿着的坏流程"。
pub struct OAuthCode { pub code: String, pub state: String, pub verifier: String }
pub async fn exchange_code(spec: &OAuthSpec, client: &OAuthClientRef, code: &OAuthCode) -> Result<OAuthTokens>;
pub async fn refresh_tokens(spec: &OAuthSpec, client: &OAuthClientRef, refresh: &str) -> Result<OAuthTokens>;
/// 撤销：**分平台语义**（第二轮评审）
/// * Google：调 `/revoke`（离线则入队重试；只有真的成功才说"已撤销"）
/// * 微软：**没有公共客户端的撤销端点**，`logout` 只结束浏览器会话 ⇒ 我们只能删本机令牌，
///   文案必须是"已在本机删除登录信息；要彻底撤销请到'已授权的应用'页移除授权"（附链接），
///   **绝不写成"已撤销令牌"**（14 §12 反驳 5 用它当 DPAPI 缓解措施，说大了就是虚假安全感）
pub async fn revoke(spec: &OAuthSpec, client: &OAuthClientRef, token: &str) -> Result<RevokeOutcome>;
pub enum RevokeOutcome { Revoked, LocalOnly, QueuedRetry, Failed(String) };

// ── 令牌供应（给 net 层用）────────────────────────────────────
pub struct TokenProvider { /* vault, single-flight 刷新锁, 内存 access token 缓存 */ }
impl TokenProvider {
    /// 取一个可用 access token：内存命中 → 否则刷新（**单飞**，避免并发刷新互相作废）
    pub async fn access_token(&self, account_id: &str, spec: &OAuthSpec, client: &OAuthClientRef) -> Result<String>;
    /// 刷新失败时把它翻译成可行动的 ApiError（见 §6）
    pub fn classify_refresh_error(status: u16, body: &str) -> ApiError;
}

// ── 连接验证（向导用；沿用 04 的连接预算）──────────────────────
pub async fn verify(acct: &AccountConfig, cred: &Credential) -> ConnectionReport;   // 既有，扩展为带 auth 方式
pub async fn probe_enablement(acct: &AccountConfig, cred: &Credential) -> EnablementReport;
pub struct EnablementReport {
    pub imap_ok: bool, pub smtp_ok: bool,
    pub smtp_auth_disabled: bool,       // 微软租户常见的"SMTP AUTH 被关闭"
    pub protocol_disabled: bool,        // 国内邮箱未开协议
    pub admin_consent_required: bool,   // 微软企业
    pub server_floor_hint: Option<String>, // 网易 30 天窗口
}
```

## 5. 状态机与主流程

### 5.1 加账号（向导）
```
输入地址
  ├ provider_suggest(domain) 命中内置库 ─────────────┐
  └ 未命中 → discover(domain)                       │
        ├ 域名 autoconfig 成功 → 用它的 IMAP/SMTP 设置
        ├ ISPDB 成功 → 同上
        ├ SRV 成功  → 同上
        ├ Autodiscover 成功 → 同上
        └ 全失败 → 手动填写（协议/端口/加密/认证方式）
                                                     ▼
                              auth_plan(provider, acct) 决定 UI
   ├ OAuth2   → 显示"使用 X 登录" + （首次）未验证应用提示图 → LoopbackFlow → exchange → vault.put
   ├ AuthCode → 显示图文步骤 + 输入框（前端预校验）→ 走密码通道存凭据管理器
   ├ AppPassword → 同上
   └ Password → 普通密码框
                              ▼
                    verify()（1 条 IMAP + 1 条 SMTP，最小化）
                       ├ 成功 → 保存账号 → 启动同步（04 阶段 A）
       └ 失败 → 按 §6 的分类给出**具体**指引，并保留已填信息可重试
```

### 5.2 令牌生命周期
```
access_token 缓存（内存，不落盘）
   ├ 过期前 5 分钟 → refresh_tokens（**单飞**：同账号并发刷新只发一次请求）
   │     ├ 200 → vault.put(新 refresh) → 删旧（微软轮换式）→ 继续
   │     ├ 400 invalid_grant → 判定"授权失效" → 标记该账号需重登 → 横幅 + 通知一次
   │     │       （Google Testing 模式的 7 天过期、用户撤销、管理员撤销都走这里）
   │     └ 5xx/网络 → 退避重试（不判定失效）
   └ 401/403（服务器拒绝 token）→ 强制刷新一次 → 仍失败则判失效
启动时：vault 里有刷新失败的记录 → 不静默重试，直接提示（避免每次启动都弹权限页）
```
**轮换安全（第二轮评审改写）**：
- **不要声称"任何时刻断电都至少留一个可用令牌"** —— 服务器一旦轮换、我们的写又没落盘，
  存下来的那枚就已经失效了。真实保证是：**单槽 + 上一枚备份（`previous_refresh_token`）+
  CAS 写入**，最坏情况是"需要重新登录一次"，而不是"永久失效"。
- `invalid_grant` 时**先用 `previous_refresh_token` 重试一次**，再判定"需要重新认证"。
- **并发刷新单飞**：同账号只允许一次刷新在飞；写回时用 `generation` 做 CAS，
  失败者读回新值而不是覆盖（第二轮评审：原设计"落盘的是最后一次返回的令牌"正是它自己
  在反驳 4 里警告的 bug）。
- **凭据管理器只在 ≤ 2560 字节时双写**并记 `cm_backed`；超长的（微软常见）只进 vault，
  UI 的"退出并撤销"是权威入口（否则会留下一份陈旧的副本）。

### 5.3 多账号与统一收件箱
```
账号列表（sort_order）→ 每个账号独立：auth / 同步策略 / 通知策略 / 是否参与统一收件箱
统一收件箱 = 各账号 INBOX 的并集视图（`thread_page` 加 account 过滤为空即代表全部）
暂停账号：停调度任务、关闭 IDLE、保留本地数据、UI 置灰 + "已暂停"
```

## 6. 失败模式与降级（**这一节是本模块的价值所在**）

| 触发 | 用户看到（照抄进 i18n） | 处理 |
|---|---|---|
| 微软租户关闭 SMTP AUTH | `你的管理员关闭了此邮箱的 SMTP 发信权限。可以让 IT 打开 SMTP AUTH，或改用网页版发送。` | `EnablementReport.smtp_auth_disabled`；仍可收信，发送按钮禁用并解释 |
| 微软企业需管理员同意 | `需要管理员批准：把这个应用加入你们组织的同意列表。`+ 可复制的说明块（含应用名与 scope） | `admin_consent_required`；给一键复制 |
| 国内邮箱协议未开 | `这个邮箱默认关闭了 IMAP/SMTP。请先到网页版设置里开启（步骤如图），再回来。` | `protocol_disabled`；附图 |
| 授权码错误/过期 | `授权码无效或已失效。请重新生成一个 16 位授权码（注意：生成后只显示一次）。` | 识别 `ERR.LOGIN.PASSERR`（**授权码错**）|
| **网易错误码三分（第二轮评审修正）** | `ERR.LOGIN.REQCODE` → `这里要填的是授权码，不是登录密码。`；`ERR.LOGIN.PASSERR` → `授权码无效或已失效，请重新生成。`；**`LOGIN.PERMDENY` → `这个邮箱没有开启 IMAP/SMTP（或企业策略不允许三方登录），请先到网页版设置里打开。`** | 原设计把 `LOGIN.PERMDENY` 也当成"授权码失效"，会让"协议没开"的用户无限重新生成授权码（A1-F38 明确它是协议未开）；`classify_login` 里目前**一个都没有**，要补 |
| **授权流程本身的失败（第二轮评审补，原设计完全没有）** | ①用户点"拒绝" → `你在浏览器里取消了授权。` ②浏览器一直没回调（5 分钟超时）→ `没有等到授权结果，请重试。` ③回环端口被占（AV/防火墙）→ `本机端口被占用，请稍后重试或改用应用专用密码。` ④`state` 不匹配 → `收到了意外的本地回调，出于安全已中止（已记录）。`（要写日志）⑤BYO 的 `redirect_uri_mismatch` → `你自己的应用没有允许这个回调地址，请在应用设置里加上 http://127.0.0.1（端口可变）` | 新增 `classify_auth_error(status, body)`（与 `classify_refresh_error` 并列），六条文案进 §6；反驳 2 承诺的"授权失败即时反馈"靠它兑现 |
| **刷新失败但错误体是 HTML/回显** | `登录状态无法刷新，请重新登录。` | 只解析 `{error, error_description}` 并截断 + `redact()`；**原始响应体绝不落库/进日志**（第二轮评审的泄漏路径） |
| 没生成授权码就用密码 | `这里要填的是授权码，不是登录密码。` | 识别 `ERR.LOGIN.REQCODE` |
| Google 7 天令牌过期（Testing 模式） | `Google 对未发布的应用每 7 天失效一次登录。请重新登录（发布应用后就不会了）。` | `invalid_grant` + 已知 Testing 模式 |
| Google 未验证应用警告 | 授权前的说明页：截图教用户点"高级 → 继续访问" | 首次登录前展示一次 |
| 网易企业邮只给 30 天 | 同步完阶段 A 后：`该邮箱的企业策略只允许第三方客户端读取最近 30 天的邮件。要更多，请在网页版把"收取范围"改大。` | 用 04 的 `server_floor` 检测 |
| 令牌库被删/损坏 | `本地登录信息已损坏，请重新登录（不影响服务器上的邮件）。` | 隔离 `vault.bin` → 备份 + 重建，账号保留 |
| 代理/证书问题 | 沿用既有错误分类 + "是否信任此证书"显式确认（ACC-11/SEC-14） | 绝不自签接受 |
| 账号被服务器禁用 | `服务器拒绝了这个账号（可能被停用或需要重新验证）。` | 分类 `Auth`，不重试 |

## 7. 迁移与兼容

- **v0.2 → v3**：**第二轮评审修正 —— 认证方式按"实际存了什么"推断，不按 `provider_id` 猜**：
  - 凭据管理器里有该账号的条目 → 保持 `AuthCode`/`AppPassword`/`Password`（老用户不动）
  - vault 里有该账号的令牌 → `OAuth2`
  - 两处都没有 → 加账号流程重新问一次
  原设计写的是"Gmail/微软=OAuth2"，那会让**已经能用授权码登录的 Gmail 老账号升级后直接失效**
  （去 vault 找不到令牌），与同一段里"不改用户行为"的承诺自相矛盾。
- **错误分类按 `AuthMethod` 分派**（第二轮评审）：`OAuth` 账号收到 535 → 提示"重新登录"；
  `AuthCode/AppPassword` → 提示"生成授权码"。同时更新 `error.rs` 里两条过期文案
  （`AuthHint::OAuth2Required` 现在写的是"当前版本尚未支持 OAuth2"，已经不对了）。
- **凭据**：老密码仍在凭据管理器，**不动**；OAuth 令牌写入新的 `vault.bin`。
- **`secret.rs` 保持**：密码/授权码路径不变（v0.2 真机验过），只新增 vault 模块。
- **服务商库数据驱动**：`providers.rs` 从硬编码迁移到"内置基线 + 可远程更新的签名 JSON"
  （更新与更新清单同一套托管；失败则用内置基线）。**理由**：Google/微软/国内服务商的规则每年都变，
  重新发版来不及。**注意**：远程配置只允许改数据（端点/scope/指引文案/配额），**不允许改行为代码**。

## 8. 性能预算

| 指标 | 预算 |
|---|---|
| 添加账号全流程（OAuth 已授权后） | ≤ 10 s 到首屏邮件 |
| 授权流程本身 | 用户交互时间；本地监听启动 ≤ 50 ms |
| 令牌刷新 | ≤ 800 ms（不含网络）；**单飞**保证并发下只发 1 次 |
| vault 读写 | ≤ 5 ms（几十个条目） |
| 自动发现 | ≤ 4 s（全部层级，含超时） |
| 账号列表渲染（20 个账号） | ≤ 50 ms |

## 9. 测试计划

| 层 | 用例 |
|---|---|
| 单元 | `auth_plan` 对 21 家服务商的分派表（快照测试：任何一家被改成 OAuth 都要显式改快照）；`classify_refresh_error` 的 12 种响应；`CredentialHelp` 的完整性（OAuth2 必须有 spec，AuthCode 必须有 steps） |
| 假服务器 | 本地假 OAuth 授权服务器（loopback）：成功、`error=access_denied`、state 不匹配、超时、token 端点 500、`invalid_grant`；断言 PKCE 参数、state 熵、监听用 `SO_EXCLUSIVEADDRUSE` |
| 令牌库 | 轮换（先写新后删旧）、断电模拟（写入中途 kill → 旧令牌仍可用）、损坏文件隔离、DPAPI 解密失败（换用户） |
| 真机矩阵 | 每家服务商：认证成功 + 一次发送 + 一次断线重连；网易/QQ/阿里按**真实步骤**走一遍（图文步骤是否对得上）；微软企业租户的 SMTP AUTH 关闭路径（可用租户实测或按文档模拟） |
| UI 冒烟 | 向导 5 条路径（OAuth/AuthCode/AppPassword/Password/自动发现失败转手动）各跑一遍 |
| 安全 | 抓包断言：授权码流程**没有**走内嵌 WebView；日志与诊断包里 grep 不到令牌；vault 文件 ACL 仅当前用户 |

## 10. 门禁新增项

1. **认证方式一致性**：`providers.rs` 中 `auth != OAuth2` 的条目**不得**出现 `oauth: Some(..)`；
   前端源码中"使用 X 登录"按钮的渲染条件必须引用 `auth_plan`（防止给 163 显示 OAuth 按钮）。
2. **令牌零泄漏**：仓库与运行产物中不得出现 `refresh_token` 的字面日志调用；
   诊断包导出代码里出现 `refresh_token` 字段名即失败（正则）。
3. **PKCE 与回环**：`oauth` 模块必须同时出现 `code_challenge` 与 `SO_EXCLUSIVEADDRUSE` 相关调用（源码检查）。
4. **不用内嵌 WebView 做 OAuth**：前端不得为授权流程创建 WebView 窗口（源码检查 + 人工验收项）。
5. **服务商库快照**：`auth_plan` 的分派表有快照测试（新增服务商必须显式登记）。

## 11. 逆向反驳（红队）

**反驳 1（最重要）：Google 的 IMAP scope 是"受限 scope"，自带 client_id 公开分发 ⇒ 必须走
Google 的受限 scope 验证 + **每年一次** CASA 第三方安全评估（已公布价格从约 $675 到 $5400/年不等），
外加主页/隐私政策域名验证。一个免费小工具承担不起 —— 这条会让"M1 支持 Gmail"直接落空。**
- 证据（A1 §五，已核实）：IMAP/POP/SMTP 需要 `https://mail.google.com/`；受限 scope 需 CASA，
  每年**完整重做**；未验证应用有 **100 用户终身上限**且 refresh token **7 天**失效。
- **改设计（P2 已拍板，2026-10-03：不花一分钱）**：
  1. **默认路径 = 应用专用密码**（Gmail 仍支持，需两步验证；A1-F39）。理由：用户 2 分钟搞定、
     **不需要建任何云项目、我们不需要持有任何注册号、不产生任何年费**；代码上它走 `AuthCode` 轴
     （与国内授权码完全同一条路径，凭据进 Windows 凭据管理器）。
     诚实标注它是 legacy 路径（Google 未来可能取消）⇒ 保留下面第 2 条作为迁移出口。
  2. **进阶路径 = BYO client_id**（用户自己建一个 OAuth 客户端，向导见 §3.1.0）。
     **Workspace 用户把客户端设为 `Internal` 即免验证、免 CASA、无警告屏** —— 这是企业用户的干净路径；
     个人用户建完自己的客户端后"发布为生产（未验证）"也不受 7 天限制（首次授权会看到"未验证应用"提示，
     可点"高级 → 继续"）。我们**不收集、不上传**任何东西，`client_id` 存在本地配置里。
  3. **不做内置 client_id**：内置就意味着要么每年付 CASA（$675-$5400），要么把全体 Gmail 用户塞进
     "终身 100 人 + 7 天令牌"的牢笼。用户的约束是零花费 ⇒ **两条都不能选**。
  4. **微软照旧内置**（应用注册免费、无 CASA），所以"微软一键登录、Gmail 要两步"这个差异
     必须在 UI 里**如实说明**（一句话：Google 对"读取邮件"范围的第三方客户端收年费，我们不转嫁给用户）。
- 结论：**产品里同时提供 1 与 2，都不花钱**；**3 明确不做**；若将来有人赞助 CASA 或项目拿到已验证客户端，
  只需把 `OAuthClientRef` 从 `Custom` 翻成 `Bundled`，**架构不用改**。
- **这一条改变了 ACC-04 的验收标准**：不是"Gmail 能登录"，而是"**两条免费路径都能用，且失败时用户知道走哪条**"。


**反驳 2：BYO client_id 对普通用户是灾难（要去 Google Cloud 建项目）。**
- 承认。**P2 拍板后，缓解方式变了**：①**默认走应用专用密码**（2 分钟、零云项目），普通用户
  **永远不需要碰 BYO**；②BYO 只在"用户不想把密码交给客户端"或"企业要求用 OAuth"时出现，
  ③提供**逐字段截图 + 复制按钮 + 校验**（client_id 形态校验、授权失败即时反馈）；
  ④文档里给一条"如果你不想折腾，请用应用专用密码"的出路。
- 让步：如果早期用户量小（< 100），BYO 甚至可以先不做，只留设置项占位。

**反驳 3：把令牌放 DPAPI 文件而不是凭据管理器，是不是降低了安全性？**
- 证据：凭据管理器装不下微软的 refresh token（2,560 字节上限，A1 已核实），所以它**不能**是唯一存储。
- 分析：两者都是**用户级**保护，都挡不住同用户下的恶意软件；DPAPI 文件多了原子写入、多条目、
  无长度限制、可整体备份的优势，少了"凭据管理器 UI 可查看/删除"的可见性。
- 让步：**两处都写**——凭据管理器存一份（当长度允许，作为可见性与用户可撤销的入口），
  vault 存权威副本；UI 里提供"退出登录并撤销令牌"（调用 Google revoke / 微软 logout）。

**反驳 4：单飞刷新 + 轮换式令牌很容易写出"两台设备/两个线程互相作废"的 bug。**
- 证据：微软每次刷新都会签发新 refresh token，旧的在服务端仍有效一段时间，但**必须删旧的**；
  并发刷新会产生两个新令牌，其中一个将被弃用。
- 缓解：①同账号刷新**串行化**（per-account async mutex + 单飞）；②刷新成功后**立刻落盘**再返回；
  ③若刷新返回的新令牌写入失败 → 视为失败（不回退到旧令牌，避免"用一个即将失效的令牌"）；
  ④测试覆盖"两个并发请求同时触发刷新"。
- 让步：多设备场景（同一账号在两台电脑）各持一份令牌，互不影响（服务端允许），不额外处理。

**反驳 5：自动发现（ISPDB/SRV/Autodiscover）会带来隐私与可用性风险（向第三方发请求、识别用户域名）。**
- 缓解：①只在**用户输入域名后**请求该域名自己的 autoconfig/SRV，不外发到我们的服务器；
  ②ISPDB 是 Mozilla 的公共服务，**默认开启但可在设置关闭**，并在隐私说明里写明"会向该服务查询域名配置"；
  ③Autodiscover 只对用户填写的域做一次；
  ④全部失败 → 手动配置，绝不静默失败。
- 让步：也可以完全不做 ISPDB，只靠"域名 autoconfig + SRV + 内置库"。**这条列进未决**（P21）。

**反驳 6：服务商库改成"远程签名 JSON"，等于引入了新的攻击面（改端点就能劫持邮件）。**
- 这是**真实且严重**的风险：远程配置里含 IMAP/SMTP 端点与 OAuth 端点。
- 缓解：①清单**必须 minisign 签名**（复用更新器的密钥与校验代码）；②只允许改**已存在服务商**的
  指引文案/配额/备注；**端点与 scope 的变更只允许在签名的完整清单里整份替换**且要域名白名单校验
  （例如 `*.qq.com` 只能指向 `*.qq.com`）；③用户可关闭"远程配置"，关闭后只用内置基线；
  ④诊断包里显示当前配置来源与版本。
- 让步：**v1 可以先只做"内置基线"**，把远程更新留到有实际需要时（用 P22 记录）。

**反驳 7：为 21 家服务商逐家写"图文步骤"维护成本很高，且会过期。**
- 缓解：文案集中在一个数据文件里，随版本更新；关键步骤页（网易/QQ/阿里/iCloud/Gmail 未验证警告）
  用**可替换的本地 HTML**，不改代码就能修文案；步骤里带"最后核实日期"，UI 上显示（诚实）。
- 让步：M1 只精写**前 8 家**（QQ/163/126/腾讯企业邮/阿里/Gmail/Outlook/iCloud），其余用通用模板 + 链接。

## 12. 未决问题

| # | 问题 | 影响 | 建议 |
|---|---|---|---|
| **P2** | Gmail 的三条路怎么配（内置 ID / BYO / 应用专用密码），以及是否投入验证+CASA | M1 门槛与预算 | 立即：内置 ID（Testing）+ 应用专用密码；随后加 BYO；CASA 待用户量 |
| **P20** | 是否在 v1 就做 BYO client_id 的设置界面 | 早期用户上限 | 做（S 级成本，解锁 100 人上限） |
| **P21** | 是否默认启用 ISPDB 查询 | 隐私 vs 覆盖率 | 默认开、可关、隐私说明写明 |
| **P22** | 服务商库远程更新是否进 v1 | 维护成本 vs 攻击面 | v1 只做内置基线；远程更新 M2（带签名与域名白名单） |
| **P23** | 凭据管理器与 vault 双写策略 | 复杂度 | 双写（反驳 3 的结论），实现时以 vault 为权威 |
| **P24** | 微软 publisher verification 是否现在做 | 企业可用性 | 暂缓（免费但需 Partner Program 账号与已验证域名）；企业用户先用 BYO 租户 ID |

## 13. 验收清单

- [ ] 21 家服务商的 `auth_plan` 快照测试通过；`auth != OAuth2` 的条目没有任何 OAuth 入口。
- [ ] Gmail：内置 ID 路径、应用专用密码路径、BYO client_id 路径三条全部走通（至少两条真机）。
- [ ] 微软：个人版与（若有租户）企业版登录成功；SMTP AUTH 关闭时给出正确指引且收信不受影响。
- [ ] 网易/QQ/阿里：按 UI 图文步骤能在真实账号上成功登录并开始同步。
- [ ] 令牌库：轮换、断电、损坏、换用户四种情况都有测试；日志与诊断包中 grep 不到令牌。
- [ ] 授权流程使用系统浏览器（抓包 + 代码检查双重确认），内嵌 WebView 未被用于 OAuth。
- [ ] 刷新并发测试：两个并发请求只触发一次刷新，且落盘的是**最后一次**返回的令牌。
- [ ] `check.ps1` 五项新增检查通过。
