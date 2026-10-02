# 14 安全与隐私

> **目标**：把"默认安全"做成产品差异点，而不是折磨：**默认零外部请求、凭据与令牌零明文、
> 危险内容明确拦并说明原因**；同时把"我们到底连了哪些服务器"对用户完全透明。
>
> 覆盖 ID：SEC-01…15、UI-18（外链策略）、DIST/SEC-15（更新签名）。
> 事实依据：A1-F6/F7（凭据库与"不要解密别人的密码"）、A1-F13…F18（PGP/S-MIME 的现实与陷阱）、
> A1-F35/F36（DPAPI 与回环）、A1-F40（令牌最佳实践）。既有红线见 `README.md` §3。

---

## 1. 目标与非目标

**目标**
1. **默认安全**：正文默认不加载任何远程内容；默认不信任附件；默认不执行任何来自邮件的脚本。
2. **凭据与令牌**：密码/授权码进 Windows 凭据管理器；OAuth refresh token 进 **DPAPI 用户级加密文件**（A1-F35）；日志/诊断包/缓存库零明文。
3. **可解释的安全**：每一条拦截都说清"拦了什么、为什么、怎么放行"，并且放行是**按发件人/按封**可控的。
4. **透明**：应用内可查看"我们访问过的外部端点清单"（本地记录，不上传）。
5. **专业能力**：OpenPGP（v1 范围内做扎实）与 S/MIME（尽力而为）——**并诚实标注能力边界**。

**非目标**
- 不做反病毒引擎（那是系统 Defender 的事；我们只做"危险类型不预览 + 提示"）。
- 不做 VPN/代理服务（只支持用户自己的代理，ACC-11）。
- 不做遥测（默认且**没有**开关；诊断靠用户手动导出，SEC-13）。
- 不承诺"防住已入侵的同用户恶意软件"（诚实写在文档里，A1-F35）。

## 2. 威胁模型（先明确对手，再谈措施）

| # | 对手 | 能力 | 我们的对策 | 明确不防 |
|---|---|---|---|---|
| T1 | **恶意邮件**（HTML/CSS/图片/附件/链接） | 任意 HTML、远程资源、危险附件、钓鱼链接 | 沙箱 iframe + 独立 CSP + 默认拦截远程内容 + 危险类型不预览 + 链接真实域名提示 + 头注入过滤 | 不去防"用户执意打开 .exe 并运行"（只给红色警告） |
| T2 | **被入侵的发件人账号** | 发来带附件/链接的邮件 | 同上 + 首次往来标记 + 相似域名检测 | 无法判断"这封信是不是本人写的" |
| T3 | **同用户的恶意软件** | 读写用户文件、注入进程 | 凭据管理器 + DPAPI 用户级加密 + 不落明文；敏感操作有审计日志 | **防不住**（明文令牌在进程内存里终会出现；诚实披露） |
| T4 | **物理接触/离线磁盘** | 拿到磁盘文件 | DPAPI 加密（换用户/换机器不可解）；缓存库不含凭据 | 用户已登录状态下的直接读取 |
| T5 | **网络中间人** | 篡改 DNS/TLS | 严格 TLS 校验 + 自签需显式同意 + 证书变更告警 + 更新包 minisign 校验 | 用户主动信任了错误证书 |
| T6 | **恶意更新** | 伪造更新包 | minisign 签名（**不可关闭**）+ Authenticode（17 §6）+ 私钥离线备份 | 私钥泄露 |
| T7 | **服务商/第三方观察者** | 看到邮件元数据 | 不用我们的服务器中转（红线 6）；OAuth 走官方端点；PGP 端到端加密可选 | 服务商本身能看到明文（IMAP 语义如此，PGP 才能解决） |
| T8 | **我们自己（开发者）** | 可能犯错或作恶 | 无遥测、出站端点清单可审计、诊断包脱敏且用户可预览、代码里对敏感路径有门禁 | 用户必须信任源码/构建；鼓励开源（P4） |

## 3. 用户可感知行为

| 场景 | 用户看到 |
|---|---|
| 打开一封带远程图片的邮件 | 顶部黄条：`已拦截 12 张远程图片、3 个外部样式、1 个疑似跟踪像素（来自 hz.yxzq.com）` + 按钮`显示图片（仅本次）`/`总是显示来自 xxx 的图片` |
| 解密一封 PGP 邮件 | 顶部绿条或灰条（**区分"已验证密钥"与"密钥未验证"**）+ 明文只以纯文本展示（**不自动渲染 HTML、不加载任何远程内容**） |
| 收到签名但未知密钥的邮件 | 灰条：`签名有效，但这个密钥你还没验证过（首次见到）` + `查看密钥指纹`/`信任此人的密钥` |
| 密钥变了 | 红条：`⚠ 这个人的密钥变了（指纹从 AAAA 变成 BBBB）。可能是换了密钥，也可能是中间人。` + 明确指引 |
| 点邮件里的链接 | 悬停显示真实域名；点击时若域名与显示文本不符/是 punycode/是危险 scheme → 二次确认弹窗 |
| 打开 .exe/.js/双扩展名附件 | 红色警告条 + 不预览 + `仍要保存`（保存后不自动打开） |
| 想发加密邮件 | 收件人旁边有钥匙图标（有密钥=绿、无密钥=灰并提示"无法加密，需要先取得对方的公钥"）；主题栏下方提示"**主题与头部不会被加密**" |
| 查看"网络活动" | 设置 → 隐私：`本应用访问过的外部端点`（主机名 + 用途 + 次数 + 最近时间，本地记录）；以及一句"本应用没有遥测、没有账号体系" |
| 导出诊断包 | 导出前可**预览**内容（脱敏后的日志/配置/同步状态）；明确写"不含邮件正文与凭据" |

## 4. 数据结构与接口

### 4.1 存储（与 03/05 对齐）

```rust
/// 凭据（既有权衡）：密码/授权码 → Windows 凭据管理器；OAuth 令牌 → DPAPI 文件
pub enum SecretStore { CredentialManager, DpapiVault }        // 05 §3.2 已定

/// 白名单（03 的 allowlist 表）：远程内容、垃圾判定复用
pub struct AllowEntry { pub kind: AllowKind, pub value: String, pub purpose: AllowPurpose, pub created_at: i64 }
pub enum AllowKind { Email, Domain }
pub enum AllowPurpose { RemoteImages, Spam }

/// 网络活动台账（本地，仅存主机名与用途；用于"透明"与排障）
pub struct NetAuditEntry { pub host: String, pub port: u16, pub purpose: NetPurpose, pub first_at: i64, pub last_at: i64, pub count: u64, pub result: NetResult }
pub enum NetPurpose { Imap, Smtp, Pop3, Jmap, Graph, OAuth, Discovery, Ispdb, Srv, Autodiscover, Wkd, Keyserver, UpdateManifest, UpdateDownload, CrashLocal }
pub enum NetResult { Ok, AuthFailed, Timeout, TlsError, Refused }

/// PGP 密钥库（**自己的**，不碰用户的 GnuPG keyring，A1-F17-⑧）
pub struct PgpKey { pub fingerprint: String, pub uids: Vec<String>, pub emails: Vec<String>,
                    pub created_at: i64, pub expires_at: Option<i64>, pub is_secret: bool,
                    pub trust: PgpTrust, pub source: KeySource, pub revoked: bool }
pub enum PgpTrust { Unknown, Tofu, Verified, OwnKey }
pub enum KeySource { Imported, Wkd, Keyserver, Autocrypt, Generated }
pub struct PgpSecretRef { pub fingerprint: String, pub encrypted: Vec<u8>, pub kdf: KdfParams }   // 口令保护
```

### 4.2 接口（函数级）

```rust
// ── 渲染与远程内容 ───────────────────────────────────────────
pub struct RenderDecision { pub allow_remote: bool, pub reason: RemoteDecisionReason, pub allow_entry: Option<AllowEntry> }
pub enum RemoteDecisionReason { Default, PerMessageOptIn, SenderAllowlisted, DecryptedMail /* 永远拦截 */, UserDisabledBlocking }
/// **第二轮评审修正**：白名单命中不再是充分条件 —— 还必须 `auth_ok`（DKIM/DMARC 通过）。
/// 否则伪造 From 就能为一个受信任的域名重新打开追踪像素（真实的隐私回退）。
pub fn decide_remote(policy: &RenderPolicy, sender: &str, auth_ok: bool, allow: &[AllowEntry], decrypted: bool) -> RenderDecision;
pub fn analyse_external(html: &str) -> TrackerReport;                     // 08 §4.3
pub fn rewrite_links(html: &str) -> String;                                // 08 §4.3
pub fn classify_url(url: &str) -> UrlRisk;                                 // Safe | Mismatch | Punycode | DangerousScheme | Shortener
pub struct LinkDecision { pub allow: bool, pub warn: Option<String>, pub real_host: String }

// ── 钓鱼与外发信号 ───────────────────────────────────────────
#[derive(Serialize, Clone, Debug)]
pub struct PhishSignals {
    pub display_name_mismatch: bool,       // 显示名里含另一个地址/域名
    pub lookalike_domain: Option<String>,  // 形近域名（同形字/缺字/换后缀）
    pub reply_to_differs: bool,
    pub first_contact: bool,               // 与本机历史往来比对
    pub external_sender: bool,             // 与账号域不同（企业场景）
    pub auth_results: Option<AuthResults>, // 来自 Authentication-Results 头（DKIM/SPF/DMARC）
    pub link_text_mismatch: u32,           // 显示文本与 href 域名不符的链接数
}
pub fn analyse_phishing(msg: &MessageDetail, raw_headers: &str, history: &dyn ContactHistory) -> PhishSignals;
pub struct AuthResults { pub spf: Option<AuthVerdict>, pub dkim: Option<AuthVerdict>, pub dmarc: Option<AuthVerdict> }
pub enum AuthVerdict { Pass, Fail, SoftFail, Neutral, None, Unknown }

// ── 附件风险 ────────────────────────────────────────────────
pub fn classify_attachment(name: &str, mime: &str) -> AttachmentRisk;      // 08 §4.4：Safe|Caution|Dangerous
pub fn dangerous_extensions() -> &'static [&'static str];                  // 单一来源，测试钉住

// ── OpenPGP（v1 做扎实：inline + PGP/MIME）──────────────────
pub enum PgpOp { Verify, Decrypt, DecryptAndVerify, Sign, Encrypt, SignAndEncrypt }
pub struct PgpOutcome { pub op: PgpOp, pub signer: Option<PgpKeyRef>, pub signature: SignatureState,
                        pub recipients: Vec<PgpKeyRef>, pub plaintext: Option<Vec<u8>>, pub mdc: MdcState }
/// **第二轮评审修正**：原来是 `mdc_ok: Option<bool>`，表达不了"这封根本没有 MDC"这一态。
/// rPGP 0.20 的 SEIPDv1 默认 `CheckFirst`（先验 MDC 再放明文，坏 MDC 直接报错），
/// 但**老式 SED 包是"没得可查"就解密的** ⇒ 实现者会把未认证的明文当正常邮件显示。
/// 三态：Ok / Bad / Absent；`Absent` 一律**只显示纯文本、不渲染 HTML、不加载任何远程内容**，
/// 并给一条醒目提示（或者按设置直接拒收）。
pub enum MdcState { Ok, Bad, Absent }
pub enum SignatureState { Good, GoodUnknownKey, BadSignature, Missing, ExpiredKey, RevokedKey, KeyChanged }
pub fn pgp_detect(raw: &[u8]) -> PgpShape;                                  // None | InlineArmored | PgpMimeSigned | PgpMimeEncrypted
pub async fn pgp_process(raw: &[u8], keys: &PgpKeyStore, ctx: &PgpContext) -> Result<PgpOutcome>;
pub async fn pgp_build(msg: &OutgoingMessage, recipients: &[PgpKeyRef], opts: &PgpBuildOptions) -> Result<Vec<u8>>;
pub async fn pgp_import(bytes: &[u8], store: &mut PgpKeyStore, source: KeySource) -> Result<Vec<PgpKey>>;
pub async fn pgp_generate(name: &str, email: &str, algo: PgpAlgo, passphrase: Option<&str>) -> Result<(PgpKey, PgpSecretRef)>;
pub async fn pgp_wkd_lookup(email: &str) -> Result<Option<Vec<u8>>>;        // HTTPS GET openpgpkey.<domain>（z-base-32(SHA1(localpart))）
pub async fn pgp_keyserver_lookup(email: &str) -> Result<Option<Vec<u8>>>;  // keys.openpgp.org REST
pub fn pgp_tofu_check(store: &PgpKeyStore, email: &str, fp: &str) -> TofuVerdict;   // FirstSeen | Same | Changed

// ── S/MIME（尽力而为）────────────────────────────────────────
pub fn smime_certs_for(email: &str) -> Result<Vec<SmimeCert>>;              // CertOpenSystemStoreW("MY")，EKU 1.3.6.1.5.5.7.3.4
pub async fn smime_verify(raw: &[u8]) -> Result<SmimeOutcome>;
pub async fn smime_sign(raw: &[u8], cert: &SmimeCert) -> Result<Vec<u8>>;   // CNG：私钥不出 TPM
pub async fn smime_decrypt(raw: &[u8]) -> Result<Vec<u8>>;
pub fn smime_encrypt(raw: &[u8], certs: &[SmimeCert]) -> Result<Vec<u8>>;   // **只能给已持有证书的人加密**（A1-F18）

// ── 本地数据与隐私 ──────────────────────────────────────────
pub async fn diagnostics_export(state: &AppState, opts: &DiagOptions) -> Result<PathBuf>;   // 脱敏 + 可预览（16 §…）
pub fn redact(text: &str) -> String;                                        // 邮箱地址/令牌/路径 → 掩码
pub struct NetAudit { /* 环形缓冲 + 落库节流 */ }
impl NetAudit { pub fn record(&self, host: &str, port: u16, purpose: NetPurpose, result: NetResult); pub fn snapshot(&self) -> Vec<NetAuditEntry>; }
```

### 4.3 CSP 与沙箱（不变式，门禁钉住）

| 层 | 策略 | 说明 |
|---|---|---|
| 外壳（`index.html` + `tauri.conf.json`） | `img-src 'self' data: blob: https: http:` | **必须**允许 https/http，否则正文的"显示图片"永远失效（既有 bug #8） |
| 正文 iframe **默认** | `default-src 'none'; img-src data: blob:; style-src 'unsafe-inline'; font-src data:` | 截图/追踪像素进不来 |
| 正文 iframe **解锁后** | `default-src 'none'; img-src data: blob: https: http:; style-src 'unsafe-inline'` | 只对当封/该发件人 |
| 解密后的 PGP 正文 | **强制** `img-src data: blob:`，**没有任何例外**（白名单、`UserDisabledBlocking`、用户再点一次都不行） | efail 类攻击的防线（A1-F17-②）。**第二轮评审**：原设计 §4.3 与 §5.1 给了两种不同答案（"永远拦截" vs "除非用户对该封再点一次"），而这条防线必须只有一个来源 —— 见 §5.1 的修正 |
| 附件 HTML 预览 | 同"正文默认"策略 + 独立 sandbox | 附件里的 HTML 比邮件正文更危险 |

## 5. 状态机与主流程

### 5.1 邮件渲染决策链（顺序不可颠倒）
```
打开邮件
 1. 是否 PGP/S-MIME 加密？ → 是：解密（内存中），标记 decrypted=true
 2. 净化：剥离 script/iframe/object/embed/meta[http-equiv]/on*=/javascript:（08 §4.3）
 3. cid: → data:（内嵌图不算远程内容）
 4. 统计远程资源（图片/样式/字体/beacon）→ TrackerReport
 5. allow_remote = decide_remote(policy, sender, auth_ok, allowlist, decrypted)
       ├ decrypted=true → **永远 false，没有例外**（白名单/再点一次都不行 —— 与 §4.3 的唯一口径一致；
         第二轮评审：原来这里写了"除非用户再点一次"，与 §4.3 冲突，而 efail 防线取决于哪一个赢）
       ├ 白名单命中 **且 auth_ok（DKIM/DMARC 通过）** → true
       └ 否则 → 用户点"显示图片"才 true（仅本次，切走即失效）
 6. 生成 iframe srcdoc（含对应 CSP）→ 渲染
 7. 链接改写（data-ee-href）→ 点击走 LinkDecision
 8. 附件按 classify_attachment 展示（Dangerous 不给预览按钮）
```

### 5.2 PGP 处理与信任
```
收到邮件
  ├ pgp_detect → InlineArmored / PgpMimeSigned / PgpMimeEncrypted
  ├ 解密：私钥（口令保护则提示解锁，缓存 N 分钟）→ 检查 MDC/AEAD：
  │     ├ 坏 MDC → **拒绝展示明文** + 红条"这封加密邮件可能被篡改"
  │     └ 好 → 明文只在内存；**纯文本展示**；远程内容永久拦截
  ├ 验签：Good / GoodUnknownKey / BadSignature / Expired / Revoked / KeyChanged
  │     └ KeyChanged → 红条 + 需要显式重新信任（TOFU 记账；A1-F17-⑤）
  └ 已知密钥自动解密（有私钥时）；未知密钥只能看密文
写信用 PGP
  ├ 收件人密钥查找：本地库 → WKD → keys.openpgp.org（设置可关网络查找）
  ├ 有密钥 → 默认加密（可关）；无密钥 → 阻止加密并说明"需要先取得公钥"
  ├ PGP/MIME（RFC 3156）优先；对方只支持 inline 时降级 inline（设置项）
  └ 发送前提示：**主题与头部不加密**（A1-F17-③）
回复解密邮件
  └ 默认继续加密（对同一批收件人）；若有人缺密钥 → 明确警告"这封会以明文发出"
```

### 5.3 链接与附件
```
链接点击 → classify_url
  ├ Safe + 域名与显示文本一致 → 系统浏览器打开
  ├ Mismatch/Punycode/Shortener → 弹窗显示真实域名与完整 URL，确认后打开
  └ DangerousScheme（javascript:/data:text/html/file:/ms-*）→ **直接拒绝**并说明
附件
  ├ Dangerous（exe/js/lnk/bat/双扩展名/宏文档）→ 红色警告、不预览、保存后不自动打开
  ├ Caution（docm/xlsm/iso/zip 等）→ 黄色提示
  └ Safe → 预览（08 §4.4）
```

## 6. 失败模式

| 触发 | 用户看到 | 处理 |
|---|---|---|
| 私钥口令错误/无法解锁 | "无法解锁私钥（口令错误？）+ 重试" | 3 次后停止提示，改为手动解锁入口 |
| 私钥缺失（只有公钥） | "这封邮件用你的公钥加密，但本机没有对应私钥" | 提示导入私钥 |
| 密钥过期/吊销 | 灰条/红条 + "仍可解密，但签名不再可信" | 明确状态，不藏 |
| Tofu 密钥变更 | 红条 + 要求显式确认 | 不允许"静默接受" |
| 远程内容被"总是允许"后用户后悔 | 设置 → 隐私 → 白名单列表，可逐条删除、可一键清空 | — |
| 诊断包里出现敏感信息 | 导出前预览 + 自动脱敏（地址/路径/令牌形态） | `redact()` 单测覆盖 |
| 用户使用企业代理（TLS 中间人） | 首次遇到未知 CA → 显式询问"信任此证书？"（显示指纹与颁发者） | 绝不自签静默接受 |
| Windows Defender 拦我们的附件保存 | 提示"系统阻止了这次保存" | 不绕过，说明原因 |
| S/MIME 收件人证书过期/无 EKU | "找不到可用于加密的证书" | A1-F18：无 GAL 就不承诺企业加密 |
| 收到 HTML 邮件内含表单 | 表单被 sandbox 禁止（无 `allow-forms`） | 说明"邮件里的表单已被禁用" |

## 7. 迁移与兼容

- **既有四项 `check.ps1` 安全检查全部保留**（外壳 https、正文默认拦截、宽度规则、CJK 分词），本模块只**增加**检查。
- v0.2 的"每封解锁"语义保留；新增"按发件人白名单"是**新增能力**，默认关闭。
- `allowlist` 表新增（03 v3 迁移）；老的"解锁"状态是内存态，不迁移。
- PGP 密钥库是全新模块；**不读取也不写入**用户的 GnuPG 目录（A1-F17-⑧）。

## 8. 性能预算

| 指标 | 预算 |
|---|---|
| 净化 + 远程资源统计（1 MB HTML） | ≤ 30 ms |
| 钓鱼信号分析（含历史查询） | ≤ 20 ms |
| PGP 验签（1 MB 邮件） | ≤ 150 ms |
| PGP 解密（RSA-2048） | ≤ 200 ms |
| PGP 密钥查找（WKD 网络） | ≤ 3 s（超时即放弃，不阻塞阅读） |
| S/MIME 验签（CNG） | ≤ 100 ms |
| 诊断包导出（100 MB 日志） | ≤ 30 s，峰值内存 ≤ 50 MB |

## 9. 测试计划

| 层 | 用例 |
|---|---|
| 恶意样本夹具 | `tests/fixtures/security/`：带 script/onerror/iframe/表单/base 标签、`cid:` 与远程图混用、1×1 像素、CSS 背景追踪、`<a>` 文本与 href 不符、punycode 域名、`javascript:` / `data:text/html` / `file:` 链接、双扩展名附件、Unicode 双向控制字符文件名、`\r\n` 头注入 |
| 单元 | `decide_remote` 真值表（含 decrypted 永假、**白名单必须 auth_ok**、MdcState 三态）；`classify_url`；`analyse_phishing`（12 条信号）；`classify_attachment` 扩展名清单；`redact()` |
| PGP | 与 GnuPG 的命令行交叉验证：我们签 → GnuPG 验；GnuPG 签/加密 → 我们验/解；PGP/MIME 与 inline 双向；坏 MDC 样本（人工构造）必须拒绝展示；未知密钥/密钥变更/过期/吊销各一条 |
| S/MIME | 自签测试证书（Windows 证书库）签/验/加解密往返；无 EKU 证书被拒 |
| 网络透明 | 跑一遍完整流程（加账号/同步/发信/搜索/更新），断言 `NetAudit` 记录的主机集合 ⊆ 文档化的端点清单（防止偷偷加端点） |
| 脱敏 | 诊断包 grep：不得出现令牌、密码、完整邮件正文；邮箱地址按规则掩码 |
| **脱敏（评审补）** | **日志模板本身不得包含邮件派生文本**（主题/正文片段/搜索词/通知文案）：主题只在用户显式勾选"包含样本头部"时才出现，导出预览里单独列出该项 |
| 真机 | 一份真实钓鱼样本 + 一份真实 PGP 邮件（若有）+ 一份真实 S/MIME 邮件（企业） |

## 10. 门禁新增项

1. **解密邮件永不自动加载远程内容**：单测 + 前端源码检查（`decrypted` 分支必须走最严策略）。
2. **危险扩展名清单单源**：前端不得自带一份扩展名黑名单（必须来自后端 `dangerous_extensions()`）。
3. **不读写 GnuPG 目录**：源码正则（`\.gnupg`）禁止出现。
4. **端点清单一致性**：`NetAudit` 的 `NetPurpose` 枚举与文档 §11 的端点表一一对应（测试遍历枚举）。
5. **脱敏函数覆盖**：`redact()` 对 8 类敏感形态（邮箱/令牌/路径/手机号/授权码/私钥块/信用卡形态/身份证形态）各有断言。
6. **凭据零明文**：仓库与运行产物中不得出现 `password` 字面量写日志（正则）；缓存库与 vault 的字段名不得出现在日志模板里。

## 11. 我们访问哪些外部端点（透明清单，与 `NetPurpose` 一一对应）

| 用途 | 端点（示例） | 何时 | 可关闭 |
|---|---|---|---|
| IMAP / SMTP / POP3 | 服务商地址（来自内置库或自动发现） | 同步与发信 | 否（核心） |
| OAuth 授权与令牌 | `accounts.google.com` / `login.microsoftonline.com` / 各服务商 | 登录与刷新 | 否（用 OAuth 时） |
| 自动发现 | `autoconfig.<你的域名>`、`<你的域名>/.well-known/autoconfig/...`、SRV 查询、`autodiscover.<你的域名>` | 加账号时 | 是 |
| ISPDB（Mozilla） | `autoconfig.thunderbird.net` | 加账号且前两层失败时 | **是（默认开，可关，P21）** |
| WKD | `openpgpkey.<收件人域名>` | 查找公钥（你点"查找/加密"时） | 是 |
| keys.openpgp.org | REST API | 同上（WKD 未命中） | 是 |
| 更新清单与包 | `<你的更新地址>` | 每 6 小时 / 手动 | 是（可关闭自动检查） |
| **崩溃上报** | —— | **不存在**（无遥测、无上报） | — |

> 设置 → 隐私里展示这张表 + `NetAudit` 的**实际**访问记录：任何"文档没写但实际连了"的端点都能被用户发现（这也是门禁第 4 条想守的东西）。

## 12. 逆向反驳（红队）

**反驳 1：默认拦截远程图片会让邮件"看起来是坏的"（很多营销邮件没有图片就是空白），用户会关掉拦截。**
- 证据：这是真实的摩擦点。
- 缓解：①黄条不遮挡正文、文案解释"这封邮件含 N 张远程图片（可能用于追踪你是否打开）"；
  ②`显示图片（仅本次）` 一键、位置显眼；③对**已白名单**的发件人自动显示（用户主动建立的信任）；
  ③**绝不在默认配置下产生外部请求**（这是产品承诺 D5/D6，不能为了好看牺牲）。
- 让步：给"总是显示所有远程图片"的开关，但放在高级设置里，并写明后果。

**反驳 2：PGP 的"绿锁"极易误导（签名有效 ≠ 发件人可信，A1-F17-①）。**
- 缓解：**三态**而不是两态：`已验证（你信任过这个密钥）` / `签名有效，密钥未验证（首次见到的密钥）` /
  `签名无效/密钥已变更（警告）`；颜色分别是绿/灰/红；灰色状态**不出现"锁"图标**（避免误读）。
- 让步：`信任此人的密钥` 需要用户主动点击（TOFU 记账），并且在密钥变更时**必须**重新确认。

**反驳 3：做 PGP 是 L 级工程（2-4 个月），对"愿意换过来"的贡献可能很小。**
- 证据：A1-F16 明确"密码学只是小头，PGP/MIME 双向 + 密钥管理 + 诚实的验证 UI 才是工作量"。
- 缓解：**分两步**：v1 先做"验签 + 解密 + 密钥导入导出 + WKD 查找"（只读向，价值最大：
  用户能读加密邮件、能验证签名）；**发送加密**（构建 PGP/MIME）放 M3 后段。
- 让步：如果用户群不需要 PGP（大多数办公用户不需要），可以只做"识别并提示"（至少不把加密邮件显示成乱码），
  把完整实现再后置一档。**这条请拍板（P8）。**

**反驳 4：S/MIME 承诺不了企业体验（没有密钥发现，A1-F18），做它是不是浪费？**
- 证据：没有 GAL/LDAP 就只能给"已持有证书的人"加密；且企业证书里大量 SHA-1/RSA-1024 会被现代策略拒。
- 改设计：**S/MIME 定位为"验签 + 解密 + 用系统证书签名"**（并把 **LDAP/GAL 的里程碑排在 S/MIME 加密之前** —— 没有 GAL 就不承诺企业加密，顺序颠倒等于先做一个用不了的功能），加密只对"从签名邮件里拿到的证书"生效，
  并在 UI 上说明限制；不做 GAL 集成（那是 M3+ 的 LDAP）。
- 让步：如果企业用户是主要目标，则先做 LDAP/GAL（那才是 S/MIME 加密的前提），顺序不能颠倒。

**反驳 5：DPAPI 用户级加密"看起来安全"，但同用户恶意软件依然能读（A1-F35）。会不会给用户虚假的安全感？**
- 缓解：①UI/文档明确写"这防的是别的用户与离线磁盘，不防已在你账户下运行的恶意程序"；
  ②提供"退出并撤销令牌"（调用 Google revoke / 微软 logout），把损失窗口压到最小；
  ③M3 评估 OS 级更强的存储（凭据管理器只能存小的，Windows Hello/TPM 绑定是可能的加强路径）。
- 让步：不宣称"军事级加密"之类的营销话术。

**反驳 6：透明端点清单会随着功能增加而腐化（文档写了 A，代码连了 B）。**
- 缓解：①`NetPurpose` 枚举是**代码里的单一来源**，任何新出站请求必须新增枚举值（编译期强制覆盖 match）；
  ②门禁第 4 条把枚举与文档表对齐；③`NetAudit` 记录实际主机，用户可核对（反驳 6 的根治办法就是"可观测"）。
- 让步：允许"用户域名"这类动态端点（自动发现），在文档里说明"主机由你输入的邮箱决定"。

**反驳 7：无遥测意味着我们收不到崩溃信息，质量问题会拖很久。**
- 缓解：①`startup-error.log` + 本地崩溃日志 + 一键诊断包（16）；②首次崩溃后温和提示"要不要导出诊断包发给作者"；
  ③**不默认上传**（红线），但用户主动发邮件附诊断包是完全可以的。
- 让步：如果用户量上来后维护压力过大，再评估"可选、明示、可关的匿名崩溃上报"——那需要托管端点（违反红线 6），
  届时要你明确同意。

## 13. 未决问题

| # | 问题 | 影响 | 建议 |
|---|---|---|---|
| **P8** | PGP 做到什么程度 | L 级工作量 | v1：验签+解密+密钥管理+WKD；发送加密后置；**请拍板** |
| S1 | PGP 引擎：rPGP（MIT/Apache）还是 Sequoia（LGPL，但 `crypto-cng` 无系统依赖） | 许可证 vs 审计历史 | **rPGP**（许可证简单）；若你接受 LGPL 义务则 Sequoia |
| S2 | Autocrypt 是否做 | 生态弱、策略难 | v1 不做（只做 WKD + 手动/keyserver） |
| S3 | S/MIME 是否做，以及是否先做 LDAP/GAL | 企业路线 | 先验签/解密；加密等 LDAP |
| S4 | SEC-11 本地库加密（SQLCipher） | 依赖/许可 | M3 评估；本设计只预留接口 |
| S5 | 是否做"仅解锁时显示通知摘要" | 隐私 | M2 小功能 |
| S6 | 网络活动台账是否默认开启 | 隐私 vs 透明 | 默认开（只记主机名，不上传），可关 |

## 14. 验收清单

- [ ] 默认配置下打开任意邮件**零外部请求**（抓包 + `NetAudit` 双重验证）。
- [ ] 恶意样本夹具 12 类全部被正确拦截/提示，且都有可读原因。
- [ ] PGP：与 GnuPG 交叉验证通过；坏 MDC 拒绝展示；密钥变更必须显式确认；解密邮件不加载远程内容。
- [ ] S/MIME：系统证书库签/验/加解密往返通过（含"没有可用证书"的清晰失败）。
- [ ] 凭据/令牌：日志、诊断包、缓存库 grep 不到；vault 文件 ACL 仅当前用户。
- [ ] 诊断包可预览且脱敏（8 类敏感形态断言）。
- [ ] 端点清单与 `NetPurpose` 一致（门禁自动验证）。
- [ ] `check.ps1` 六项新增检查通过。

> **章节名对照（模板 13 节）**：数据结构→§4.1 存储。
> 内容齐全，只是节名与模板不完全同名（见 design/README.md 的说明）。
