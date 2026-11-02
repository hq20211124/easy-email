# Easy Email

[![Release](https://img.shields.io/github/v/release/hq20211124/easy-email?style=flat-square)](https://github.com/hq20211124/easy-email/releases/latest)
[![CI](https://img.shields.io/github/actions/workflow/status/hq20211124/easy-email/release.yml?style=flat-square&label=build)](https://github.com/hq20211124/easy-email/actions)
[![License: MIT](https://img.shields.io/badge/license-MIT-green?style=flat-square)](LICENSE)
[![Windows](https://img.shields.io/badge/Windows-0078D6?style=flat-square&logo=windows&logoColor=white)](#安装)
[![Linux](https://img.shields.io/badge/Linux-FCC624?style=flat-square&logo=linux&logoColor=black)](#安装)

轻量级**跨平台**邮件桌面客户端，主要目标是 Windows，Linux 亦可编译与运行。**用户只需要填邮箱地址和授权码 —— 服务器地址、端口、加密方式全部内置。**

技术栈：Tauri v2 + Rust。没有 Node 构建步骤，前端是静态 HTML/JS，整个项目一次 `cargo build` 搞定。

> **版本口径**：最新发布是 **v0.1.3**（2026-10-16）。仓库里已经是 **0.1.4**，但还没打 tag ——
> 本文件描述的是**仓库当前状态**，已发布版本比它少一些东西（下表里标了 ⚠️ 的项大多属于这一类）。

---

## 能做什么

| 能力 | 状态 |
|---|---|
| 内置 21 个主流国内外邮箱配置（含授权码引导与 quirks） | ✅ 40 个端点真机握手验证 |
| 按邮箱地址自动识别服务商；文件夹名本地化并按习惯排序 | ✅ |
| IMAP：文件夹、列表、读正文（含附件元数据） | ✅ |
| SMTP：发信、回复、转发（含正确的会话头） | ✅ |
| 删除、标为已读/未读、移动到文件夹 | ✅ |
| 账号保存 + 密码进系统凭据管理器（Windows / D-Bus Secret Service） | ✅ |
| 添加账号时用真实连接验证 IMAP + SMTP | ✅ |
| 安全渲染正文（沙箱 iframe，默认拦截远程图片，按封解锁） | ✅ |
| 浅色 / 深色主题 + 三套主题色（默认浅色，记住选择） | ✅ |
| SQLite 本地缓存 + 增量同步（`UIDVALIDITY` + UID 游标） | ✅ |
| 断网可读：列表、正文、文件夹都来自本地 | ✅ |
| 本地全文搜索（FTS5，中文按字切词，支持字段限定） | ✅ |
| 正文全文索引：后台有界回填 + 覆盖率可见 | ✅ 一轮 50 封，轮间按占空比自适应 |
| 正文不落盘（可切换，切换时可清空已下载正文） | ✅ |
| 附件名进索引（中文文件名、扩展名都能搜） | ✅ |
| 索引重建（可续跑、可停止、不联网） | ✅ 二次确认 |
| 保存搜索 / 搜索历史 | ✅ |
| 失败可见性：按失败类型的横幅（离线 / 授权码 / 配额 / 本地）+ 重试 | ✅ |
| 连接池（复用 IMAP 连接）+ IMAP IDLE 推送 | ✅ 推送亚秒级；服务器不支持 IDLE 时降级轮询 |
| 系统托盘 + Windows 通知 | ✅ |
| 附件下载 / 上传 | ✅ 上传有大小预检 |
| 对话模式（跨文件夹气泡视图，底部可直接回复） | ✅ |
| 历史邮件回填（缓存最新 500 封以外，「加载更多」先给缓存） | ✅ |
| 存储与清理（看占用、干跑、确认后清理，未读与旗标永不清） | ✅ 手动触发，没有定时任务 |
| 关于面板：版本 + 检查更新（minisign）+ 诊断包（先预览、全脱敏） | ✅ |
| 自动更新（四类安装包各有清单条目） | ✅ 代码完成；真机待下一次 tag |
| 磁盘加密自检（读 BitLocker 两个文档化数字，逐卷报告） | ⚠️ 非提权一律「无法判定」，提权归档还没做（`tools/disk-protection-check.ps1`，退出码 0/1/2 = 成立/不成立/无法判定） |
| OAuth2 登录（Outlook / Microsoft 365） | ⚠️ 代码就绪、端点实测可达，**未在真实 Outlook 账号上走通** |
| 自定义 / 自建服务器（手填 IMAP/SMTP） | ✅ 错的输入被正确拒绝；**未对着真实自建服务器登录过** |
| 自动发现（autoconfig / ISPDB / SRV / Autodiscover） | ❌ 未做 |

---

## 安装

装好的包在 [Releases](https://github.com/hq20211124/easy-email/releases/latest) 页面，
四类产物由 CI 自动构建并附带 `SHA256SUMS`。

| 平台 | 下载 | 安装 |
|---|---|---|
| Windows | `Easy Email_x64-setup.exe` | 双击安装（需要 WebView2 运行时，Win10/11 一般自带；安装时按需联网下载） |
| Debian / Ubuntu | `Easy.Email_*_amd64.deb` | `sudo dpkg -i Easy.Email_*_amd64.deb`（依赖会自动提示） |
| Fedora / RHEL | `Easy.Email-*-1.x86_64.rpm` | `sudo dnf install ./Easy.Email-*-1.x86_64.rpm` |
| 任意 Linux（免安装） | `Easy.Email_*_amd64.AppImage` | `chmod +x Easy.Email_*_amd64.AppImage && ./Easy.Email_*_amd64.AppImage` |

> 文件名里的版本号以 Release 页的实际资产为准；**CI 上传时空格会被换成点**
> （`Easy Email` → `Easy.Email`），所以资产名与本地 `cargo tauri build` 的产物名不同。
> 用通配符就不用记版本号。

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
> 只有走到 bundler 那一步才要密钥。真正的发布不要本地打 —— 见下一节。

**没有控制台窗口。** exe 是 GUI 子系统的（Tauri 模板默认只有 release 才是，于是每次双击 debug
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

**改完 `src/` 下的文件必须重新 `cargo build`**：前端资源在编译期嵌进二进制，运行时不会再读磁盘。
（实测：把 `src/theme.js` 改名移走、不重新构建直接启动已构建的 exe，界面仍然完全正常 ——
如果它读磁盘，缺文件会让 `boot()` 抛错、邮箱列表根本不会出现。）
**装到本机的那份也一样**：桌面快捷方式指向的是安装目录里的副本（Windows 上是
`%LOCALAPPDATA%\Easy Email\easy-email.exe`），不重新构建并覆盖它，点图标看到的永远是旧版本。

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

重新生成图标：`python tools/make_icon.py`。
抓一张运行中的窗口截图（调试 UI 用）：`.\tools\capture-window.ps1`（输出 `target\screenshot.png`）。

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
  src/secret.rs            系统凭据管理器封装
  src/error.rs             错误分类：把 AUTHENTICATIONFAILED 变成可操作的中文提示
  src/model.rs             与 UI 共享的可序列化类型
  src/cache.rs             本地 SQLite 缓存 + FTS5 搜索索引
  src/index.rs             后台有界回填：把没打开过的正文加进索引
  src/userdb.rs            用户自己的库：草稿、发件箱、同步窗口、配额账本
  src/worker.rs            发件箱 worker（一遍有界的出队）
  tests/loopback.rs        进程内假 IMAP/SMTP 服务器，驱动真实客户端跑完整收发闭环
  tests/provider_reachability.rs   对着真实服务器验证配置库
src-tauri/                 Tauri 外壳
  src/lib.rs               83 个 Tauri 命令 + 状态管理 + 后台 worker
  src/store.rs             账号持久化（原子写入 + 损坏文件自动备份）
src/                       前端（静态 HTML/JS/CSS，无需打包）
tools/                     门禁、基准、CDP 探针、一次性脚本
reports/                   性能与安全报告；development-log.md 是踩坑记录
design/                    设计文档（决策与理由都在这里）
```

## 测试与门禁

| 测试 | 证明什么 | 需要凭据吗 |
|---|---|---|
| `tests/loopback.rs` | 客户端的协议交互是对的：问候语握手、`ID` 在 `LOGIN` 之前、`UID FETCH` 分帧、附件与 GB2312 解码、SMTP 认证与发信头。61 个用例，包括"服务器不支持 UIDPLUS"和"密码被拒"两条错误路径 | 不需要 |
| `tests/provider_reachability.rs` | 内置配置库指向的 40 个端点是真的存在、端口和加密方式没写错 | 不需要 |

两层都不需要真实邮箱，所以 CI 里能全跑。`loopback` 用的是一个只实现必要协议子集的进程内服务器 ——
这已足够覆盖客户端自己的行为。

**第三层：跑在真实窗口上的 CDP 探针**（`tools/inspect-dom.mjs` + `tools/probe-*.mjs`）。
它驱动的是真正在跑的那个界面，所以能验证"两个纯函数各自都对、接起来却不对"这类问题
（`probe-oauth-wizard.mjs` 就是为一个"按钮可见但没有事件"的 bug 写的）。
用法与开关见 `HANDOFF.md`：要先临时把 `--remote-debugging-port=9222` 加回配置并重建，**验完必须删掉**
（`check.ps1` 会拦）。

> 为什么需要三层 —— 十个只有真的跑起来才会发现的问题（编译绿、界面坏；测试绿、数据错；接口通、
> 用户看不到），逐条记在 **[reports/development-log.md](reports/development-log.md)**。

---

## 性能预算

唯一的预算表在 `design/15` §4（P1…P16）；实测数字、推演过程与原始读数全部在
**[reports/performance.md](reports/performance.md)**。`tools\bench.ps1` 会跑夹具并把 16 项**全部**
列出来：实测的给数字，没测的写明为什么。

| 结论 | 项 |
|---|---|
| ✅ 达标 | P1 冷启动（中位 1 039 ms，预算 1 500）、P2 有缓存的文件夹（12–27 ms）、P3 每帧 DOM 变更（22–35 行）、P4 搜索（P95 41.4 ms）、P5 常驻内存（私有字节 226–253 MB）、P7/P9/P10/P12/P13/P15 存储与 IPC 侧 |
| ⚠️ 未测 / 读数不一致 | P11 附件另存峰值（要真实 IMAP 取件）、P16 备份（要第二个库）、P6 空闲 CPU（五次 0.21–0.43%，一次 0.82%）；P1/P5 的"10 万封"形态：夹具已就位（`tools\make-fixture.ps1`），但桌面壳在独立 profile 下渲染进程起不来 |
| ❌ 超标 | **P2 在没有本地缓存的文件夹上 273 ms > 200 ms**（有缓存的是 12–27 ms，预算判**任何**文件夹）；**P14 索引体积 ≈171%** 的已索引文本（预算 ≤80%，结构性原因：CJK 逐字分词，待拍板改数字还是改索引形状） |
| 🔧 量到收益、但没改生产代码 | 正文入库的批量事务（2–3 倍），因为空事务只占每封 0.2–0.3%，而生产里唯一没批量的那条路在等 IMAP 往返 —— 收益到不了用户 |

**倍数的口径**（这一节唯一值得背下来的一句）：只有**同条件配对**才发倍数，目前只有三对 ——
滚动的 52 → 22–35 行/帧（1.5–2.4 倍）、批量事务的 2–3 倍（只指 SQLite 那一半）、
P1 的 1 508 → 1 024 / 930 ms（1.47 / 1.62 倍，其中"问一次文件夹"是 38 倍但整条只有 1.47 倍）。
**"量到 N 倍"和"用户会感到 N 倍"是两个问题**：前者的答案在基准输出里，后者的答案在这段代码占不占总账。

---

## 真实账号手工验证

有些东西只能用人自己的账号点一遍。目前**没验过**的是下面两条，步骤写全是为了下一次不用再想：

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

### 验证状态与判定标准

`verified` 标记表示**已经完成真实的 TCP + TLS + 协议握手**（IMAP 收到 `* OK`，SMTP 收到 `220`），
由 `tests/provider_reachability.rs` 执行，所以这个标记是证据而不是乐观。

最近一次全量验证：**2026-10-02，40 个端点，43 秒，全部通过**（`qq`/`163`/`126`/`yeah`/`exmail`/
`qiye163`/`aliyun`/`sina`/`139`/`gmail`/`outlook`/`yahoo`/`icloud`/`zoho`/`yandex`/`aol`/`gmx`/`fastmail`
的 IMAP 全部 `OK`；SMTP 除 Gmail 外全部通过）。完整输出见 `PROGRESS.md` 对应轮次。

> **Gmail 的 SMTP 465 从大陆网络经常连不上** —— 这是网络事实，不是配置错误，所以 `verified`
> 保持 `true`，同时客户端会把"连不上"当成一种可解释的结果而不是 bug 抛给用户。
> 只有 Proton（本机 Bridge，无法在 CI 验证）和 Tuta（无 IMAP 端点）保持 `verified: false`。

这个测试把结论分成三类，并且**只有第一类会判定失败**：

- **协议不符** —— 连上了、TLS 也握上手了，但对方说的话不是 IMAP/SMTP 该说的话。
  这才是"内置数据错了"的证据，会直接 fail。
- **不可达** —— 连不上、或者 TLS 握手超时。**超时不含任何关于对方的信息**，所以只告警。
- **OK** —— 收到 `* OK` / `220` 问候语。

（这里踩过一次：最初把"TLS 握手超时"也归成"协议不符"，一次网络抖动就让整个门禁变红。
现在超时一律算不可达，并且一个"协议不符"结论会有**第二次机会** —— 重试一次才判失败。）

---

## 本地缓存与搜索

**列表是窗口化的**：不管缓存里有多少封，DOM 里始终只有视口 ± 8 行（`design/15` §6）。
行高固定 56px（`--msg-height`，CSS 与 `src/vlist.js` 的 `DEFAULT_ROW_HEIGHT` 共用一个值，
`tools/vlist_test.mjs` 断言两者相等——一个变一个不变会让滚动时行慢慢漂移，而且任何地方都不报错）。
滚动提交合并在一个 `requestAnimationFrame` 里。「加载更多」每次只取一页（`message_page`），
追加而不是全量重取。

**切文件夹先画本地缓存**：`loadMessages` 分两条腿——先用 `message_page`（只读缓存、不联网）
把已有的 50 行画出来，再用 `list_messages` 刷新、重画。代价是明说的：屏幕上第一瞬是**上次同步**
的内容，不是服务器的；收取期间标题写着「收件箱（正在收取…）」，刷新回来（或被失败横幅接管）才消失。
三个只有"事后截图"看不出来的错各自有钉子（`src/first-paint.js` + `tools/first_paint_test.mjs` + 门禁）：
把没同步过的文件夹说成"空文件夹"、刷新失败时把用户本来看得见的列表擦成"收取失败"、
以及切走之后迟到的答案盖到新文件夹上。「新邮件」推送只画缓存 —— 后端是**存好邮件之后**才发这个事件的。
同步失败时列表顶部有一条横幅说明"下面是上次同步的内容"，并**按失败类型**给不同的出路
（连不上 / 授权码过期 / 当日配额到顶 / 本地数据库），带「重试」，下一次刷新成功就消失 ——
一律说成"离线"会把该换授权码的人劝去检查网线。

`%APPDATA%\com.easyemail.desktop\cache.db`（SQLite，紧挨着 `accounts.json`，不想要就直接删）。
缓存里**没有任何密码或授权码** —— 那些只在系统凭据管理器里。

### 增量同步靠 `UIDVALIDITY` + UID 游标

* **首次**同步取最新 500 封的头部（不是从 UID 1 全量拉 —— 两万封的文件夹会让首屏不可用）。
* **之后**每次同步就是 `UID FETCH <last_uid+1>:*`，一次往返，与文件夹大小无关。
* 游标是 `(UIDVALIDITY, last_uid)` 的组合。**`UIDVALIDITY` 一变，缓存里所有 UID 都可能指向别的邮件**，
  所以整个文件夹会被清空重建，而不是拿旧头部去配新邮件。

真机实测（QQ，149 封）：第一次同步新增 149 封、缓存共 149 封；第二次新增 0 封。

### 保存的搜索与搜索历史

侧栏「保存的搜索」：点一下跑那个查询，`✎` 改名、`✕` 删除；搜索框旁的「保存」把当前查询存进去
（全局查询会标「所有账号」，否则跨账号看起来像邮件跟着人走）。搜索框旁的「历史」是最近 12 条的下拉，
点一下重跑；同名查询刷新不重复，最多留 50 条。两个列表都只存在本机。

### 搜索能搜到什么，搜不到什么

搜**全部已同步邮件的主题、发件人、收件人**，加上**正文已被索引的邮件的正文**，
再加上**附件名**（`报告.pdf` 这种：中文文件名和 `pdf` 这样的扩展名都能搜）。
附件**文本**（PDF/Word 里面的字）不做 —— 那是 `design/09` §11/Q3 里 M2 的开关，默认关。

**可以按字段限定**（多个词之间是 AND）：

| 写法 | 含义 |
|---|---|
| `from:boss@example.com` | 只搜发件人（显示名 + 地址） |
| `to:me@example.com` / `cc:…` | 只搜收件人/抄送 |
| `subject:对账` | 只搜主题 |
| `body:季度` | 只搜正文（2 字的中文词同样可搜） |
| `filename:报告.pdf` | 只搜附件名 |
| `has:attachment` | 只看带附件的邮件，可与别的词组合（`from:shop has:attachment`） |
| `after:2026-01-01` / `before:2026-01-01` | 按日期切（闭区间；日期按 **UTC** 解释，见下） |
| `is:unread` / `is:flagged` | 只看未读 / 已标星 |
| `in:INBOX` | 只看某个文件夹（不区分大小写） |
| `account:<id>` | 指定账号（**覆盖**当前账号范围；不给就用当前账号） |
| `larger:10MB` / `smaller:1GB` | 按大小切。**KB/MB/GB 按十进制**（1KB=1000），`KiB`/`MiB`/`GiB` 按二进制；大小未知的邮件两边都不进 |

**不认识的写法按字面搜**：`frm:boss` 会去找 `frm:boss` 这几个字——一次手滑不该换来一个
静默的空结果。打错了会有提示：状态行会直接说哪儿不对（「『frm:』不是已知前缀」或
「日期『昨天』看不懂，这个条件没有生效」），这是 `design/09` §2 的要求——是提示，
不是一个空结果。一次查询最多返回 200 条命中。日期按 **UTC** 解释（没有为它引入时区库）：
东八区用户看到的边界是 UTC 的当日 00:00 而不是本地午夜，这一点写在文档里而不是留给用户猜。

正文不在同步范围内：同步只取头部，取一封正文的代价和打开它一样。所以正文索引是一个
**独立的后台任务**（`crates/mail-core/src/index.rs`），一轮最多取 50 封（一批一次往返），
轮间等待不是固定 60 秒而是**按占空比 250 ‰ 从上一轮的实际耗时推出来**——链路变慢时绝对间隔自动变长，
但连接份额不变；用户正在读邮件时立刻让路，500 毫秒后再看一眼。它专门去取"已同步但你没打开过"的邮件；
搜索状态行会显示 **「正文已索引 x/y 封」**（分子是已索引、分母是已同步），旁边一个「现在索引」按钮
可以立刻再跑一遍。这样"搜不到"有两种含义时界面说得清是哪一种：**邮箱里没有**，还是**还没索引到**。

> 索引进去的正文会截断到 64K 字符（`design/09` 的默认值）。**截断只发生在索引那一份** ——
> `bodies` 表里存的仍是全文，因为那是你要读的东西，截了就是丢邮件。
> **附件名接在截断之后**，不是之前：一封 64K 的长信不该把文件名挤出索引，
> 而文件名恰恰是长会话里最可能被搜的那几个字。

> **从 v0.1.2 升级来的邮件也会拿到附件名索引，而且是本地的。** 每条已存正文记着自己是由哪一版
> 索引写成的（`index_version`）；落后于当前版本的行会重新进待办遍，但补齐动作只是从
> `bodies` 表里已经存着的正文重写一次索引行 —— **不重新取件、不占连接、不花配额**。

搜索完全走本地，**断网可用**，也不会因为服务器慢而卡住。

### 重建索引

搜索状态行有「重建索引」：把本地索引整个丢掉，再分批建回来。索引是**派生数据**，
`messages` 与 `bodies` 一行都不动，所以重建不可能丢邮件——但它确实会先清空，
中间有一段时间搜索能答的比之前少，所以要**点两次**（第一次按钮变成「确认重建？」）。

- **分批**：每批 500 条重建完就把控制权还给界面，进度显示「正在重建索引 done/total」；
  进度数字是**从数据库里数出来的**，所以中途关窗再打开，进度还在、接着做完 —— 这也是"可续跑"的全部实现。
- **可停止**：点「停止」就是不再进行下一批。已重建的部分照常可搜，未重建的搜不到，
  再点一次从原地继续。部分索引不是坏索引。
- **不联网**：重建只重新推导本机已有的内容。没有取过正文的邮件重建出来仍然**只按头部可搜**，
  正文要靠上面的「现在索引」补 —— 这也是 UI 必须写清楚的一句。

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

## 连接池与推送

### 连接池：每个账号最多 `max_connections` 条空闲连接

空闲连接会被复用，`list_messages` 这类操作只花一次往返。两个细节：

* **复用前会发一条 `NOOP` 验活**，因为服务器会悄悄关掉空闲连接，而 IMAP 里这不会表现为任何
  错误 —— 要等下一条命令才失败，而且往往是一条不相干的命令。
* **但 `NOOP` 不足以判定连接可用**（实测：服务器刚关闭时客户端写入仍可能成功，而 async-imap
  把"流结束"读成空响应，`noop()` 返回 `Ok`）。所以池子还会在命令失败时把那条连接标记为坏、
  丢弃，**换新连接重试一次**。

### 推送：IMAP IDLE，亚秒级

流程是「连上 → SELECT → IDLE → 服务器推送 → 有变化就同步」，断线按 1s→2s→…→60s 指数退避重连，
成功空转一轮就重置。收到推送时**要同时刷新比对基线**，否则紧接着的心跳会再触发一次同步。

> 旧文档曾断言"`NewData` 永远不会触发、推送延迟由 50 秒的 IDLE 窗口决定"，**这是错的**：
> `MailboxData(Exists)` 落到通配分支，推送后约 **0.2 秒**就返回。现在由
> `tests/loopback.rs::probe_idle_delivers_exists_before_the_window_closes` 钉住（5 秒窗口下断言 < 2 秒）。

IDLE 窗口只是心跳与"连接还活着吗"的兜底（设计定为 120 s），一个周期里真正昂贵的同步只在数量变化时
才发生。

**服务器不支持 `IDLE` 时降级轮询。** RFC 2177 是可选的，网易 163 对 `IDLE` 回 tagged
`BAD command not support`。这**不是**断线：重连一万次也拿不到推送。所以 watcher 一旦收到这个拒绝
就**降级为轮询**（每个窗口一次 `SELECT` 比对）并只上报一次 —— 在此之前它把这条拒绝当成"连接丢了"，
真机日志里刷了 420 条 `command not support`。副作用只是新邮件延迟从"秒级"变成"最多一个窗口"。

托盘菜单：打开主窗口 / 立即检查新邮件 / 退出；左键点击唤出窗口；tooltip 显示未读数。
Windows 通知**由 Rust 侧调用**（`tauri-plugin-notification`）—— 前端没有打包器，插件的 JS 包
进不来，而推送本来就发生在 Rust 里。

---

## 附件

**下载**：读信时点附件标签即保存到 `下载\Easy Email\`，文件名由后端消毒后决定。
**文件名是敌意输入**：`filename="..\..\Startup\x.exe"` 是发件人可以随便写的合法值，所以名字会被
压成单个路径分量、替换掉 Windows 不接受与所有控制字符、超长截断、`CON`/`NUL` 这类设备名加前缀，
空名则退回 `attachment-<part>`。已存在的文件不会被覆盖，而是加 ` (2)` 后缀 —— 静默覆盖会丢掉
用户可能还没看过的文件。

**已知代价：下载会把整封邮件取回来**，所以峰值内存与**邮件大小**成正比，而不是与附件大小成正比。
更省的两条路都试过并否掉了：IMAP 分部取件（`BODY.PEEK[]<0.196608>`）的响应 `imap-proto` 0.17
解析不了（section 语法里没有 `<origin.offset>`，服务器按 RFC 回显后直接报 `TakeWhile1`；
让夹具不回显能让测试变绿，但会在每一个真实服务器上失败）；按 part 路径取单个 part 则需要靠遍历
MIME 树去推导 `mail-parser` 不暴露的点分路径，那正是会在真实邮件上崩的"聪明代码"。
所以选择一次取回、用 `mail-parser` 已经解好码的字节直接落盘。

**上传**：写信窗口点「📎 添加附件」选本地文件；文件**从磁盘读取**，不经过界面 ——
`OutgoingAttachment` 传的是路径，`smtp` 在组装时才读，所以 20MB 的附件不会先变成一串 JSON
穿过 IPC。MIME 类型按扩展名猜；猜不出来就用 `application/octet-stream`，**猜错的类型不会中止发送**。
有附件时是 `multipart/mixed`，第一个 part 是正文（有 HTML 时是 `multipart/alternative`）；
纯文本正文**直接放进去**，不套一层只有单个 part 的 `multipart/mixed`。
选取时按 `字节 × 4/3 + 4096` 估算上限，发送前再用真实编码后的字节拦一次（`max_send_bytes`）。

真机验证（QQ 邮箱）：下载——真实发票 PDF 存下，声明长度 / 写入长度 / 磁盘长度三者一致
（23 477 字节），文件头 `%PDF-`；上传——40 KB 非 7bit 内容经真实 SMTP 发出、再从 IMAP 取回，
**逐字节一致**。

---

## 对话模式

列表把同一个会话折成一行（行上带封数）。点开它、并且顶部按钮处于 **💬 对话模式** 时，
整个会话按气泡铺开：对方在左、自己的邮件在右（**跨文件夹**取的，所以只存在于「已发送」里的回复也在）。
**对话模式对每一封都适用** —— 只有一封时也是「一封的会话」，照样按气泡显示。

**怎么算"同一个会话"**：两条信号**并集**。
① `References` / `In-Reply-To` 链（`crates/mail-core/src/threads.rs` 取根）——事实；
② **同一发件人 + 同一主题**（去掉 `Re:` / `Fwd:` / `回复：` 等前缀）且**相邻两封间隔不超过 7 天**——启发式，
  只对**你收到的**邮件生效。所以你收到的那四封同主题、彼此不是回复的信会并成一行；
  而"同一个主题、下个月又来一封"不会（Google 的「安全提醒」在真实邮箱里 24 封跨 4.7 年，
  不加时间窗会被并成一行，加窗后是 15 段）。两条信号并集就是为了**只增不减**：
  今天能合并的绝不会因为这个猜测而被拆开。

出现"该合没合 / 不该合却合了"时，先跑 `python tools/inspect-cache-detail.py <对方地址>`：
它会打印每封的 `thread_key`、`subject_key` 与线程头，一眼就能判断。

**默认是普通模式**（✉ 普通模式 / 💬 对话模式 一键切换，选择记在本机）。

- **所有气泡默认折叠**，只显示发件人、时间和一小段正文预览；**点一下展开**，**点它的头部再收起**
  （头部有 `▸ / ▾`）。展开气泡的正文里点击不会误收起 —— 那是一份你正在读、正在选字的文档。
- **同时最多展开一封。** 这不是省事：界面上始终只有**一个**沙箱正文渲染面（`#body-surface`），
  它会在展开的那封里"就位"。两个渲染面就意味着「显示图片」的解锁能从一封泄漏到另一封，
  而 20 封的会话也不该有 20 份渲染器。
- **底部可以直接回复**：输入内容 → 发送（或 `Ctrl+Enter`）。它走的是同一个发件箱，
  所以 5 秒撤销窗口、"关掉应用也不会丢"、"崩溃后按冻结的 Message-ID 查重、绝不重发"这些保证一个不少。
  回复框上方一行会**先告诉你这封回给谁**：优先回答你展开的那封，否则回答会话里最新的一封**不是你自己**的邮件
  —— "回最新一封"在"最后说话的是自己"时会把信发给自己。
- **发出去就当场出现**：回复进的是发件箱，而对话读的是缓存，所以在服务器副本同步回来之前，
  它会以**虚线气泡**出现在会话底部（文字跟着队列状态走：发送中 / 已发送 / 发送失败），
  撤销会把它收回。重新打开会话时以缓存为准。
- 回复会带上 `In-Reply-To` 与完整的 `References` 链（规则在 `src/reply.js`，由 `tools/reply_test.mjs` 与门禁钉住），
  否则在对方那里它就是一个全新会话。

真机验证（QQ 账号，真实 35 封会话）：首屏 0 封展开；展开/收起/换一封都正确；
展开第二封时第一封收起；底部显示"回复给 correspondent@example.com"；空内容点发送只得到提示、
发件箱没有多出任何东西；全场始终只有 1 个正文渲染面。

---

## 安全设计

- **密码不进配置文件。** 账号信息写 `%APPDATA%/easy-email/accounts.json`，
  密码/授权码进系统凭据管理器。
- **邮件正文用双重沙箱渲染。** `iframe sandbox`（禁脚本、禁表单、禁同源、禁顶层跳转）
  加独立 CSP `default-src 'none'`。**远程图片默认不加载** —— 那正是追踪像素上报"已读"的方式。
  内联样式和 `data:` 图片保留，所以邮件排版和嵌入 logo 正常。
  "显示图片"只为**当前这一封**解锁（切到别的邮件会立刻恢复拦截），而且必须点一次才生效。
- **CSP 的约束关系是"取交集"，而且 `srcdoc` 文档会继承父文档的策略。**
  所以外壳的 `img-src`（`index.html` + `tauri.conf.json`）不能比正文策略更严，
  否则正文策略再宽松也没用。真正的把关点在 `buildSandboxedDocument()` 的默认分支，
  `check.ps1` 有一项专门盯着它别被改坏。
- **CSP 锁死渲染进程**：脚本只能来自本地，`connect-src` 只留 IPC。
- **凭据按平台分离保存**：Windows 走凭据管理器（密码）+ DPAPI 用户态加密文件（刷新令牌）；
  Linux 走 D-Bus Secret Service（密码）+ 机器标识派生密钥加密的 `0600` 文件（刷新令牌）。
  日志里永远不出现令牌或密码（`tools/check.ps1` 有一项专门拦这个）。
- 报告安全问题见 [SECURITY.md](SECURITY.md)，第三方组件许可清单见
  [THIRD-PARTY-NOTICES.md](THIRD-PARTY-NOTICES.md)。

---

## 已知限制 / 下一步

- **只监听 INBOX。** 其他文件夹有新邮件不会触发通知。
- **托盘图标的位置由 Windows 决定**，可能被折叠进"隐藏的图标"溢出区。
- **缓存只覆盖同步窗口内的邮件**（默认最近 2 年，可在侧栏「设置 → 同步窗口」改）。列表一次显示 50 封，
  「加载更多」**先把本地缓存里剩下的那几页显示出来**（所以离线也能往回翻），缓存翻完了才向服务器要下一页；
  每次只取**一页**（`message_page(offset, limit)`）——不联网、不刷新，也不必把已显示的窗口整个重取一遍；
  两边都没有了才会显示"没有更早的邮件了"。
- **Outlook / Microsoft 365 只能走 OAuth2**（微软 2024-09-16 起对个人账号彻底关闭基本认证）。
  推荐路径：向导里选 Outlook，展开「高级」，填**你自己的** Azure 应用注册的客户端 ID，再点
  「打开浏览器登录」。个人 outlook.com 账号没有 Azure 目录、注册不了应用时，旁边还有一个
  **「试用（借助 Mozilla 的注册）」**按钮：它借用 Mozilla 公开发布在 Thunderbird 源码里的注册，
  授权页会显示「Mozilla Thunderbird」而不是本应用，Mozilla 随时可撤销 —— 仅供试用，正式使用
  请换成自己的。**这条路（含试用）还没有在真实 Outlook 账号上走通过**，注册步骤见上面
  「真实账号手工验证」。
  能说的保证到什么程度：这条路的**线上契约**已按微软官方文档逐条核对并钉进测试
  （`the_microsoft_flow_matches_the_published_imap_smtp_oauth_contract`：三个资源 scope、`offline_access`、
  `common` + v2 端点、PKCE S256、公共客户端**不带** `client_secret`、回调用 `http://localhost` 且匹配时忽略端口、
  以及**用文档里那个 XOAUTH2 示例的 base64 做逐字节比对**）。
  文档核对替代不了的只有三件：**同意页上你真的点了允许**、**租户/账号允许 IMAP 与 SMTP**、**浏览器里授权的
  账号和你填的是同一个**（最后这条程序会自己核对并当场说明）。
  另外注意：很多 **M365 租户把 SMTP AUTH 关掉了**，那种账号能收信、不能发信，
  程序会明确说"需要在 Microsoft 365 管理中心开启 SMTP AUTH"，而不是让你去找一个**并不存在**的授权码。
- **自定义 / 自建服务器只做了手动层，没有自动发现。** 域名 autoconfig / ISPDB / DNS SRV /
  Autodiscover 都没做：它们要向第三方发出用户域名的请求（`design/05` §11 反驳 5），
  而且需要网络层的额外工作。表外服务商必须自己知道服务器地址。
  地址会被清洗（`imaps://imap.example.com/` 这种粘贴会被化简），端口、路径、空格写错会当场拒绝，
  `不加密` 只允许本机地址 —— 这几条规则的**唯一出处是 Rust**（`normalise_host` / `validate_endpoint`），
  前端不抄第二份。
  填自定义服务器时可以勾「**这个服务器不提供 SMTP（只收信，不发信）**」：勾了之后 IMAP 通过就保存，
  该账号从写信入口会被明确拒绝；**没勾时行为和以前完全一样**（SMTP 失败照样拒绝保存）。
  只发信不收信的服务器**仍然加不进来**。
  自定义账号不参与服务商表的所有策略：`daily_download_budget()`、`append_limit_bytes()`、
  `smtp_max_message_bytes()` 对它都返回 `None`（"未知"），所以配额节流与发送前的大小预检对它不生效 ——
  这是如实表示未知，不是漏洞。
- **下载附件时峰值内存与整封邮件同量级**，原因见「附件」一节。
- **发送大小预检只覆盖写信那条路**：归档副本那条路的 `append_limit_bytes` 预检仍未做。
- **正文不落盘**（侧栏「设置 → 存储与清理」可切）已在真机配对实测过：同一封邮件在不落盘时，
  正文片段在 `cache.db` 与 `-wal` 里都搜不到，切回"缓存在本地"后出现。**但"同时清空已下载正文"
  那一步没在真实账号上试过**（要删 482 封真邮件），只有临时库的字节级测试。
- **删除是永久删除**（`UID EXPUNGE`，只在服务器不支持 UIDPLUS 时才退化成 `EXPUNGE`）。
  还没有"移到垃圾箱"的自动行为。
- **本地缓存可以手动清理**：侧栏「设置 → 存储与清理」→ 看占用、设上限、「看看会清什么」（干跑）、
  确认后才「开始清理」，并留一条「最近清理」记录（未读邮件与旗标永不清）。
  **但还没有定时任务**——没有任何东西会自己删你的数据，清理永远是你按下按钮才发生。
- **日志只能看当前文件**：轮转后的 `.1`…`.7` 不在「查看日志」里（要看更早的得自己去数据目录翻）。
- **断网时的整理操作先本地生效、再排队补做**（标已读/删除/移动，`design/04` §5.5）：
  恢复联网后**先发信、再补旗标**，每条意图幂等所以重放安全；界面暂时不弹"已排入离线队列"的提示
  （本地即时生效、静默返回成功）——这一条已知，未当成已完成。
- **`@foxmail.com` 别名**：QQ 号地址能登录，同一授权码下 foxmail 别名被拒。已有「登录用户名」覆盖字段，
  但**没有实测确认**哪种填法能通。

---

## 许可与致谢

本项目以 [MIT](LICENSE) 发布。第三方组件与各自许可的完整清单见
[THIRD-PARTY-NOTICES.md](THIRD-PARTY-NOTICES.md)；安全相关问题见 [SECURITY.md](SECURITY.md)；
想参与开发见 [CONTRIBUTING.md](CONTRIBUTING.md)（一句话：**修了行为就要有能抓住那个 bug 的测试**）。

内置的 21 个服务商配置来自各服务商公开文档与登录帮助页的表述，不含任何商标图形；
OAuth 路径不内置任何第三方客户端注册——Gmail 与 Microsoft 365 都要求用户使用自己注册的应用。
