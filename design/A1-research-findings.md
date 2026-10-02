# A1 事实核对与外部硬约束（研究日志）

> 本文是设计的**证据底座**：凡是"外部世界怎么规定的"这一类结论，都以本文为准；
> 其余文档引用本文的编号（如 `A1-F3`）而不是凭印象。
> 每条都标：**已核实（带来源）/ 未核实 / 会变**。实现前**必须**复核标"会变"的条目。
> 核对时间：2026-10-03（其余研究项陆续并入，见文末"待并入"）。

---

## 一、迁移与导入

| 编号 | 结论 | 状态 |
|---|---|---|
| **A1-F1** | **Gmail IMAP 下载上限约 2,500 MB/天**，上传 500 MB/天，最多 15 个并发 IMAP 连接，单个标签最多 1 万封。触发后连接会被限到近乎不可用，24 小时后重置。⇒ 50 GB 邮箱"全量重同步"需要 **≥20 天**。来源：[MailJerry Gmail limits 2026](https://www.mailjerry.com/gmail-limits/)，与 Google 官方带宽限制页一致（该页未直接抓取成功）。 | 已核实（厂商口径一致）；**会变** |
| **A1-F2** | **网易 163/126 企业邮：三方客户端"收取范围"默认只有最近 30 天**，更早的邮件除非用户在网页端把范围改大，否则 **IMAP 根本取不到**。管理员需设"中级安全模式"并开启 IMAP/SMTP；三方客户端必须用**授权码**（不是登录密码）。典型错误码 `ERR.LOGIN.REQCODE` / `LOGIN.PERMDENY` / `ERR.LOGIN.PASSERR`。授权码默认有效期 10 年且只显示一次。来源：[网易企业邮帮助中心](https://office.163.com/helpCenter/mail/d/2057123645218729986.html)（2026-09-09 更新）。 | 已核实；**会变** |
| **A1-F3** | **邮件上传（APPEND）大小上限因服务商而异**：Gmail `APPENDLIMIT=35651584`（≈34 MiB）、Office 365 默认 45 MB、Exchange 默认 10 MB、Dovecot 由管理员定。超限报 `NO Maximum size of appendable message has been exceeded`。来源：[imapsync FAQ](https://imapsync.lamiral.info/FAQ.d/FAQ.Messages_Too_Big.txt)。 | 已核实 |
| **A1-F4** | Gmail 会按 Message-ID 去重并把所有邮件收进 All Mail，**mbox 的文件夹结构映射成 Gmail 标签是不可靠的**（会重复/错位）。 | 已核实 |
| **A1-F5** | **Thunderbird 账号配置**可从 `prefs.js`（`mail.account.*`/`mail.server.*`/`mail.identity.*`）读到；地址簿在 `abook.sqlite` + `history.sqlite`（旧版为 `.mab`）；邮件在 `Mail/`、`ImapMail/`（mbox + `.msf`，或 maildir）。`global-messages-db.sqlite`（Gloda）**不要导**，本地重建。⚠️ prefs.js 是否已迁移到 JSON 未核实 → **两种都探测**。 | 基本核实；prefs 格式**未核实** |
| **A1-F6** | **Thunderbird 保存的密码技术上可解**：`key4.db`（SQLite）+ `logins.json`，现行 KDF 是 PBES2/PBKDF2-HMAC-SHA256 + AES-256-CBC（NSS 3.49+），校验串 `password-check\x02\x02`；**设了主密码则无法绕过**。来源：[firepwd.py](https://github.com/lclevy/firepwd/blob/master/firepwd.py)。**但没有任何维护中的 Rust 实现**，且该 Python 项目自称"仅教学用途"。 | 已核实（算法）；实现需自写 |
| **A1-F7** | **不建议做"导入其它客户端保存的密码"**：功能上与凭据窃取工具无法区分，会被 AV/EDR 与商店审核当作恶意行为，且把自己变成高价值攻击目标。**改为让用户重登一次（60 秒）。** | 设计建议（非事实） |
| **A1-F8** | **PST/OST：没有纯 Rust 实现，自己写是 L+（数月）**。可行路线是捆绑 `pffexport`（libpff）作为 sidecar 子进程。**libpff 许可证在仓库内自相矛盾**：`COPYING` 是 GPL-3.0 全文，而 GitHub 的 license API 报 LGPL-3.0 —— **需要律师判断**，不是工程判断。`pffexport` 导出 20 GB PST 需要数小时。OST 不要碰。 | 已核实（事实）；许可证**需法务** |
| **A1-F9** | **Classic Outlook 已于 2026-04 进入"opt-out"阶段**，支持延续到**至少 2029**；**New Outlook 没有 COM/MAPI 对象模型**。⇒ 依赖 MAPI 的导入路线只剩几年寿命。`outlook-mapi`（0.15.x，2026-03 仍活跃）可用但大量 `unsafe`，且要求位数匹配（32 位 Outlook 需 32 位辅助进程）。 | 已核实；**会变** |
| **A1-F10** | **EML/MBOX/Maildir**：解析用 `mail-parser` 0.11.9（Apache-2.0/MIT）。⚠️ **crates.io 上的 `mbox` crate 不是 mbox 解析器**（是"malloc-based box"）⇒ **自己写流式 `From ` 分割器**（mboxo/mboxrd/mboxcl + `>From` 反转义），S（3-5 天）。`maildir` 0.6.x 可用。 | 已核实 |
| **A1-F11** | **导入的用户预期是"本地"**：Thunderbird 的 ImportExportTools NG 与 Apple Mail 的"导入邮箱"都是建成**本地文件夹**，不是上传到服务器。⇒ 我们默认**导入为本地账号**，另给"上传到服务器"显式动作，且绝不自动上传（Gmail 500 MB/天上限会让它变成 100 天）。 | 已核实（行为）；设计建议 |
| **A1-F12** | **联系人**：vCard 用 `calcard` 0.3.14（Apache-2.0/MIT，含 vCard 2.1/3.0/4.0 + iCalendar + JSContact）。坑：vCard 2.1 常用 quoted-printable + `CHARSET=`；75 字节折行；`\n` 转义；3.0↔4.0 的 `TYPE` 大小写差异；base64 `PHOTO` 膨胀。**CSV 的坑**：RFC 4180 引号、BOM、**GBK/GB18030 编码**，以及**中文版 Outlook 导出的表头是中文**⇒ 必须给"手工列映射"UI，不要自动猜。 | 已核实 |

## 二、加密（PGP / S-MIME）

| 编号 | 结论 | 状态 |
|---|---|---|
| **A1-F13** | **`sequoia-openpgp` 2.4.1（LGPL-2.0-or-later）在 Windows 上并不难编译**：它提供 `crypto-cng` feature（用 Windows CNG），可以**完全避开 Nettle/OpenSSL/MSYS2**。来源：[crypto/backend.rs](https://docs.rs/sequoia-openpgp/latest/src/sequoia_openpgp/crypto/backend.rs.html)。**但 LGPL + Rust 静态链接在闭源商用产品里是许可义务问题**（重新链接/目标文件提供）。 | 已核实 |
| **A1-F14** | **`rPGP`（crate `pgp` 0.20.0，MIT OR Apache-2.0）**：36k 行、无系统依赖、14 个月从 0.16 到 0.20。**若许可证是硬约束，选它**；代价是第三方审计历史不如 Sequoia（⚠️ 未核实当前审计状态）。 | 已核实（版本/许可证）；审计**未核实** |
| **A1-F15** | **密钥发现（OpenPGP 的真正优势）**：WKD 是一次 HTTPS GET（`openpgpkey.<domain>/.well-known/openpgpkey/...`，z-base-32(SHA-1(localpart))）；`keys.openpgp.org` 有 REST API 且只服务经验证的 UID、不返回第三方签名；Autocrypt 走邮件头。**全部客户端可实现，不需要服务器。** | 已核实 |
| **A1-F16** | **一个像样的客户端实现要做的比想象多**：inline PGP + **PGP/MIME（RFC 3156）**双向、签名+加密同时、密钥导入导出（文件/剪贴板/附件）、密钥库加密存储、吊销/过期/**子密钥选择**、算法协商、诚实的验证 UI。**工作量 L（2-4 个月）**，密码学只是小头。 | 已核实 |
| **A1-F17** | **PGP 的 8 个 UX/安全陷阱**（必须写进 UI 与文档）：①"签名有效"不等于"发件人可信"（未知密钥是常态，给绿锁就是安全 bug）②efail 类攻击：CBC/CFB 可延展 + HTML 渲染 ⇒ 必须默认拦截远程内容、优先 AEAD/MDC、拒绝坏 MDC ③**主题与头部仍是明文**，用户以为不是 ④回复/引用解密内容时会误发明文（Thunderbird 多个 CVE 的来源）⑤密钥变更必须 TOFU 警告 ⑥口令保护私钥的解锁与缓存 ⑦Autocrypt 可被头部注入降级 ⑧**不要碰用户的 GnuPG keyring**，自带密钥库。 | 已核实 |
| **A1-F18** | **S/MIME 的结构性缺陷：互联网上没有 S/MIME 密钥发现机制。** 只能给"已持有证书"的人加密（来自他签过的邮件，或 LDAP/AD 的全局地址簿）。企业场景要靠 GAL ⇒ 需要 LDAP/M365 目录。**这是 S/MIME 加密体验普遍很差的根因，客户端改不了。** 另外：很多企业证书是 SHA-1/RSA-1024，现代策略会直接拒；同一地址可能有多张证书（要选择 UI）；**没有现成的高层 crate**，CMS 结构（`rasn-cms`）+ Windows 证书库（`windows` crate）+ CNG（`NCryptSignHash`，私钥不出 TPM）要自己粘。签名+验签 M（3-6 周），解密再 M。 | 已核实 |

## 三、日历 / 联系人协议

| 编号 | 结论 | 状态 |
|---|---|---|
| **A1-F19** | **CalDAV/CardDAV 用 `libdav` 0.11.0（ISC 许可，NGI 资助，`pimsync`/vdirsyncer 血统）**。`fast-dav-rs` 功能更全（含 RFC 6638 排期、RFC 6578 WebDAV-Sync）但 **LGPL-3.0 + 单人维护 + 8 star + 2025-10 才首发**，风险高。发现/认证/CRUD 约 M（3-5 周），同步与冲突再 M。**principal 发现各家不同**（Google 要邮箱，iCloud 需应用专用密码）。 | 已核实 |
| **A1-F20** | **iCalendar 用 `calcard`（Apache-2.0/MIT，含 RRULE 展开 + IANA 时区）**；**不要用 `icalendar` crate 做重复展开**（仍是 0.x，有"带本地时间的 RRULE 永不终止"等未修问题）。 | 已核实 |
| **A1-F21** | **日历真正的坑**：VTIMEZONE 常缺失/错误（未知 TZID 要明确按"浮动时间/本地时间"处理并告诉用户）；`DTSTART;TZID` + RRULE 锚定**墙上时间**（跨 DST 不能漂移）；**绝不物化无限 RRULE**，按可见窗口惰性展开；**`RECURRENCE-ID` 覆盖实例**、`EXDATE`/`RDATE`/`THISANDFUTURE`；`DURATION` vs `DTEND`；全天事件 `VALUE=DATE` 的 **DTEND 是排他的**；跨午夜。⇒ 即便用 calcard 展开，**正确性层还要 M（4-8 周）**。 | 已核实 |
| **A1-F22** | **会议邀请**：`text/calendar; method=REQUEST|REPLY|CANCEL|COUNTER|PUBLISH`，通常在 `multipart/alternative` 里**出现两次**（要在 `UID`+`SEQUENCE`+`DTSTAMP` 上去重）；**`SEQUENCE` 大小是正确性要求**（忽略更小的 REQUEST；CANCEL 的 SEQUENCE 取消 ≤ 它的实例），搞错会出现"已取消的会议还显示为有效"。RSVP 要发 `METHOD:REPLY` + `PARTSTAT` 给 `ORGANIZER`。Google 有时漏 `mailto:`。⇒ 只读展示 S-M（2-4 周）；能真正让 Exchange/Google 满意的 RSVP 再 M（3-6 周）。 | 已核实 |
| **A1-F23** | **日历视图**：议程列表 S（1 周）；**日/周网格**（重叠事件布局、全天行、拖拽新建、时区正确、月视图滚动）**M（2-3 个月）**，是前端布局引擎问题，最容易超期。**结论：v1 不做网格**，只做"邀请只读 + RSVP + 议程"。 | 设计建议（基于已核实的工作量） |

## 四、额外协议

| 编号 | 结论 | 状态 |
|---|---|---|
| **A1-F24** | **JMAP**：`jmap-client` 0.4.x（Stalwart，活跃但小众）；提供方是 Fastmail / Apache James / Stalwart / Cyrus；**Gmail 与微软都不提供**。工作量 M（4-8 周）。不需要 OAuth broker。**不进 v1**，但一旦有后端 trait 就便宜。 | 已核实 |
| **A1-F25** | **⚠️ 重要更正（前一版结论错了）**：Gmail 的 IMAP/POP/SMTP 需要 scope **`https://mail.google.com/`**，而它是 **受限 scope（restricted）** ⇒ 自带 client_id 公开发布**无法绕开** Google 的受限 scope 验证与**每年一次**的 CASA 第三方安全评估。旁证：Thunderbird 的发布源码里 imap/pop3/smtp 全映射到这一个 scope（[OAuth2Providers.sys.mjs](https://raw.githubusercontent.com/mozilla/releases-comm-central/master/mailnews/base/src/OAuth2Providers.sys.mjs)）。**代价**：评估费由第三方实验室定（已公布的一种价目：AL1 $675 / 带无限复审 $855 / AL2 $5400），Google 本身不收费；审核"可能数周"；每年**完整重做**。**未验证应用**（Published-unverified 或 Testing）的硬限制：**整个项目生命周期累计 100 个用户、不可重置**；Testing 模式另加 **refresh token 7 天失效**。来源：[OAuth 应用状态](https://developers.google.com/identity/protocols/oauth2/production-readiness/overview)、[受限 scope 验证](https://developers.google.com/identity/protocols/oauth2/production-readiness/restricted-scope-verification)、[CASA 指南](https://deepstrike.io/blog/google-casa-security-assessment-2025)。⇒ **设计对策见 `05-accounts-auth.md` §11 反驳 1（内置 ID + BYO client_id + 应用专用密码三条路并存）。** | 已核实 |
| **A1-F32** | Gmail 桌面端正确流程：**回环重定向 `http://127.0.0.1:<随机端口>`（只对桌面允许，移动端已弃用）+ PKCE S256 + 系统浏览器**。**OOB 已彻底关闭**（2022-02 停止新用、2023-01 全部封禁）。**设备码流程不适用于 Windows**。Google 明确**禁止内嵌 WebView 做授权**（会返回 `disallowed_useragent`）。client_secret 在桌面客户端里**不被当作机密**（官方原话），但仍要求"不要提交到公开仓库"——实践上 Thunderbird 就带着 secret 发布。 | 已核实 |
| **A1-F33** | 微软：**IMAP/SMTP 的 OAuth2（XOAUTH2）在 2026 年仍然支持**，个人 outlook.com 与 M365 都行；scope 是 `https://outlook.office.com/IMAP.AccessAsUser.All` + `SMTP.Send` + `offline_access`（资源 URL 形式必须完整）。**EWS 于 2026-10 开始全局禁用、2027 完全禁用，但 IMAP 不受影响。** **基本认证已在所有租户关闭且不可重新开启**；更麻烦的是"**未在使用 SMTP AUTH 的租户被微软直接关闭了 SMTP AUTH**" ⇒ 很多企业账号**能收不能发**，必须给专门的错误路径（04/05 已设计）。 | 已核实；**会变** |
| **A1-F34** | 微软 refresh token：默认 90 天（SPA/邮件 OTP 流程 24 小时），**每次刷新都会签发新令牌（轮换式）**，官方要求"拿到新令牌后安全删除旧的" ⇒ 必须**每次刷新都落盘**，且并发刷新要单飞。企业侧还需要 **publisher verification**（免费，但要 Microsoft AI Cloud Partner Program 账号 + 已验证域名 + MFA）；没有它，开了"基于风险的逐步同意"的租户里用户**无法同意新的多租户应用** ⇒ 企业用户会被卡。 | 已核实 |
| **A1-F35** | **Windows 凭据管理器 blob 上限 2,560 字节**（`CRED_MAX_CREDENTIAL_BLOB_SIZE`），而**微软 Entra 的 refresh token 常见 1-4 KB** ⇒ **凭据管理器装不下**，不能作为唯一令牌存储。正确做法：**DPAPI 用户级加密文件**（`CryptProtectData` + 应用熵；**绝不用 `CRYPTPROTECT_LOCAL_MACHINE`**；prompt 流程 2027-02 移除 ⇒ 传 `NULL`）。诚实说明：DPAPI 挡不住同用户下的恶意软件。 | 已核实 |
| **A1-F36** | 回环监听安全细节（RFC 8252 附录 B.3）：绑定回环后应设 **`SO_EXCLUSIVEADDRUSE`** 防止其它程序抢注；state 必须高熵；回调必须校验 state。 | 已核实 |
| **A1-F37** | **没有任何可复用的 OAuth broker / 公共 client_id**：Thunderbird 源码里明确写"**不要抄这些值，请自己注册**"；mutt 的脚本留空让用户填。Nylas 这类托管 OAuth 是**服务端邮件 API**，用了就等于放弃自建 IMAP/SMTP 客户端。⇒ **必须自己持有应用注册**（单点故障：被封则全体用户同时失效）。 | 已核实 |
| **A1-F38** | **国内邮箱全部没有 OAuth2**（只有授权码/专用密码）：网易 163/126/企业邮要"客户端授权码"（默认 10 年有效、只显示一次）；**QQ 要 16 位授权码且 POP3/SMTP/IMAP 默认关闭、需短信验证**；腾讯企业邮 16 位授权码；**阿里邮箱要"三方客户端安全密码"**（16 位、最多 10 个、需管理员先开启三方登录）。错误码：`ERR.LOGIN.REQCODE`（没用授权码）、`LOGIN.PERMDENY`（协议未开）。⇒ 服务商库必须有**认证方式轴**，且这些服务商**永远不显示 OAuth 按钮**。 | 已核实；**会变** |
| **A1-F39** | 其它服务商：**iCloud 没有第三方 OAuth，只能用应用专用密码**；Yahoo/AOL/Fastmail/Yandex/Mail.ru/Seznam/Comcast 有 OAuth（各自注册）；**Zoho 可自助注册 Self Client** 后走 XOAUTH2；Gmail 的应用专用密码仍存在但属 legacy（需两步验证）。 | 已核实 |
| **A1-F40** | 令牌管理的最佳实践（Google 官方）：优先 AEAD；**Google 已支持对 refresh token 做 DPoP 绑定**（公开客户端强烈建议）；refresh token 数量有上限，超限会静默作废旧令牌；`invalid_grant` 一律视为"需要重新认证"。 | 已核实 |

| **A1-F26** | **graph-rs-sdk 处于风险状态**：最新 3.0.1（2025-04）在 docs.rs 上**构建失败**，最后一个有文档的版本是 2.0.3（2024-10）；token 只存内存（无系统密钥库）。**⇒ 不要用 SDK**，用 `reqwest` + `serde` 手写约 8 个端点（`/me/mailFolders`、`…/messages`、`/me/messages/{id}`、`…/move`、`/me/sendMail`、`…/messages/delta`）。deltaLink 会过期（410）⇒ 必须内建"全量重同步"回退。**通知订阅需要公网 HTTPS**（微软要能访问并回显 validationToken）⇒ 桌面端**轮询 delta**。 | 已核实 |
| **A1-F27** | **EWS 对云上 Exchange 已死**：微软明确 **2026 年 10 月起全局开始禁用，2027 年 4 月完全禁用**（来源：[Deprecation of EWS in Exchange Online](https://learn.microsoft.com/en-us/exchange/clients-and-mobile-in-exchange-online/deprecation-of-ews-exchange-online)，2026-09-04 更新）。**⇒ 不要做 EWS 客户端。** 仅本地部署 Exchange 2016/2019 还有意义，且那些环境 IMAP + Autodiscover 通常仍可用 ⇒ 用清晰的管理员指引替代 EWS。 | 已核实；**会变** |
| **A1-F28** | **POP3 值得做，且便宜（S，4-7 天）**：`pop3`/`async-pop3` 都是 0.1.x ⇒ **自己写约 400 行**（USER/PASS、STAT、LIST、UIDL、TOP、RETR、DELE、QUIT、STLS）。要点：**`UIDL` 是唯一的稳定标识**（序号跨会话不稳定）⇒ 去重键；`DELE` 只是**标记**，真正删除在 `QUIT`；`TOP n k` 可低成本预览；**没有文件夹/旗标/服务器已读状态/服务器搜索**；`RETR` 可能**点填充**（`..`→`.`）且是 CRLF。**定位：本地账号（不与其他设备同步），UI 明确说明。** Thunderbird/Outlook/Foxmail 都仍支持且默认"在服务器保留副本"。 | 已核实 |
| **A1-F29** | **OAuth2 服务商矩阵（v1 事实表）**：Gmail/Workspace = OAuth2 必需，scope `https://mail.google.com/`；M365/Outlook.com = OAuth2 必需，`https://outlook.office.com/IMAP.AccessAsUser.All` + `SMTP.Send` + `offline_access`，多租户需管理员同意；**iCloud = 只有应用专用密码**，三方 IMAP 没有 OAuth2；**163/126/QQ = 授权码**（见 A1-F2）；Yahoo/Fastmail/Zoho/GMX/Yandex = OAuth2 或应用专用密码。crate 用 **`oauth2` 5.x**（PKCE/授权码/设备码/刷新）。 | 已核实；**会变** |
| **A1-F30** | **桌面端不需要任何 OAuth broker**：**RFC 8252** 明确认可 **loopback 重定向**（`http://127.0.0.1:<临时端口>/callback`）与自定义 URI scheme，Google 与微软都支持公共客户端 + PKCE。代价：**你要永久持有 client_id**，且它是单点故障（被封 ⇒ 全体用户同时失效）；企业租户可封禁未验证应用 ⇒ **Google 应用验证 / 微软 publisher verification** 决定企业市场（那是流程与费用，不是服务器）。 | 已核实 |
| **A1-F31** | **哪些能力"必须"有服务端**：Gmail push（Pub/Sub）、Graph 变更通知、Gmail API 受限 scope（CASA）、S/MIME 给新收件人加密（需要 GAL/LDAP，那是**别人的**服务器）、更新/崩溃上报/付费激活。**mail/contacts/calendar 协议本身与 OAuth 都不需要。** ⇒ v1 可以真正做到 serverless（红线 6 成立）。 | 已核实 |

## 五、由此产生的设计改动（已回写进各文档）

| 改动 | 落到哪 |
|---|---|
| **配额感知同步**：按服务商设"每日下载字节预算"（Gmail 2.5 GB/天），到预算就停，第二天续；UI 显示"今日配额已用完，明天继续" | `04-sync.md` §5.7（新增） |
| **诚实预期**：加账号时按邮箱大小与配额算出"首屏 1 分钟 / 全部 N 天"，并给"只要最近 90 天"的快捷选项 | `04-sync.md` §5.2、`05` 向导 |
| **163/126 引导**：检测到网易企业邮时，向导明确要求"去网页端把收取范围改大"，并显示当前只能取到的时间范围 | `05-accounts-auth.md`、`01-product.md` §6 |
| **v1 不做 PST**、**不做 Thunderbird 密码解密**（改为重登一次） | `01-product.md` §7 新增 P13/P14（需你拍板） |
| **v1 不做 Gmail API / Graph / EWS / JMAP**；推送用轮询式（history/delta/IDLE 分层） | `18-protocols-extensions.md` |
| **PGP 引擎选 rPGP（MIT/Apache）**，S/MIME 定位"尽力而为"；不做 GAL 就不要承诺企业加密 | `14-security-privacy.md` |
| **日历 v1 只做"邀请只读 + RSVP + 议程"**，网格与双向同步后置 | `18`、`01-product.md` P7 |
| **POP3 进 v1（本地账号）** | `01-product.md` P12、`18` |
| **导入默认本地**，上传到服务器是显式动作 | `06-migration.md` |
| **联系人 CSV 必须带列映射 UI**；vCard 用 calcard | `11-contacts.md`、`06` |
| **附件大小上限按服务商预检**（Gmail 34 MiB / O365 45 MB / Exchange 10 MB） | `07-compose-send.md` |

## 六、库能力核对（IMAP / SMTP / MIME，2026-10-03）

> 这一节里 F41 是一条**推翻既有结论**的更正：它推翻了 `HANDOFF.md` §5 与 `imap.rs`/`idle.rs` 顶部注释
> 里"IDLE 不投递 EXISTS"的说法。**它在设计上意味着"推送延迟 50 秒"从产品限制变成了实现缺陷，
> M1 必须修**（并且顺带把 README/HANDOFF 里的错误说明改掉）。

| 编号 | 结论 | 状态 |
|---|---|---|
| **A1-F41** | **⚠️ 重要更正：IDLE 的 EXISTS 在 async-imap 0.12.0 + imap-proto 0.17.0 上是能拿到的。** 审计代理**写了探针 crate 实测**（假 IMAP 服务器 + tokio duplex）：推送 `* 5 EXISTS` 后，`IdleResponse::NewData(MailboxData(Exists(5)))` 在 **206 ms** 内返回。机制：`wait_with_timeout` 只吞掉无标签的 `* OK/NO/BAD/BYE`（`Response::Data{status:Ok,..}`）、`Continue`，以及把 `Done` 转给 unsolicited 通道；`MailboxData(Exists)` 落到通配分支 → `NewData`。⇒ **既有 `HANDOFF.md` §5、`imap.rs`/`idle.rs` 顶部注释里"IDLE 不投递 EXISTS、所以只能 50 秒轮询"的说法对当前版本是错的**（0.12.0 发布于 2026-09-30）。剩余真实限制：`wait_with_timeout(dur)` 是**对每次 `next()` 各包一层 timeout**，任何响应（含 `* OK Still here`）都会重置窗口 ⇒ **不是硬超时**，想要心跳必须自己在外面再包一层 `tokio::time::timeout`。**行动**：先用我们自己的 loopback 夹具加一条"推送后 1 秒内必须收到 NewData"的断言复核（0.5 天），确认后再删除那两处错误注释。 | **已实测（探针）**；待本仓库夹具复核 |
| **A1-F42** | **分部取件比"解析不了"更糟：会让连接直接关闭。**（**M0-4 实测收紧**：`imap-proto` 0.17 的 `msg_att_body_section` 只认 `BODY [section] <number> " " nstring`，**连 `BODY[]<0.10>` 都解析失败** —— 服务器回答分部取件时通常带 `.offset`，所以**补丁前分部取件完全不可用**，不是"只差 offset"）；解析错误会把 `read_closed=true` ⇒ **一条无法解析的响应把整条连接弄死**。**同样的陷阱适用于所有 imap-proto 不认识的扩展响应**：`THREAD`、`ESEARCH`、`NAMESPACE`、`LIST-STATUS` 在 imap-proto 里**根本没有语法** ⇒ 发这些命令等于自杀。修复路径：给 imap-proto 提 PR（`src/rfc3501/body.rs:86` 约 3 行）或用 `[patch.crates-io]` 挂本地 fork。 | 已实测（探针） |
| **A1-F43** | **哪些能力可用（对本项目最重要的一张表）**：**可用** = IDLE+EXISTS、MOVE、CONDSTORE（`select_condstore`/`Fetch::modseq`/`CHANGEDSINCE`/`SEARCH MODSEQ`）、复杂 SEARCH（字符串透传）、APPEND（含 flags/internaldate）、SPECIAL-USE、COMPRESS=DEFLATE、ID、QUOTA、METADATA(只读)、Gmail X-GM-LABELS/X-GM-MSGID。**要绕** = UIDPLUS 的 **`APPENDUID`/`COPYUID` 被解析但丢弃**（`append`/`uid_copy`/`uid_mv` 只看 `check_done_ok`）⇒ **APPEND 后拿不到新 UID，必须自己 `UID SEARCH HEADER Message-ID` 去找**；SORT 可发原始命令（`* SORT` 有语法）；UTF8=ACCEPT 可 `ENABLE` 但要绕过 mutf7；NOTIFY 理论可行未验证。**不能用** = THREAD、ESEARCH、NAMESPACE、LIST-STATUS、BINARY、SETMETADATA。**QRESYNC 没有 API**，但理论可用原始命令（imap-proto 认识 `* VANISHED`），实践中 **用 CONDSTORE 更划算**。 | 已核实 |
| **A1-F44** | **结论：留在 async-imap 0.12。** 迁移路径都更差：同步 `imap` crate 的稳定版是 **2021-01-13**、依赖 imap-proto 0.10、阻塞式、IDLE 只给 `TimedOut/MailboxChanged`（没有 EXISTS 数）；`imap-codec` 是**编解码器不是客户端**（自己写 TLS/SASL/标签/路由/IDLE 状态机，2.5-4k 行、6-10 周才到今天的水平）；Graph/Gmail 快路各 2-4 周且推送都要公网端点。**成本对照**：修 IDLE 认知 0.5 天、补 imap-proto 分部取件 1-2 天、用本地手段补上真实缺口 1-2 周。 | 已核实 |
| **A1-F45** | **lettre 0.11.23 的真实边界（⚠️ 已更正一处错误）**：✅ XOAUTH2、8BITMIME、SMTPUTF8、连接池、`AsyncTransport::send_raw`（**字节级原样发送**：只做点填充，不做 LF→CRLF 重写）；❌ **DSN（RFC 3461 的 RET/ENVID/NOTIFY/ORCPT 全都没有）**、❌ BINARYMIME/BDAT、❌ PIPELINING（一条命令一个往返）、❌ **附件无法流式**（`Body` 是 `Vec<u8>`；今天 25 MB 附件峰值约 90 MB）。**❌ `SIZE=n` 预检也是错的**（第二轮评审核对源码：`MailParameter::Size` 只出现在单元测试里，`AsyncSmtpConnection::send()` 的 `mail_options` 只由 SMTPUTF8/8BITMIME 组成，EHLO 的 `SIZE` 关键字被丢进 `_ => ()` ⇒ **lettre 连服务器宣告的上限都读不到**）⇒ 发送前预检只能靠**本地服务商表 + 硬上限**，不要写任何 `SIZE=` 断言。替代：`mail-send` 0.6.2（stalwart，支持 XOAUTH2/DKIM，但依赖 mail-builder ^0.5 且同样没有 DSN）；composing 用 **`mail-builder` 1.0.0**（显式 CTE/自定义头），发送仍走 lettre 的 `send_raw`。 | 已核实（含更正） |
| **A1-F46** | **mail-parser 0.11.9 的两个坑**：①`references()` 返回 `Empty \| Text(String) \| TextList(Vec<Cow<str>>)`。**⚠️ 已由 M0-1 探针实测更正**：原文说「只出现一个 id 时 `as_text_list()` 会得到 None」是**错的** —— 0.11.9 下 `as_text_list()` 对两种变体都作答（`Text` 返回 1 个元素的列表）。真正的坑在另一头：**`as_text()` 作用在多 id 的 `TextList` 上只返回最后一个 id**。⇒ 实践规则：**读引用一律用 `as_text_list()`，永不用 `as_text()`**（`Empty` 时两者都是 None，所以仍然要处理"没有引用"这一态）。探针：`tests/loopback.rs::probe_references_has_three_shapes`；②`message_id()` **已经剥掉尖括号**。好消息：`body_text()` 内置 HTML→text（`decoders::html::html_to_text`），不必再引 html2text；`thread_name()` 提供 RFC 5256 的 base subject（含多语言 Re:/Fwd: 剥离）⇒ 线程串用它比自己写正则稳。另外 **`X-GM-THRID` 被 imap-proto 解析但 async-imap 的 `Fetch` 没有访问器**（约 10 行 PR 就能白拿 Gmail 的服务端会话 id）。 | 已实测（含更正） |

## 七、仍待并入

| 研究项 | 会影响的文档 | 状态 |
|---|---|---|
| 索引方案（FTS5 分词器对比、tantivy、附件文本抽取、CJK 精确性） | `09-search.md`、`15-performance.md` | ⏳ 仍在进行。未到之前 `09` 以"候选方案 + 必做基准"的形式给出设计，**不硬编码分词器结论** |
| 本仓库 loopback 夹具对 IDLE 的复核（A1-F41 的行动项）✅ 已完成（M0-1） | `04-sync.md`、`HANDOFF.md`、`README.md` | ⏳ 属实现阶段第 1 步（0.5 天） |

