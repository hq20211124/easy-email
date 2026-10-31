# A4 · 内网 / 离线部署：今天真能做的是哪几件，缺的是哪几件

> 2026-10-20 写。起因是安全项 5（用户 2026-10-20 工作单里的"5，6，7 都加"）。
> **写法纪律**：每一条都指到代码或配置的 `文件:行`；做不到的直接写"不存在"。
> 这份文档不是承诺，是一张"现在能做什么"的清单 —— 拿去和甲方 IT 谈之前先照它核一遍。

## 一、先把结论摆前面

| 问题 | 今天的答案 |
| --- | --- |
| 端点完全不通公网，能装吗 | **看目标机有没有 WebView2 运行时**：有就能装，没有就卡在安装那一步（§2.3）。仓库里没有离线安装开关 |
| 能让更新不连公网吗 | **能，但不是靠设置**。更新地址是编译期写死的，要么换地址重新构建，要么就不装更新通道（§3） |
| 会被软件"打电话回家"吗 | **不会**。没有任何遥测/崩溃上报；检查更新只在"关于"面板里由用户点一下才发生；会主动联网的定时动作只有"草稿镜像到**用户自己的**服务器"（§4） |
| 能给甲方 IT 一份可归档的送审材料吗 | **一部分能**（诊断包、SHA256SUMS、许可清单、磁盘加密检查脚本的退出码），**关键的几样不存在**（安装包签名、SBOM、离线构建、第三方审计，§5） |

## 二、安装

### 2.1 产物与安装范围

Windows 只出 NSIS 一个产物（`src-tauri/tauri.windows.conf.json:18`），文件名 `*-setup.exe`
（`.github/workflows/release.yml:209`）。安装范围是**当前用户**：`nsis.installMode: "currentUser"`
（`tauri.windows.conf.json:25`）。**机器级 / 需要管理员的安装程序不存在**（`perMachine` 只在
`design/17-distribution.md:367` 里作为 M3 的待办出现，没有配置、没有脚本）。
这个取舍不是偷懒：提权运行的应用发不出 Toast，通知与托盘那一层会整体失效（理由记在 `installer/hooks.nsh:8-10`）。

写盘位置：

- 程序文件在用户目录下的应用包位置（NSIS currentUser 常规）。
- 数据与配置：`%APPDATA%\com.easyemail.desktop\`（应用标识 `com.easyemail.desktop`，
  `src-tauri/tauri.conf.json:5`；目录由 `app.path().app_config_dir()` 解析，`src-tauri/src/lib.rs:4135-4138`）。
  2026-10-20 在一台真实用过几周的机器上 `ls` 到这个目录里有：
  `accounts.json`、`user.db`（+ `-shm`/`-wal`）、`cache.db`（+ `-shm`/`-wal`）、`tokens\`、
  `oauth-client.json`、`spool\`、`startup-error.log`。另外两个是按需出现的：`update-state.json`（安装过更新之后）、
  `easy-email-diagnostics-*.txt`（生成过诊断包之后）。
  **`tokens\` 里是什么**：每个账号一份刷新令牌、一份 OAuth client_secret，文件名是 `token-<16 hex>.bin` /
  `client-<16 hex>.bin`（哈希自账号 id，邮箱地址不会出现在路径里，`tokenstore.rs:165-185`）。内容是 **DPAPI
  `CryptProtectData` 密封后的 blob，额外熵是账号 id，用户域（`CRYPTPROTECT` 标志为 0，从不使用机器域）**，
  `tokenstore.rs:194-236`。⇒ 文件只有**同一台机器上的同一个 Windows 登录用户**能解开；拷到别的机器或别的
  用户下直接解密失败，程序这时报"令牌无法解密（可能需要重新登录）"而不是崩溃（`:141-148`）。
  非 Windows 才走另一条路：密钥 = SHA-256(固定域 || 机器标识 || 账号 id)，机器标识优先读
  `/etc/machine-id`（`:341-348`、`:386-394`），密文是 SHA-256 计数器模式密钥流 + 8 字节校验标签
  （`:306-329`、`:362-365` 自己写明"这不是新代码里 AEAD 的替代品"）。⇒ **Linux 那条路只是"别台机器读不走"
  级别的防护**（machine-id 本身可读，且不是真 AEAD），本产品的部署目标是 Windows，不要用这条路径评估安全性。
  **写在明处的边界**：DPAPI 保护的是"别人拿走文件读不走"，**不保护"这个登录会话本身被接管"**；当前用户
  运行时进程可以正常解密，所以口令泄露/会话劫持后令牌仍是可得的。这就是 A3 把"操作系统全盘加密 + 登录口令"
  列为部署前置条件的原因。Windows 路径上**没有**再单独收紧这两个文件的 ACL（`restrict_dir`/`restrict_file`
  在非 Unix 分支是空实现，`:288-292`），靠的是 `%APPDATA%` 本身的用户配置文件 ACL。
  **`oauth-client.json` 要单独说**：那是**用户自己放进来**的 OAuth 客户端注册文件（Google 原生格式，
  `crates/mail-core/src/oauthclient.rs:82,85-89`），文件里**含明文的 client_secret**。程序不会把它复制进任何
  数据库（运行用的是密封存储里那一份，`tokenstore.rs:97`，并有
  `a_client_secret_round_trips_and_is_not_readable_in_the_file` 钉住），但这个文件本身是明文，
  属于"贵方若启用自带 OAuth 注册就要纳入自己的密钥管理"的一项。没有这个文件时 OAuth 登录不可用，
  密码/授权码账号不受影响。
- WebView2 自己的缓存在 `%LOCALAPPDATA%\com.easyemail.desktop\EBWebView`（`design/17-distribution.md:153`）。
- 注册表只写 HKCU：`Software\RegisteredApplications`、`Software\Easy Email\Capabilities`、`mailto` 与
  `.eml` 的关联（`installer/hooks.nsh:12-21`），卸载时删除（`:23-31`）。
  **没有** Run 键、**没有** 计划任务、**没有** 服务、**没有** 常驻后台进程（不存在）。

### 2.2 安装包没有 Authenticode 签名

安装包**未签名**（`README.md:202-204`、`src-tauri/Cargo.toml:24-26`），后果就是第一次运行时的 SmartScreen 提示。
配置里那两个字段（`digestAlgorithm: sha256`、`timestampUrl: http://timestamp.digicert.com`，
`tauri.windows.conf.json:22-23`）只是**参数**：仓库里没有证书、没有 `sigintool` 调用、没有任何签名步骤
（全库 grep `TAURI_WINDOWS_*` / `signtool` 零命中）。给 IT 的说法只能是"手动放行一次，或由贵方用贵机构的
代码签名证书重签"。**注意那个时间戳地址是明文 `http://`** —— 它只在构建期取时间戳，不影响运行期，
但在受限网络里它需要出站 HTTP 白名单，值得写进部署单。

### 2.3 离线机器装不上的原因，以及代价

`webviewInstallMode: { type: "downloadBootstrapper", silent: true }`（`tauri.windows.conf.json:21`）
意思是安装程序**在安装时联网下载** WebView2 运行时。门禁把这一步钉住了，并且把代价写在门禁的输出里：
"the installer now needs a network connection to fetch the runtime"（`tools/check.ps1:2191-2199`），
理由是换成 `embedBootstrapper` 就等于随包再分发一份微软的二进制。

所以要在全隔离网络里装，今天只有三条路，**没有一条是软件里已有的开关**：

1. 目标机预装 WebView2 运行时。**运行时已存在时 bootstrapper 是空操作**，安装能过 —— 这是今天唯一
   不需要改代码的离线部署路。（本机 2026-10-20 实测：`C:\Program Files (x86)\Microsoft\EdgeWebView\Application\`
   下有 `155.0.4283.45/60/62` 三个版本目录，即 WebView2 Runtime 作为独立"常青"安装已存在；
   **"哪一版 Windows 出厂自带"我们没有逐版本验证过**，甲方要按贵方镜像的实际版本核对。）
   注意 `design/17-distribution.md:215` 那张流程图里写的是 `embedBootstrapper`，与实际配置
   （`downloadBootstrapper`，`tauri.windows.conf.json:21`）不一致 —— **以配置与门禁为准**。
2. 先在有外网的一台机器上取离线安装包（微软官方的 EverGreen / offline installer），与我们的 `*-setup.exe`
   一起放进部署包 —— 这一步是**甲方 IT 的运维动作**，本仓库没有为它准备任何东西（不存在）。
3. 改成 `embedBootstrapper` 重新构建，包体会大上百 MB（`design/17-distribution.md:31,301` 里写过 127 MB 的量级，
   **只是设计文字，没有产物验证过**）。

## 三、更新通道：地址写死在编译期

### 3.1 今天的样子

- 清单地址只有一个：`https://github.com/hq20211124/easy-email/releases/latest/download/latest.json`
  （`src-tauri/tauri.conf.json:40`）。
- 校验用的公钥同样是编译期常量（`tauri.conf.json:38`，minisign key id `795C573235B412FA`）；
  门禁要求它是**内容而不是路径**、且端点必须是 https（`tools/check.ps1:388-397`）。
- 清单里按平台有四个条目（linux deb/rpm/appimage + windows x86_64），CI 少一个就中断发布
  （`.github/workflows/release.yml:276-283`）。
- 签名校验发生在**写盘之前**：下载完在内存里 `verify_signature`，通过才 `write_to_temp` 交给安装器
  （`tauri-plugin-updater` 2.13.2 的 `src/updater.rs:837-843` 与 `:1093,1103-1119`）。这一步**不能关掉**
  （`src-tauri/Cargo.toml:24-26`）。
- 传输用的是 rustls + 系统信任根（`Cargo.lock` 里 `rustls-native-certs`，见第 101 轮"统一信任根"）。

### 3.2 对甲方 IT 真正要紧的三条

1. **没有运行时可改的更新地址。** 代码、配置、前端里 grep `endpoint_override` / `mirror_url` 只命中一处，
   而且是 `design/17-distribution.md:170` 那段**结构体草稿**；`镜像` 这个词只出现在设计文字里
   （`design/17-distribution.md:38` 写的"设置里可换更新镜像地址"是**未实现的意图**，设置面板里没有这一项）。
   想走内网 HTTPS 只能：改 `tauri.conf.json:40` → 重新构建 → 由贵方自己分发这个定制包。
2. **镜像站必须由持有发布私钥的一方维护。** 清单是 minisign 签名过的，客户端只认公钥那一把
   （`tauri.conf.json:38`）。所以"内网镜像"不是把一个目录同步进去就行 —— 要么把签名私钥交给内网发布流程，
   要么客户端不认。这是**信任根问题，不是配置问题**，得由贵机构决定谁持钥、怎么托管。
3. **不做更新也完全可用。** 检查只发生在"关于"面板那一次点击（`src/app.js:4655`）；
   **没有开机自动检查、没有定时器、没有后台拉清单的动作**（`check_update` 的调用点只有那一处），
   因此也没有"关掉自动更新"的开关的必要 —— 它本来就不自动。
   失败的处理同样照实说：只在面板里显示"检查更新失败：…"（`src/app.js:4655-4657`），
   Rust 侧记一行日志（`src-tauri/src/lib.rs:1474-1477`）。
   `design/17-distribution.md:224` 里"连续失败三次就停止询问"的规则**没有计数器实现，不存在**。

## 四、这个程序会连哪些地址（防火墙白名单口径）

| 目的地 | 什么时候 | 出处 |
| --- | --- | --- |
| 用户配置的 IMAP/SMTP 主机（内置服务商表或手填） | 同步、收发、IDLE 推送 | `crates/mail-core/src/providers.rs`、`imap.rs`、`smtp.rs` |
| `accounts.google.com`、`oauth2.googleapis.com`、`mail.google.com` | 仅 OAuth 账号登录/刷新 | `crates/mail-core/src/oauth.rs:258-274` |
| `login.microsoftonline.com`、`outlook.office.com` | 仅 Outlook / M365 OAuth | 同上 |
| `localhost` 回环监听（一次性、独占绑定） | OAuth 收回调 | `oauth.rs`，PKCE S256 + state |
| `github.com`（清单 + releases 页） | 只有用户点了"检查更新"或"打开下载页" | `tauri.conf.json:40`、`src-tauri/src/lib.rs:2447-2452` |
| **邮件正文里引用的任意 `http(s)` 主机** | 只有用户对该封信点了"显示图片"（默认全拦，切信即重置） | `src/app.js:3556-3569`、`:3616-3619` |
| 邮件正文/签名档里的链接目标 | 用户点击并在提示条上确认之后，**由系统浏览器**发起，本程序不直接连 | `src/app.js:3525-3528`、`crates/mail-core/src/links.rs` |
| 各服务商"去开启"页面（`service.mail.qq.com`、`myaccount.google.com` 等 23 个） | 只在设置向导里由用户点击，同样是系统浏览器 | `crates/mail-core/src/providers.rs:234-653` 的 `enable_url` |
| `timestamp.digicert.com`（HTTP） | 只在**构建期**签名参数里，运行时不访问 | `tauri.windows.conf.json:23` |

- **定时动作只有两个，都只连用户自己的邮件服务器**：文件夹的 IDLE 推送监听，和一个**打开着的写信窗口**
  每 30 秒把草稿镜像到服务器（`src/app.js:4000-4012`、`invoke('mirror_draft')` `:4233`）。写信窗口关掉
  就停（`:4020-4021`）。全应用只有两处 `setInterval`：这一处，和 `:347` 的撤销倒计时（纯本地、不联网）。
- **渲染出的邮件内容拿不到网络**：正文 iframe 的 `sandbox` 属性**不带任何值**（脚本、同源、表单、顶层导航
  全禁），并且 `referrerpolicy="no-referrer"`（`src/index.html:258-263`）；帧内自己的 meta CSP 是
  `default-src 'none'`，未获用户许可时 `img-src data: blob:`（`src/app.js:3616-3626`）；外层壳的
  `connect-src 'self' ipc: http://ipc.localhost` 挡住页面对任意主机的 `fetch`/XHR
  （`src-tauri/tauri.conf.json:24`）；`<a href>` 在净化阶段被改写成 `data-ee-href`，链接只能经
  `open_link` 交给系统浏览器（`crates/mail-core/src/sanitize.rs:352,411`）。
  **唯一的主动出网口就是上面那行"用户点了显示图片"。**
- **遥测 / 崩溃上报 / 统计：不存在。** 全库 grep `telemetry|analytics|sentry|posthog` 在
  `crates/`、`src/`、`src-tauri/` 里零命中；`design/14-security-privacy.md:299` 写的就是"没有崩溃上报"。
- **自动配置发现（ISPDB / SRV / Autodiscover / WKD / keys.openpgp.org）：不存在**，README 的路线图里排在 v0.3。
  这一条对甲方有意义：程序不会因为"帮你找服务器"去连任何第三方地址。
- `design/14-security-privacy.md:72,165,282,288-299` 描述的 `NetPurpose` / `NetAudit`（端点清单与代码一致性自动验证）
  **目前只在设计文档里，代码与门禁中都没有**（`tools/check.ps1` 里与更新有关的步骤只有两步：
  `:376-418` 查签名链与公钥是内容不是路径，`:448` 查界面版本号取自清单）。
  所以 §14 验收清单那条"端点清单与 `NetPurpose` 一致"仍未完成，本表的口径是**人工按代码整理**的。

## 五、送审材料：已经能给的，和还缺的

**已经能给的**：

- **诊断包**：先在界面预览、确认后才写盘（`src-tauri/src/lib.rs:2721-2761` 预览 / `:2768-2811` 写，
  UI 走"预览 → 确认 → 写"两步，`src/app.js:4301-4325`）。内容是最近 3 天日志、脱敏后的账号与设置、
  以及**可选**的 8 封收件箱摘要（只有头、结构上就没有正文字段，`crates/mail-core/src/diag.rs:97-129`）；
  每一部分都过 `redact()`（`diag.rs:146-148`）。落点是 `%APPDATA%\com.easyemail.desktop\easy-email-diagnostics-<unix>.txt`。
  已知毛病：清单里的 `webview` 字段硬编码成 `"unknown"`（`lib.rs:2746,2791`）—— 送审前要么修，要么注明。
- **每个产物的 SHA256**（CI 生成 `SHA256SUMS`，`release.yml:80-85,192-196`；README `:62,198`）。
- **依赖锁定**：`Cargo.lock` 入库。
- **第三方许可清单**：`THIRD-PARTY-NOTICES.md` 与 `reports/licensing.md`（后者今天的结论是
  **NO —— 现在不该按 MIT 发布**，这条对甲方合规审查同样是要紧的诚实话）。
- **磁盘加密的部署记录**：`tools/disk-protection-check.ps1` 提权运行，退出码 0/1/2 = 前置条件成立/不成立/无法判定
  （第 101 轮，2b）。应用自己那份启动自检只读同一批数字，非提权时照实显示"无法判定"（`src-tauri/src/diskprotect.rs`）。

**缺的（不要假装存在）**：

1. **安装包代码签名**（§2.2）。甲方 IT 通常把这当成硬性门槛。
2. **离线构建能力**：仓库里**没有** `.cargo/config.toml`、**没有** `vendor` 目录（2026-10-20 `ls` 四个可能位置
   均不存在）。这台开发机能构建，靠的是**用户目录下的** `C:\Users\<用户>\.cargo\config.toml`：
   它把 `crates-io` 换成了清华 TUNA 的 sparse 镜像，还设了 `RUSTUP_DIST_SERVER` 指向同一台镜像站。
   ⇒ **换一台只有内网源、或者一个用户配置的机器，构建就未必能过**；要在内网复现构建，得先做一份完整
   `cargo vendor` 包并把这份配置随源码交付。
3. **SBOM**：不存在。
4. **可复现构建说明**：不存在。
5. **第三方安全审计**：不存在（`SECURITY.md` 的 Known Limitations 里明写）。
6. **企业策略分发**：没有 MSIX/APPX、没有 Intune/SCCM 包、没有静默安装的机器策略模板 —— 都不存在。
7. **日志对外投递**（syslog / SIEM 对接）：不存在，诊断包是"人工生成的一份文本"。

## 六、要往下推的话，顺序是这样的（不含日期承诺）

1. 先解决 §2.3 的 WebView2：要么在部署单里写明"目标机需预装 WebView2 运行时"（一句话，零代码），
   要么做一次 `embedBootstrapper` 构建实测包体与安装耗时。
2. 再决定更新的信任根：谁持 minisign 私钥、内网发布是否要做。想清楚之前，把"关于面板里手动检查"当成
   默认关闭的功能对待（它本来就不自动）。
3. 然后才是签名、vendor 离线构建、SBOM 这三件纯工程活 —— 它们的共同点是**工作量可估、风险低、
   但都需要一次真实的构建环境改造**，不该和代码功能混在一轮里做。
4. `endpoint_override`（把清单地址变成运行时可配）只在第 2 条有结论之后再动：地址能改不等于允许改，
   这一步动的是信任根。
