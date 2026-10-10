# Easy Email

[![Release](https://img.shields.io/github/v/release/hq20211124/easy-email?style=flat-square)](https://github.com/hq20211124/easy-email/releases/latest)
[![CI](https://img.shields.io/github/actions/workflow/status/hq20211124/easy-email/release.yml?style=flat-square&label=build)](https://github.com/hq20211124/easy-email/actions)
[![License: MIT](https://img.shields.io/badge/license-MIT-green?style=flat-square)](LICENSE)
[![Windows](https://img.shields.io/badge/Windows-0078D6?style=flat-square&logo=windows&logoColor=white)](#安装)
[![Linux](https://img.shields.io/badge/Linux-FCC624?style=flat-square&logo=linux&logoColor=black)](#安装)

轻量级**跨平台**邮件桌面客户端，主要目标是 Windows，Linux 亦可编译与运行。**用户只需要填邮箱地址和授权码 —— 服务器地址、端口、加密方式全部内置。**

技术栈：Tauri v2 + Rust。没有 Node 构建步骤，前端是静态 HTML/JS，整个项目一次 `cargo build` 搞定。

---

## 当前进度（v0.1.2 已发布：内部计划“v0.2”的全部六块已交付）

> 版本号口径：里程碑内部叫 v0.2， tag 是 `v0.1.2`（2026-10-10 发布，四类安装包 + 签名 + 更新清单齐全）。

已完成：**单账号收发闭环** · 经过真实服务器验证的内置邮箱配置库 · **本地缓存与离线可读** · **本地全文搜索**。

| 能力 | 状态 |
|---|---|
| 内置 21 个主流国内外邮箱配置（含授权码引导与 quirks） | ✅ 已用真机 TCP+TLS+协议握手验证 |
| 按邮箱地址自动识别服务商 | ✅ |
| 文件夹名本地化（收件箱/草稿箱/已发送/垃圾邮件/已删除）+ 按习惯排序 | ✅ RFC 6154 属性优先，英文名回退 |
| IMAP：文件夹列表、邮件列表、读正文（含附件元数据） | ✅ |
| SMTP：发信、回复、转发（含正确的会话头） | ✅ |
| 删除、标为已读/未读、移动到文件夹 | ✅ |
| 账号保存 + 密码存入 Windows 凭据管理器 | ✅ |
| 添加账号时用真实连接验证 IMAP + SMTP | ✅ |
| 安全渲染邮件正文（沙箱 iframe，默认拦截远程图片） | ✅ 含"显示图片"按封邮件解锁 |
| 浅色 / 深色主题切换（默认浅色，记住选择） | ✅ |
| **SQLite 本地缓存 + 增量同步（UIDVALIDITY 游标）** | ✅ 真机验证：149 封首次、二次 0 封 |
| **关于面板**：当前版本 + 检查并安装更新（minisign 签名校验） | ✅ 版本读 CARGO_PKG_VERSION |
| **失败可见性**：按失败类型的横幅（离线/授权码/配额/本地）+ 重试 | ✅ 判据来自后端 failure_kind |
| **断网可读**：列表、正文、文件夹列表都来自本地 | ✅ |
| **本地全文搜索（FTS5，中文可用）** | ✅ 命令行 200 条命中上限 |
| **正文全文索引（后台有界回填 + 覆盖率可见）** | ✅ 每 60 秒一遍、每遍 20 封 |
| **附件名进索引**（中文文件名与 `pdf` 之类扩展名都能搜到） | ✅ 追加在正文截断之后；v0.1.2 之前索引过的邮件本地补齐，不联网 |
| **自动更新（minisign 签名；四类安装包各有清单条目）** | ✅ 代码完成；真机待下一次 tag |
| **连接池**：复用 IMAP 连接，不再每条命令握一次手 | ✅ 坏连接自动丢弃并重试一次 |
| **IMAP IDLE 推送**：新邮件自动出现，不用轮询 | ✅ 探针证实 `NewData` 约 0.2 s 到达（`probe_idle_delivers_exists_before_the_window_closes`）；真机自发一封 20 秒内被推送 |
| **系统托盘**：菜单（打开主窗口/立即检查/退出）+ 未读数提示 | ✅ 图标创建成功，显示位置取决于 Windows |
| **Windows 通知** | ✅ 由 Rust 侧调用 |
| **附件下载**：点附件标签即存入"下载 / Easy Email"文件夹 | ✅ 真机验证：真实 PDF 存下且头部是 `%PDF-` |
| **附件上传**：写信时可添加本地文件 | ✅ 真机验证：40KB 经真实 SMTP 发出、IMAP 取回逐字节一致 |
| 历史邮件回填（缓存最新 500 封以外的部分） | ✅ `backfill.rs` +「加载更多」先给缓存 |
| OAuth2 登录（Outlook / Microsoft 365） | ⚠️ 代码就绪、端点实测可达；**未在真实 Outlook 账号上走通**（见「真实账号手工验证」） |
| 自定义 / 自建服务器（手动填 IMAP/SMTP） | ✅ 真机探针验证：错的输入被正确拒绝、拒绝时不留半成品账号；**未对着真实自建服务器登录过** |
| 自动发现（ISPDB / SRV / Autodiscover） | ❌ v0.3 |

---

## 安装

装好的包在 [Releases](https://github.com/hq20211124/easy-email/releases/latest) 页面，
四类产物由 CI 自动构建并附带 `SHA256SUMS`。

| 平台 | 下载 | 安装 |
|---|---|---|
| Windows | `Easy Email_x64-setup.exe` | 双击安装（需要系统有 WebView2 运行时，Win10/11 一般自带；安装时按需联网下载） |
| Debian / Ubuntu | `Easy Email_0.1.2_amd64.deb` | `sudo dpkg -i Easy\ Email_0.1.2_amd64.deb`（依赖会自动提示） |
| Fedora / RHEL | `Easy Email-0.1.2-1.x86_64.rpm` | `sudo dnf install ./Easy\ Email-0.1.2-1.x86_64.rpm` |
| 任意 Linux（免安装） | `Easy Email_0.1.2_amd64.AppImage` | `chmod +x Easy\ Email_0.1.2_amd64.AppImage && ./Easy\ Email_0.1.2_amd64.AppImage` |

> 文件名里的版本号以 [Releases](https://github.com/hq20211124/easy-email/releases/latest) 页的实际资产为准；
> 另外注意 CI 上传时空格会被换成点（`Easy Email` → `Easy.Email`），所以**资产名**可能和上面本地构建名不同。

Linux 版凭据走 D-Bus Secret Service（gnome-keyring / KWallet）；没有密钥守护进程时应用会明确
报错而不是硬崩，刷新令牌以"机器标识派生密钥加密 + `0600` 权限"的本地文件存放。

首次添加账号：填邮箱地址 → 服务商与服务器自动识别 → 填**授权码**（不是登录密码）→ 应用先做一次
真实连接验证再保存。Gmail 与 Outlook/Microsoft 365 只支持 OAuth2，需要在向导里填**你自己注册的**
客户端 ID（微软没有授权码这东西）。

---

## 从源码构建

需要 Rust 1.90+（本项目使用 edition 2024；下限由自动更新插件 `tauri-plugin-updater` 的官方要求决定）。
前端是静态 HTML/JS，没有 Node 构建步骤。

**Windows**：装 VS 2022 Build Tools + WebView2 Runtime，然后 `cargo run -p easy-email`。

**Linux**（含 WSL）：除 Rust 工具链外还需要

```bash
sudo apt install build-essential pkg-config libwebkit2gtk-4.1-dev libsoup-3.0-dev \
  libgtk-3-dev librsvg2-dev libayatana-appindicator3-dev libdbus-1-dev
```

没有图形会话时（例如 WSL 默认会话）用 `dbus-run-session -- ./target/debug/easy-email` 起
桌面总线和托盘，否则托盘/密钥环会报告"没有 session bus"。Linux 打包：

```bash
cargo tauri build    # deb / AppImage / rpm；Windows 上为 nsis
```

> **打包需要签名私钥**（`createUpdaterArtifacts` 开启后 bundler 必须产出 `.sig`）。
> 本地打包时把密钥指给它，否则会停在签名那一步：
>
> ```bash
> export TAURI_SIGNING_PRIVATE_KEY="$HOME/.tauri/easy-email.key"
> export TAURI_SIGNING_PRIVATE_KEY_PASSWORD='你生成密钥时设的密码'
> cargo tauri build
> ```
>
> `cargo build` / `cargo run` / `tools/check.ps1` **不受影响**（那只编译，不打安装包），
> 只有走到 bundler 那一步才要密钥。真正的发布也不要本地打 —— CI 里有"密钥缺失就说人话"
> 的前置检查和三个 job 的清单合并，本地没有这些。

**没有控制台窗口。** exe 现在是 GUI 子系统的（Tauri 模板默认只有 release 才是，于是每次双击 debug
exe 都会跟一个 cmd 窗口）。要看启动期的输出：

```powershell
# 挂回当前终端（UTF-8，中文不会乱码）
.\target\debug\easy-email.exe --console

# 或者重定向 —— GUI 子系统进程会继承调用者给的句柄，不需要 --console
.\target\debug\easy-email.exe > target\app.log 2>&1
```

**panic 会写进 `%APPDATA%\com.easyemail.desktop\startup-error.log`**，所以"窗口出来了但一片空白、
又没有任何输出"这种情况不会无迹可寻。`tools/check.ps1` 里有一项直接读 PE 头的 Subsystem 字段
来防止这个设置被改回去。

改完 `src/` 下的文件**必须重新 `cargo build`**：前端资源在编译期嵌进二进制，运行时不会再读磁盘。
（实测：把 `src/theme.js` 改名移走、不重新构建直接启动已构建的 exe，界面仍然完全正常 ——
如果它读磁盘，缺文件会让 `boot()` 抛错、邮箱列表根本不会出现。）

提交前跑一遍全部检查（前端语法、元素 id 引用、fmt、clippy、单测）：

```powershell
.\tools\check.ps1              # 离线，约 30 秒
.\tools\check.ps1 -Network     # 额外对着真实服务器验证内置配置库
```

单独跑某一项：

```powershell
node --check src/app.js        # 前端语法
cargo test --workspace         # 单元测试（不需要网络）
cargo test -p mail-core --test provider_reachability -- --ignored --nocapture
```

重新生成图标：

```powershell
python tools/make_icon.py
```

抓一张运行中的窗口截图（调试 UI 用）：

```powershell
.\tools\capture-window.ps1     # 输出 target\screenshot.png
```

---

## 发版（维护者）

打 tag 会自动构建四类安装包并上传到 Release（`.github/workflows/release.yml`），但 CI **不会**替你
改版本号——产物文件名里的版本来自 `src-tauri/tauri.conf.json` 的 `version` 和 workspace `Cargo.toml`，
与 git tag 无关。忘了改的后果是每个版本都产出 `Easy Email_0.1.0_…`，Release 页上无法区分（v0.1.1
第一次就是这么发错的）。

```bash
# 1. 升版本号（两处保持一致）
sed -i 's/^version = .*/version = "X.Y.Z"/' Cargo.toml
sed -i 's/"version": ".*"/"version": "X.Y.Z"/' src-tauri/tauri.conf.json

# 2. 提交并推送
git commit -am "发布 X.Y.Z" && git push

# 3. 打 tag 并推送（触发 CI 构建与上传）
git tag -a vX.Y.Z -m "…" && git push origin vX.Y.Z
```

**第 4 步不是 `gh release create`。** CI 的 `softprops/action-gh-release` 在上传资产时已经把
release 建好了，再 `create` 会报 `release already exists`。release 建好之后只差标题、说明和
latest 标记，用 `edit`：

```bash
# 4. 补标题与发布说明，并置为 latest
gh release edit vX.Y.Z --title "Easy Email X.Y.Z（主题）" --notes-file notes.md --latest
```

> 想在打 tag **之前**就把说明写好也可以：先 `gh release create vX.Y.Z --draft …` 建一个草稿，
> CI 会把资产传进去，最后 `gh release edit vX.Y.Z --draft=false --latest` 发布。
> 但注意草稿 release 不会出现在 `releases/latest` 里，**自动更新会认为没有新版**
> （插件的 `latest` 端点只解析非草稿、非预发布的 release）。

产物带 `SHA256SUMS`；deb/rpm 的运行时依赖声明在 `src-tauri/tauri.linux.conf.json`。

### 签名与更新的现状（发版前必读）

- **安装包未签名**（Authenticode）。后果：Windows 首次运行会弹 SmartScreen
  "Windows 保护了你的电脑"——点「更多信息 → 仍要运行」即可。这不影响自动更新的安全性：
  更新包的完整性由 minisign 签名保证，与 Authenticode 是两件事（`design/17` §6）。
- **自动更新按安装包类型分别提供**：`windows-x86_64`（NSIS）、`linux-x86_64-deb`、
  `linux-x86_64-rpm`、`linux-x86_64-appimage` 各一条（`tauri-plugin-updater` 的
  `Installer` 变体就是这四种，target 串形如 `linux-x86_64-deb`）。
  AppImage 的更新是"整份替换"：它下载新的 AppImage 并替换自己，
  **不会**去动系统的 deb/rpm 数据库，两者互不干扰。
- 更新清单 `latest.json` 由 CI 在同一 tag 上生成并上传，
  内容 = `version` / `pub_date` / 上述四个 platform 各自的 `{url, signature}`。
  **清单缺失时更新检查静默失败**（离线或墙内都不打扰，连续失败 3 次才在关于面板里提一句）。

---

## 目录结构

```
crates/mail-core/          协议层：不含任何 UI，可以单独测试
  src/providers.rs         内置邮箱配置库（21 个服务商 + quirks + 授权码引导）
  src/imap.rs              IMAP：连接、文件夹、列表、读信、删除、移动
  src/smtp.rs              SMTP：组信与发信（含 In-Reply-To / References）
  src/secret.rs            Windows 凭据管理器封装
  src/error.rs             错误分类：把 AUTHENTICATIONFAILED 变成可操作的中文提示
  src/model.rs             与 UI 共享的可序列化类型
  src/cache.rs             本地 SQLite 缓存 + FTS5 搜索索引
  src/index.rs             后台有界回填：把没打开过的正文加进索引
  src/userdb.rs            用户自己的库：草稿、发件箱、同步窗口、配额账本
  src/worker.rs            发件箱 worker（一遍有界的出队）
  tests/loopback.rs        进程内假 IMAP/SMTP 服务器，驱动真实客户端跑完整收发闭环
  tests/provider_reachability.rs   对着真实服务器验证配置库
src-tauri/                 Tauri 外壳
  src/lib.rs               55 个 Tauri 命令（前端调用 49 个）+ 状态管理 + 后台 worker
  src/store.rs             账号持久化（原子写入 + 损坏文件自动备份）
src/                       前端（静态 HTML/JS/CSS，无需打包）
tools/make_icon.py         生成多分辨率 .ico
```

## 两层验证，各管一件事

| 测试 | 证明什么 | 需要凭据吗 |
|---|---|---|
| `tests/loopback.rs` | 客户端的协议交互是对的：问候语握手、`ID` 在 `LOGIN` 之前、`UID FETCH` 分帧、附件与 GB2312 解码、SMTP 认证与发信头。有 50 个用例，包括"服务器不支持 UIDPLUS"和"密码被拒"两条错误路径 | 不需要 |
| `tests/provider_reachability.rs` | 内置配置库指向的 40 个端点是真的存在、端口和加密方式没写错 | 不需要 |

两层都不需要真实邮箱，所以 CI 里能全跑。`loopback` 用的是一个只实现必要协议子集的进程内服务器 ——
这已足够覆盖客户端自己的行为，也正是它第一次运行时抓到下面第 4 个 bug。

**第三层：跑在真实窗口上的 CDP 探针**（`tools/inspect-dom.mjs` + `tools/probe-*.mjs`）。
它驱动的是真正在跑的那个界面，所以能验证"两个纯函数各自都对、接起来却不对"这类问题
（`probe-oauth-wizard.mjs` 就是为一个"按钮可见但没有事件"的 bug 写的）。
用法与开关见 `HANDOFF.md`：要先临时把 `--remote-debugging-port=9222` 加回配置并重建，**验完必须删掉**
（`check.ps1` 会拦）。

---

## 真实账号手工验证

有些东西只能用人自己的账号点一遍。目前**没验过**的是下面这条，步骤写全是为了下一次不用再想：

### Outlook / Microsoft 365（OAuth2）

1. 到 <https://portal.azure.com> → **Microsoft Entra ID** → **应用注册** → **新注册**。
   名称随意（会显示在授权页上）；「受支持的帐户类型」选**「任何组织目录中的帐户和个人 Microsoft 帐户」**
   （个人 outlook.com 与单位 M365 都要能登）；「重定向 URI」平台选**「移动和桌面应用程序」**，
   地址填 **`http://localhost`**。**端口不用写** —— Azure 对桌面平台忽略端口，
   而且它**不接受 `http://127.0.0.1`**（2026-10-04 实测；官方文档也写明门户的文本框不接受 http 的回环
   IP 字面量，要加只能改应用清单）。**不需要客户端密码**：桌面应用是公共客户端，用 PKCE。

   > **进不去门户的话**：如果看到
   > `AADSTS16000 … 用户帐户在租户"Microsoft Services"中不存在 … 应用 74658136-14ec-4630-ad9b-26e160ff0fc6`，
   > 那个应用是 **Azure 门户自己**（内部代号 Ibiza），报错来自**门户的登录层**：含义是"你登录用的账号
   > 不是这个目录的成员 / 目录选错了 / 浏览器会话里是另一个账号"。用**创建该目录的那个账号**登录；
   > 门户右上角把**目录**切对；或者开一个 InPrivate 窗口重新登录。**这与本程序无关。**
2. 复制**应用程序(客户端) ID**。应用 → **添加邮箱账号** → 填微软账号地址 → 服务商选
   **Outlook / Microsoft 365** → OAuth 路径 → 展开「高级」→ 粘贴客户端 ID，**密钥留空**。
3. 点**「打开浏览器登录」** → 在浏览器里登录并点「允许」。
   - 授权页上的应用名应该是你注册时填的名字；回到应用后状态框显示"✓ 已授权…"。
   - `startup-error.log` 里应出现 **`OAuth 授权账号已核对（与填写的地址一致）`**。
     若出现 `服务商没有返回可用的 id_token`，说明微软没回 `id_token`，值得单独记一笔。
4. 点**「验证并保存」**，看报告里的 IMAP / SMTP 两行：
   - 两行都成功 ⇒ 完整可用，左侧出现账号，点开能看到 `Inbox/Sent Items/…` 文件夹列表；
   - **IMAP 成功、SMTP 被拒** ⇒ 这就是 M365 关闭 SMTP AUTH 的场景。预期是账号**被保存**，
     提示"可以收信，但服务器拒绝了发信 —— …"，并且文案说的是**去管理中心开 SMTP AUTH**，
     不是让你去生成授权码（M365 没有那个东西）；
   - IMAP 也失败 ⇒ 把报告里 `imap_detail` 的原文发回来，别猜。

### 自建 / 表外服务器（自定义端点）

向导里选「自定义 / 自建服务器」，按服务商文档填 IMAP/SMTP 主机、端口与加密方式
（绝大多数：IMAP 993 直接 TLS、SMTP 465 直接 TLS 或 587 STARTTLS）。
主机名只填主机名 —— 端口写进右边的「端口」框；IPv6 写成 `[::1]`。
**「不加密」只允许本机地址**：它意味着密码明文上线，所以对外部服务器会被拒绝。

---

## 内置配置库

数据在 `crates/mail-core/src/providers.rs`，21 个服务商：

**国内**：QQ 邮箱/Foxmail、网易 163、网易 126、网易 yeah.net、腾讯企业邮、网易企业邮、
阿里邮箱（个人版）、阿里企业邮、新浪邮箱、中国移动 139 邮箱

**国际**：Gmail、Outlook/Microsoft 365、Yahoo、iCloud、Zoho、Yandex、AOL、GMX、Fastmail、
Proton Mail（需 Bridge）、Tuta（不支持 IMAP，明确标注）

表里还有**第 22 行 `custom`（自定义 / 自建服务器）**：它不是服务商，而是"服务器地址由你填"这件事的
载体（`manual_endpoints: true`）。它刻意放在**最后**，因为没有域名的它永远不该被自动选中；
界面上显示手动填写区看的是这个标志，**不是**在渲染层里比较 id（`design/05` §10.1）。

每个条目包含服务器地址、端口、加密方式、认证类型、开通授权码的图文引导、跳转链接，
以及 **quirks（服务端脾气）**：

- `needs_id` —— 网易系（163/126/yeah/企业邮）要求客户端先发 RFC 2971 `ID` 命令，
  否则登录后所有命令都被拒。代码在 `LOGIN` 之前自动发送。
- `max_connections` —— QQ 和网易对并发连接限制很严，超了会直接拒连。

### 验证状态

`verified` 标记表示**已经完成真实的 TCP + TLS + 协议握手**（IMAP 收到 `* OK`，SMTP 收到 `220`），
由 `tests/provider_reachability.rs` 执行，所以这个标记是证据而不是乐观。

最近一次全量验证（2026-10-02，40 个端点，耗时 43 秒，全部通过）：

```
qq        IMAP imap.qq.com:993            OK  * OK [CAPABILITY IMAP4 IMAP4rev1 ID AUTH=PLAIN ...
qq        SMTP smtp.qq.com:465            OK  220 ... QQ Mail Server
163       IMAP imap.163.com:993           OK  * OK Coremail System IMap Server Ready
126       IMAP imap.126.com:993           OK  * OK Coremail System IMap Server Ready
yeah      IMAP imap.yeah.net:993          OK  * OK Coremail System IMap Server Ready
exmail    IMAP imap.exmail.qq.com:993     OK
qiye163   IMAP imap.qiye.163.com:993      OK  * OK IMAP4 ready
aliyun    IMAP imap.aliyun.com:993        OK  * OK AliYun IMAP Server Ready
sina      IMAP imap.sina.com:993          OK  * OK [CAPABILITY IMAP4rev1 ID] sina imap server ready
139       IMAP imap.139.com:993           OK  * OK RICHINFO System IMap Server Ready
gmail     IMAP imap.gmail.com:993         OK  * OK Gimap ready for requests from 203.0.113.9（我方公网 IP，已脱敏）
outlook   IMAP outlook.office365.com:993  OK  * OK Microsoft Exchange IMAP4 service ready
yahoo     IMAP imap.mail.yahoo.com:993    OK  * OK Welcome! IMAP Server up and ready
icloud    IMAP imap.mail.me.com:993       OK
zoho / yandex / aol / gmx / fastmail      OK  （IMAP 与 SMTP 全部通过）

gmail     SMTP smtp.gmail.com:465         不可达  连接超时（中国大陆网络，非数据错误）
```

Gmail 的 IMAP 是通的，但 SMTP 的 465 端口从大陆网络经常连不上 —— 这是网络事实，不是配置错误，
所以 `verified` 保持 `true`，同时客户端会把"连不上"当成一种可解释的结果而不是 bug 抛给用户。

只有 Proton（本机 Bridge，无法在 CI 验证）和 Tuta（无 IMAP 端点）保持 `verified: false`。

### 这个测试的判定标准（以及一次踩坑）

它把结论分成三类，并且**只有两类里的第一类会判定失败**：

- **协议不符** —— 连上了、TLS 也握上手了，但对方说的话不是 IMAP/SMTP 该说的话。
  这才是"内置数据错了"的证据，会直接 fail。
- **不可达** —— 连不上、或者 TLS 握手超时。**超时不含任何关于对方的信息**，所以只告警。
- **OK** —— 收到 `* OK` / `220` 问候语。

一开始我把「TLS 握手超时」也归成了"协议不符"，结果一次网络抖动就让整个门禁变红 ——
这跟前面那句「不能把网络问题和数据错误混为一谈」是同一个错误，只是换了一层。
现在超时一律算不可达，并且一个"协议不符"结论会有**第二次机会**（重试一次才判失败），
因为宣布一个端点数据过时是个很强的断言，值得多看一眼。

---

## 三个值得注意的实现细节

这些都是踩过之后才写对的，测试里有对应的回归用例：

1. **mail-parser 默认不认 GB2312/GBK。** 它的 `default = []`，不开 `full_encoding` 特性时
   中文老邮件全部解析成乱码。`Cargo.toml` 里已经打开。

2. **SMTP 组信不能用 `MessageBuilder::body()`。** 它只写 `Content-Transfer-Encoding`，
   完全不写 `Content-Type` —— 中文正文会以 base64 发出但没有 charset 声明，收件人看到的是乱码。
   必须用 `singlepart(SinglePart::plain(..))`。

3. **`references()` 是覆盖而不是追加。** 循环调用会把 References 链截断成最后一个 Message-Id，
   导致回复在对方客户端里不成为会话。必须一次性把整条链用空格拼好传进去。

另外两个库层面的坑：`async-imap` 0.12 **没有 `connect()` 函数**（文档示例是 `ignore` 的，
引用了已删除的函数），必须自己搭 TCP+TLS；而 `Client<T>` 要求 `T: Debug`，
但 `async_native_tls::TlsStream` 故意没实现 `Debug`，所以需要一个 `DebugStream` 包装。

4. **错误分类必须同时用在 `send` 和 `test_connection` 上。** 一开始只有 `send` 走了
   `classify_smtp`，`test_connection` 直接构造 `MailError::Smtp`。而添加账号向导走的是
   `verify_account` → `test_connection` —— 也就是说**用户输错授权码时，最需要"你应该填授权码"
   这句提示的那条路径，恰好是唯一没接上分类的**。

## 四个只有真的把程序跑起来才会发现的 bug

第 1-3 个是截图看界面时抓到的，第 4 个是写集成测试、第一次真正执行客户端协议代码时抓到的。
四个都不会被 `cargo build` 或单元测试发现：

1. **模板字符串里写了反引号。** 我在一段 HTML 注释里用了 `` `off` `` 和 `` `new-password` ``，
   而这段 HTML 本身是 JS 模板字符串 —— 反引号直接把字符串截断了，
   `SyntaxError: Unexpected token 'new'`。整个 `app.js` 拒绝执行，表现是"界面出来了但一片空白"：
   静态 HTML 正常渲染，所有 JS 填充的内容全都没有。
   `cargo build` 全程绿色。现在 `tools/check.ps1` 第一项就是 `node --check`。

2. **`[hidden]` 被自己的 CSS 覆盖了。** `#reader-body { display: flex }` 的优先级高于浏览器
   默认的 `[hidden] { display: none }`，所以 `element.hidden = true` 完全不起作用 ——
   读信面板和模态遮罩永远显示。修法是全局加一条 `[hidden] { display: none !important }`。

3. **三栏布局在小屏/高 DPI 上会把读信栏压成 0。** `grid-template-columns: 232px 340px 1fr`
   在窗口只有 640 逻辑像素宽时，前两栏吃掉 572px，读信栏只剩几十像素，正文完全看不见。
   改成 `minmax()`，让侧栏先让位。

4. **不支持 UIDPLUS 的服务器上，删除会静默失败。**（最严重的一个，数据一致性问题）
   原来靠 `if self.expunge_uid(uid).await.is_err()` 决定要不要回退到 `EXPUNGE`。
   但 `async-imap` 从 0.11.1 起"命令被拒就关流"，被拒的命令可能表现为一个**干净结束的 stream**
   （0 个元素、没有 `Err`），于是 `try_collect` 返回 `Ok(vec![])`、`.is_err()` 是 `false` ——
   不回退，直接返回 `Ok(())`。
   用户看到的是：点了删除，邮件从列表消失，但服务器上只是被打上了 `\Deleted` 标记、永远没清掉。
   现在改成登录后用 `capabilities().has_str("UIDPLUS")` **一次性决定策略**，
   而不是先试错再回退 —— 试错本身会触发 async-imap 关流，所以是双重错误。

顺带还发现 WebView2 的自动填充会往"显示名称"里塞 Windows 账户名，
已经用 `autocomplete` 属性加 `additionalBrowserArgs` 关掉。

## 第一次用真实邮箱跑通时发现的三个问题

QQ 邮箱真实账号（QQ 号地址 + 16 位授权码）登录成功之后，界面立刻暴露了两个问题；
第三个是排查登录失败时发现的。

5. **文件夹名没有从 IMAP modified UTF-7 解码。** 中文文件夹在协议里不是 UTF-8：
   可打印 ASCII 原样传，`&` 写成 `&-`，其余字符是 UTF-16BE 走一套「用 `,` 代替 `/`、
   不带 padding」的 base64，夹在 `&`…`-` 之间。结果 `其他文件夹/QQ邮件订阅` 在界面上显示成
   `&UXZO1mWHTvZZOQ-/QQ...`。修法是新增 `src/mutf7.rs` 解码，并且让 `MailboxInfo` **同时带两个名字**：
   `name` 是线路上的原始编码（所有命令必须原样发回去），`display_name` 才是给人看的。
   只解码、不回编码，是为了避免在命令路径上引入新的出错可能。

   这里还踩了一个自己挖的坑：我最初用 `if name.is_ascii() { 不解码 }` 来省一次分配 ——
   但**编码后的名字本身就是纯 ASCII**，这个判断会把所有真正需要解码的名字全部跳过。
   正确的判断是"含不含 `&`"。测试里专门留了一个用例把这个错误钉死。

6. **登录名和邮箱地址被硬绑在一起。** `AccountConfig` 本来就有独立的 `username` 字段，
   但向导从来没让用户填过，永远等于邮箱地址。而有些账号的登录名和地址确实不一样 ——
   `@foxmail.com` 别名、VIP 账号、企业下发的单独登录名。
   现在向导多了「登录用户名（可选）」一栏，留空就还是用地址；「检测结果」里会显示最终用哪个名字登录。

7. **错误提示里的 async-imap 包装层直接暴露给了用户。** 服务器拒绝命令时，
   async-imap 渲染成 `no response: code: None, info: Some("...")`，用户看到的是这坨，
   而服务器真正写的那句话（QQ 的还带帮助链接）被埋在 `Some("` 里面，
   再加上没有自动换行，右半边直接被裁掉。现在会把这层剥掉、并且 `overflow-wrap: anywhere`。

---

## 只有真的点开一封真实邮件才会暴露的三个问题

这三个不在 `cargo build`、单元测试、甚至 loopback 集成测试的覆盖范围内 —— 它们只在真的把一封
HTML 邮件渲染进 WebView2 时才出现。第 8 个是靠 CDP 探针进入沙箱 iframe 读 `img.naturalWidth`
才定位的，第 10 个是对着真实邮件逐个候选注入 CSS 测出来的（正文在 `sandbox` iframe 里，
应用自己的 JS 碰不到它）。

8. **"显示图片"按钮从来没生效过 —— `srcdoc` iframe 会继承父文档的 CSP。**（本轮最严重的一个）

   现象很有迷惑性：点了"显示图片"，iframe 自己的 CSP 确实变成了
   `img-src data: blob: https: http:`（探针读 `srcdoc` 确认过），提示条也正确地消失了，
   但 14 张图**全部** `complete: true` 且 `naturalWidth: 0` —— 加载失败，不是还在下载。
   同样的 URL 用 PowerShell 请求返回 200，所以不是网络问题。

   根因：**通过 `srcdoc` 加载的文档会继承嵌入它的文档的 CSP，两条策略取交集。**
   外壳那条 `img-src 'self' data:`（`index.html` 的 meta 和 `tauri.conf.json` 的 `security.csp`
   各有一条）因此成了真正的约束 —— 无论里层策略放多宽，远程图片都过不去。而 `data:` 图片能正常显示，
   正因为它在外壳的允许列表里。

   修法是让外壳不再成为约束：两处 `img-src` 都加上 `https: http:`。
   **默认拦截并没有被削弱** —— 实际把关的是 iframe 自己那条策略，它的默认值仍是
   `img-src data: blob:`，只有用户为这一封邮件点过"显示图片"才会变成宽松的。
   `tools/check.ps1` 现在把这个不变量钉住了（外壳必须允许 https，且
   `allowRemoteImages` 三元分支必须仍然选择宽松策略）。

   顺带一个说明性细节：这封 uSMART 邮件里有一个 `naturalWidth: 1` 的 1×1 图片，
   来自 `hz.yxzq.com/news-shorturl/api/v1/images/MTY5NTM6...`，而那段 base64 解出来是
   `16953:reader@example.com:usmart:0101.jpg` —— 它就是追踪像素。
   用户点"显示图片"的那一刻，它就会上报"这封邮件被打开了"。这正是默认拦截存在的理由。

9. **读信头部里发件人和日期会叠在一起。** 长地址把发件人名挤成 54px 宽、折成三行，
   而地址块的右边缘（892）压到了日期的左边缘（881）—— **重叠 11px**。
   这不是肉眼判断的，是 CDP 量 `getBoundingClientRect()` 量出来的。

   根因：`#reader-from-addr` 没有 `min-width: 0`。flex item 的自动最小尺寸是它的
   min-content 宽度，所以那一整串不可断的地址把 `.reader-from` 撑出了容器，直接压到日期上。
   修法是给会被裁的部分加 `min-width: 0` + 省略号，并让 `.reader-meta` / `.reader-from`
   可以换行 —— 现在地址太长时日期会自己落到第二行，而不是互相覆盖。

10. **邮件是固定宽度的桌面布局，塞进窄读信栏会被裁掉。** 真实广发银行那封：内容宽 752px 落在
   406px 的读信栏里，**142 个元素越过右边缘** —— 横幅右半截被切掉，底部出现横向滚动条。
   宽度是写在 `<td style="width:720px">` 和 `width="720px"` 里的，而 `table { max-width: 100% }`
   **管不住它**：表格的实际宽度由单元格的 min-content 撑着，加不加 `max-width` 都是 720px。

   修法是中和固定像素宽度，而中和内联样式必须用 `!important` —— 风险也正在这里。**一刀切地
   `width: auto !important` 会把 `<table width="100%">` 压成读信栏的 6%**，那会毁掉正常邮件的排版。
   所以规则集显式排除百分比形式：

   ```css
   table[width]:not([width$="%"]), td[width]:not([width$="%"]), /* … */ { width: auto !important; }
   table[style*="width:"]:not([style*="%"]), /* … */              { width: auto !important; }
   table, td, th, div, p { max-width: 100% !important; }
   img { max-width: 100% !important; height: auto !important; }
   ```

   这不是推理出来的，是对着**真实那封邮件**逐个候选注入测量出来的（752 → 406，零溢出），
   同时用一个合成夹具确认 `width="100%"` 仍是 1.00、`width="50%"` 仍是 0.50。
   `check.ps1` 把这个不变量钉住了。

   同一轮还测出**不要给图片加高度上限**：800×3765 的竖版横幅限到 420px 高之后，
   因为等比缩放会变成 **89px 宽的一条细缝**，比不限还糟。所以只限宽度。

   顺带：上面第 1 条那个"模板字符串里写反引号"的坑**又踩了一次** —— 这次的 CSS 注释里用了
   `` `width: auto` ``，同样让整个 `app.js` 解析失败、界面看起来一片空白，而 `cargo build` 全绿。
   区别在于这次是 `node --check` 立刻定位到的（我改完没先跑门禁就构建，所以第一眼看到的是"界面空白"）。
   门禁第一项存在的意义就在这里。

---

## 本地缓存与搜索（v0.2）

`%APPDATA%\com.easyemail.desktop\cache.db`（SQLite，紧挨着 `accounts.json`，不想要就直接删）。
缓存里**没有任何密码或授权码** —— 那些只在 Windows 凭据管理器里。

### 增量同步靠 `UIDVALIDITY` + UID 游标

* **首次**同步取最新 500 封的头部（不是从 UID 1 全量拉 —— 两万封的文件夹会让首屏不可用）。
* **之后**每次同步就是 `UID FETCH <last_uid+1>:*`，一次往返，与文件夹大小无关。
* 游标是 `(UIDVALIDITY, last_uid)` 的组合。**`UIDVALIDITY` 一变，缓存里所有 UID 都可能指向别的邮件**，
  所以整个文件夹会被清空重建，而不是拿旧头部去配新邮件。

真实账号实测（QQ，149 封）：

```
第一次：同步 INBOX：新增 149 封，缓存共 149 封
第二次：同步 INBOX：新增 0 封，缓存共 149 封
```

### 搜索能搜到什么，搜不到什么

搜**全部已同步邮件的主题、发件人、收件人**，加上**正文已被索引的邮件的正文**，
再加上**附件名**（`报告.pdf` 这种：中文文件名和 `pdf` 这样的扩展名都能搜）。
附件**文本**（PDF/Word 里面的字）不做 —— 那是 `design/09` §11/Q3 里 M2 的开关，默认关。

正文不在同步范围内：同步只取头部，取一封正文的代价和打开它一样。所以正文索引是一个
**独立的后台任务**（`crates/mail-core/src/index.rs`），每 60 秒跑一遍、每遍最多 20 封，
专门去取"已同步但你没打开过"的邮件；搜索状态行会显示 **「正文已索引 x/y 封」**
（分子是已索引、分母是已同步），旁边一个「现在索引」按钮可以立刻再跑一遍。
这样"搜不到"有两种含义时界面说得清是哪一种：**邮箱里没有**，还是**还没索引到**。

> 索引进去的正文会截断到 64K 字符（`design/09` 的默认值）。**截断只发生在索引那一份** ——
> `bodies` 表里存的仍是全文，因为那是你要读的东西，截了就是丢邮件。这条由
> `a_long_body_is_truncated_for_the_index_but_kept_whole_for_reading` 和 `check.ps1`
> 的两项新检查共同钉住。
> **附件名接在截断之后**，不是之前：一封 64K 的长信不该把文件名挤出索引，
> 而文件名恰恰是长会话里最可能被搜的那几个字（`a_truncated_body_does_not_take_the_attachment_names_with_it`）。

> **从 v0.1.2 升级来的邮件也会拿到附件名索引，而且是本地的。** 每条已存正文记着自己是由哪一版
> 索引写成的（`index_version`）；落后于当前版本的行会重新进待办遍，但补齐动作只是从
> `bodies` 表里已经存着的正文重写一次索引行 —— **不重新取件、不占连接、不花配额**
> （`a_row_indexed_before_attachment_names_existed_is_reindexed_locally_and_never_fetched` 断言
> 那个补齐遍一次服务器都不碰）。待办归零后，一切与新装的版本完全一致。

搜索完全走本地，**断网可用**，也不会因为服务器慢而卡住。

### 中文搜索为什么不能用官方推荐的 `trigram`

FTS5 的默认分词器 `unicode61` 把连续的汉字当成**一个** token，所以搜「账单」在
「这是您十月的账单」里命中 0。所有资料都会推荐改用 `trigram` 分词器 —— **这里它也是错的**：
trigram 要求至少 3 个字符，而中文词大量是两个字。三条路都实测过：

| 方案 | `账单`（2 字） | `invoi` | `2724` |
|---|---|---|---|
| `unicode61` 默认 | ❌ 0 命中 | — | — |
| `trigram` | ❌ 0 命中 | ✅ | ✅ |
| **逐字符空格化 + `unicode61`** | ✅ | ✅ | ✅ |

所以索引侧和查询侧都做逐字符空格化（`to_tokens` / `to_query`，见 `cache.rs`）。
**两边必须用同一套变换**，这一点由两个单元测试钉住，`check.ps1` 还有一项专门拦 trigram 回归。

---

## 连接池与推送（v0.2）

### 连接池：每个账号最多 `max_connections` 条空闲连接

以前**每条命令都新建一次连接** —— TCP + TLS + LOGIN，几百毫秒一次，而且 `max_connections`
从来没有被真正执行过。现在空闲连接会被复用，`list_messages` 这类操作只花一次往返。

两个细节值得写下来：

* **复用前会发一条 `NOOP` 验活**，因为服务器会悄悄关掉空闲连接，而 IMAP 里这不会表现为任何
  错误 —— 要等下一条命令才失败，而且往往是一条不相干的命令。
* **但 `NOOP` 不足以判定连接可用**，这是实测结论：服务器刚关闭时，客户端**写入仍可能成功**，
  而 async-imap 把"流结束"读成了空响应，于是 `noop()` 返回 `Ok`。真正失败的会是下一条命令
  （实测 `os error 10053`）。所以池子还会在命令失败时把那条连接标记为坏、丢弃，**换新连接重试一次**。

### 推送：IMAP IDLE，亚秒级（2026-10-03 更正）

新邮件不再靠轮询。流程是「连上 → SELECT → IDLE → 服务器推送 → 有变化就同步」，
断线按 1s→2s→…→60s 指数退避重连，成功空转一轮就重置。

**这里曾有一条错误的结论。** 旧文档写"async-imap 0.12 把 `* n EXISTS` 归到
`Response::Data { status: Ok, .. }` 并 `continue`，所以 `IdleResponse::NewData` 永远不会触发，
于是推送延迟由 IDLE 窗口决定（50 秒）"。**这是错的**：`MailboxData(Exists)` 落到通配分支，
`NewData` 在推送后约 **0.2 秒**就返回。现在由探针钉住：
`tests/loopback.rs::probe_idle_delivers_exists_before_the_window_closes`（5 秒窗口下断言 < 2 秒）。

所以：**通知是推送驱动的、亚秒级**；IDLE 窗口只是心跳与"连接还活着吗"的兜底（设计定为 120 s），
一个周期里真正昂贵的同步只在数量变化时才发生。收到推送时**要同时刷新比对基线**，
否则紧接着的心跳会再触发一次同步（同一条变化同步两遍）。

真机验证（QQ 邮箱，给自己发一封）：

```
已开始监听 me@example.com 的 INBOX
同步 INBOX：新增 1 封，缓存共 150 封        ← 20 秒后检测到
推送：me@example.com 的 INBOX 新增 1 封（未读 53）
```

### 托盘与通知

托盘菜单：打开主窗口 / 立即检查新邮件 / 退出；左键点击唤出窗口；tooltip 显示未读数。
Windows 通知**由 Rust 侧调用**（`tauri-plugin-notification`）—— 前端没有打包器，插件的 JS 包
进不来，而推送本来就发生在 Rust 里。

---

## 附件下载（v0.2）

读信时点附件标签即保存到 `下载\Easy Email\`，文件名由后端消毒后决定。

**文件名是敌意输入。** `filename="..\..\Startup\x.exe"` 是发件人可以随便写的合法值，所以名字会被
压成单个路径分量、替换掉 Windows 不接受与所有控制字符、超长截断、`CON`/`NUL` 这类设备名加前缀，
空名则退回 `attachment-<part>`。已存在的文件不会被覆盖，而是加 ` (2)` 后缀 —— 静默覆盖会丢掉
用户可能还没看过的文件。

**已知代价：下载会把整封邮件取回来。** 也就是说峰值内存与**邮件大小**成正比，而不是与附件大小成正比。
这是库的限制而非疏忽，而且尝试过更省的路：

* **IMAP 分部取件**（`BODY.PEEK[]<0.196608>`）可以真正流式，但**响应解析不了**。
  `imap-proto` 0.17 的 section 语法里没有 `<origin.offset>`，所以服务器按 RFC 回显的
  `BODY[]<0.196608>` 会让解析器直接报 `TakeWhile1`（实测）。让夹具不回显那个后缀能让测试变绿，
  但会在每一个真实服务器上失败 —— 那比不做优化更糟。
* **按 IMAP part 路径取单个 part**（`BODY.PEEK[2]`）能少缓冲一部分，但 `mail-parser` 暴露的是
  附件在文档顺序里的下标，不是协议用的 `1.2.3` 点分路径；靠遍历 MIME 树去推导，正是那种在真实邮件上
  会崩的"聪明代码"。

所以选择：一次取回、用 `mail-parser` 已经解好码的字节直接落盘，并把限制写在这里。

真机验证（QQ 邮箱，一份发票 PDF）：

```
uid=5809 name="Invoice-XKOGBCCQ-0001.pdf" declared=23477 written=23477 on_disk=23477 head=25 50 44 46 2D
```

`25 50 44 46 2D` 就是 `%PDF-`：声明长度、写入长度、磁盘长度三者一致，文件头也对。

---

## 附件上传（v0.2）

写信窗口点「📎 添加附件」选本地文件；文件**从磁盘读取**，不经过界面 —— `OutgoingAttachment`
传的是路径，`smtp` 在组装时才读，所以 20MB 的附件不会先变成一串 JSON 穿过 IPC。

MIME 类型按扩展名猜；猜不出来就用 `application/octet-stream`。**猜错的类型不会中止发送** ——
把文件当成不透明字节发出去，永远好过因为一个没人会看的细节丢掉用户的附件。

组装形状：有附件时是 `multipart/mixed`，第一个 part 是正文（有 HTML 时是 `multipart/alternative`），
其余是附件。纯文本正文**直接放进去**，而不是套一层只有单个 part 的 `multipart/mixed` —— 那合法但没必要。

真机往返验证（QQ 邮箱，发给自己一封带 40KB 非 7bit 内容的邮件，再从 IMAP 取回）：

```
sent 40000 bytes
attempt 0: uid=5814 attachments=1 names=["roundtrip.bin"] sizes=[40000]
back=40000 written=40000 identical=true
```

---

## 对话模式（v0.2）

列表把同一个会话折成一行（行上带封数）。点开它、并且顶部按钮处于 **💬 对话模式** 时，
整个会话按气泡铺开：对方在左、自己的邮件在右（**跨文件夹**取的，所以只存在于「已发送」里的回复也在）。
**对话模式对每一封都适用** —— 只有一封时也是「一封的会话」，照样按气泡显示，
视图不会因为邮箱里碰巧没有会话而变形。

**怎么算"同一个会话"**：两条信号**并集**。
① `References` / `In-Reply-To` 链（`crates/mail-core/src/threads.rs` 取根）——事实；
② **同一发件人 + 同一主题**（去掉 `Re:` / `Fwd:` / `回复：` 等前缀）且**相邻两封间隔不超过 7 天**——启发式，
   只对**你收到的**邮件生效。所以你收到的那四封同主题、彼此不是回复的信会并成一行；
   而"同一个主题、下个月又来一封"不会（Google 的「安全提醒」在真实邮箱里 24 封跨 4.7 年，
   不加时间窗会被并成一行，加窗后是 15 段）。只按主题合并会把同主题但不相干的信永久并到一起，
   两条信号并集就是为了**只增不减**：今天能合并的绝不会因为这个猜测而被拆开。

出现"该合没合 / 不该合却合了"时，先跑 `python tools/inspect-cache-detail.py <对方地址>`：
它会打印每封的 `thread_key`、`subject_key` 与线程头，一眼就能判断。

**默认是普通模式**（✉ 普通模式 / 💬 对话模式 一键切换，选择记在本机）。

- **所有气泡默认折叠**，只显示发件人、时间和一小段正文预览；**点一下展开**，**点它的头部再收起**
  （头部有 `▸ / ▾`）。展开气泡的正文里点击不会误收起 —— 那是一份你正在读、正在选字的文档。
- **同时最多展开一封。** 这不是省事：界面上始终只有**一个**沙箱正文渲染面（`#body-surface`），
  它会在展开的那封里"就位"。两个渲染面就意味着「显示图片」的解锁能从一封泄漏到另一封，
  而 20 封的会话也不该有 20 份渲染器。展开第二封会把第一封收起。
- **底部可以直接回复**：输入内容 → 发送（或 `Ctrl+Enter`）。它走的是同一个发件箱，
  所以 5 秒撤销窗口、"关掉应用也不会丢"、"崩溃后按冻结的 Message-ID 查重、绝不重发"这些保证一个不少。
  回复框上方一行会**先告诉你这封回给谁**：优先回答你展开的那封，否则回答会话里最新的一封**不是你自己**的邮件
  —— "回最新一封"在"最后说话的是自己"时会把信发给自己。
- 回复会带上 `In-Reply-To` 与完整的 `References` 链（规则在 `src/reply.js`，由 `tools/reply_test.mjs` 与门禁钉住），
  否则在对方那里它就是一个全新会话。

真机验证（QQ 账号，真实 35 封会话）：首屏 0 封展开；展开/收起/换一封都正确；
展开第二封时第一封收起；底部显示"回复给 correspondent@example.com"；空内容点发送只得到提示、发件箱没有多出任何东西；
全场始终只有 1 个正文渲染面。
**没有验证的**：真的按下「发送」——那会给真人发信，所以探针刻意不点（入队那条路由发件箱探针与 loopback 测试覆盖）。

也就是说：出去的编码、进来的解码、两边的大小，全都对得上，逐字节一致。

---

## 安全设计

- **密码不进配置文件。** 账号信息写 `%APPDATA%/easy-email/accounts.json`，
  密码/授权码进 Windows 凭据管理器。
- **邮件正文用双重沙箱渲染。** `iframe sandbox`（禁脚本、禁表单、禁同源、禁顶层跳转）
  加独立 CSP `default-src 'none'`。**远程图片默认不加载** —— 那正是追踪像素上报"已读"的方式。
  内联样式和 `data:` 图片保留，所以邮件排版和嵌入 logo 正常。
  "显示图片"只为**当前这一封**解锁（切到别的邮件会立刻恢复拦截），而且必须点一次才生效。
- **CSP 的约束关系是"取交集"，而且 `srcdoc` 文档会继承父文档的策略。**
  所以外壳的 `img-src`（`index.html` + `tauri.conf.json`）不能比正文策略更严，
  否则正文策略再宽松也没用（这就是上面第 8 条那个 bug）。
  真正的把关点在 `buildSandboxedDocument()` 的默认分支，`check.ps1` 有一项专门盯着它别被改坏。
- **CSP 锁死渲染进程**：脚本只能来自本地，`connect-src` 只留 IPC。
- **凭据按平台分离保存**：Windows 走凭据管理器（密码）+ DPAPI 用户态加密文件（刷新令牌）；
  Linux 走 D-Bus Secret Service（密码）+ 机器标识派生密钥加密的 `0600` 文件（刷新令牌）。
  日志里永远不出现令牌或密码（`tools/check.ps1` 有一项专门拦这个）。
- 报告安全问题见 [SECURITY.md](SECURITY.md)，第三方组件许可清单见
  [THIRD-PARTY-NOTICES.md](THIRD-PARTY-NOTICES.md)。

---

## 已知限制 / 下一步

- **每个操作新建一次 IMAP 连接。** ~~一次 TLS 握手换掉整个连接池子系统，v0.1 的取舍。~~
  已解决：见「连接池与推送」。`max_connections` 现在作用在空闲连接数上。
- ~~推送有最多 50 秒延迟~~ **已更正**：IDLE 推送是亚秒级的（见上一节）。
- **只监听 INBOX。** 其他文件夹有新邮件不会触发通知。
- **托盘图标的位置由 Windows 决定**，可能被折叠进"隐藏的图标"溢出区。
- **缓存只覆盖同步窗口内的邮件**（默认最近 2 年，可在侧栏「同步窗口」改）。列表一次显示 50 封，
  「加载更多」**先把本地缓存里剩下的那几页显示出来**（所以离线也能往回翻），缓存翻完了才向服务器要下一页；
  两边都没有了才会显示"没有更早的邮件了"。
  > 这一条曾经是错的（2026-10-06 修）：早期版本「加载更多」**直接问服务器**，从不显示缓存里多出来的部分 ——
  > 于是服务器没有更早邮件时它就宣布"没有更早的邮件了"，而本地已经下载好的 142 封（9/2 起）永远看不到。
  > 规则现在住在 `src/history.js`，由 `tools/history_test.mjs` 与门禁钉住。
- ~~**搜索的正文只覆盖已打开过的邮件**~~ **已改**：后台每 60 秒索引一批（每批 20 封）
  已同步但未打开的正文，覆盖率显示在搜索状态行（见「本地缓存与搜索」）。
- ~~**离线时不会显式提示"离线"**~~ **已做**：列表顶部一条横幅说明"下面是上次同步的内容"，
  并按失败类型给不同的出路（连不上 / 授权码过期 / 当日配额到顶 / 本地数据库），
  带「重试」；下一次刷新成功就消失（复盘：`sync_status.offline` 的语义其实是"上次同步失败"，
  把它一律说成"离线"会把该换授权码的人劝去检查网线）。
- **Outlook / Microsoft 365 只能走 OAuth2**（微软 2024-09-16 起对个人账号彻底关闭基本认证）。
  这条路已经实现：向导里选 Outlook，展开「高级」，填**你自己的** Azure 应用注册的客户端 ID
  （微软没有公开可借的注册），再点「打开浏览器登录」。
  **它还没有在真实 Outlook 账号上走通过** —— 注册步骤见下面「真实账号手工验证」。
  能说的保证到什么程度：这条路的**线上契约**已按微软官方文档逐条核对并钉进测试
  （`the_microsoft_flow_matches_the_published_imap_smtp_oauth_contract`：三个资源 scope、`offline_access`、
  `common` + v2 端点、PKCE S256、公共客户端**不带** `client_secret`、回调用 `http://localhost` 且匹配时忽略端口、
  以及**用文档里那个 XOAUTH2 示例的 base64 做逐字节比对**）。
  文档核对替代不了的只有三件：**同意页上你真的点了允许**、**租户/账号允许 IMAP 与 SMTP**、**浏览器里授权的
  账号和你填的是同一个**（最后这条程序会自己核对并当场说明）。
  另外注意：很多 **M365 租户把 SMTP AUTH 关掉了**，那种账号能收信、不能发信，
  程序会明确说"需要在 Microsoft 365 管理中心开启 SMTP AUTH"，而不是让你去找一个**并不存在**的授权码。
- ~~**附件上传没有大小上限检查**~~ **已解决**：选取附件时先按 `字节 × 4/3 + 4096` 估算，
  超限就不加进来；发送前再用真实编码后的字节拦一次（`max_send_bytes`，一个数字一个出处）。
  仍未做的是**归档副本**那条路的 `append_limit_bytes` 预检。
- **下载附件时峰值内存与整封邮件同量级**，原因见「附件下载」一节。
- **删除是永久删除**（`UID EXPUNGE`，只在服务器不支持 UIDPLUS 时才退化成 `EXPUNGE`）。
  还没有"移到垃圾箱"的自动行为。
- 单账号视图，没有统一收件箱。

### 自定义服务器（`custom` / 自建邮局）

向导里可以选「自定义 / 自建服务器」，自己填 IMAP/SMTP 的主机、端口与加密方式。
地址会被清洗（`imaps://imap.example.com/` 这种粘贴会被化简），端口、路径、空格写错会当场拒绝，
`不加密` 只允许本机地址。这几条规则的**唯一出处是 Rust**（`normalise_host` / `validate_endpoint`），
前端不抄第二份。

- **只做了手动层，没有自动发现。** 域名 autoconfig / ISPDB / DNS SRV / Autodiscover 都没做：
  它们要向第三方发出用户域名的请求（`design/05` §11 反驳 5），而且需要网络层的额外工作。
  表外服务商必须自己知道服务器地址。
- ~~**仍然要求 IMAP 与 SMTP 都通才会自动保存账号。**~~
  **已做**：填自定义服务器时多一个勾选「**这个服务器不提供 SMTP（只收信，不发信）**」。
  勾了之后 IMAP 通过就保存；**没勾时行为和以前完全一样**（SMTP 失败照样拒绝保存，
  这是"账号必须是完整的"这条保护，loopback 夹具钉住了它）。
  该账号从写信入口会被明确拒绝，而不是让 worker 每轮重试一遍同一个错误。
  只发信不收信的服务器**仍然加不进来**（那是另一个方向，没做）。
- **自定义账号不参与服务商表的所有策略**：`daily_download_budget()`、`append_limit_bytes()`、
  `smtp_max_message_bytes()` 对它都返回 `None`（"未知"），所以配额节流与发送前的大小预检对它不生效。
  这是如实表示未知，不是漏洞。

---

## 许可与致谢

本项目以 [MIT](LICENSE) 发布。第三方组件与各自许可的完整清单见
[THIRD-PARTY-NOTICES.md](THIRD-PARTY-NOTICES.md)；安全相关问题见 [SECURITY.md](SECURITY.md)；
想参与开发见 [CONTRIBUTING.md](CONTRIBUTING.md)（一句话：**修了行为就要有能抓住那个 bug 的测试**）。

内置的 21 个服务商配置来自各服务商公开文档与登录帮助页的表述，不含任何商标图形；
OAuth 路径不内置任何第三方客户端注册——Gmail 与 Microsoft 365 都要求用户使用自己注册的应用。
