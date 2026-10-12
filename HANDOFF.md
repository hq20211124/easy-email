# HANDOFF

给下一个会话。**只记录从这个仓库里读不出来的东西** —— 架构、模块职责、为什么这样写，
都在代码注释和 `README.md` 里。先读 `README.md`，再读这份。

---

## 0. 交接快照（2026-10-09，交接前必读这一节）

### 0.1 仓库与发布状态

- 公开仓库：**https://github.com/hq20211124/easy-email**（MIT）。本地 `/home/myroot/my-projects/easy-email`
  就是它（同一份历史，`origin` 已指向，WSL 全局配了 Linux gh 的凭证助手）。
- 历史**清洗过**：`bestbychinas@gmail.com`、`facai2411923@126.com`、`forwork8888@foxmail.com`、公网 IP
  `111.55.214.44` 在全部提交里都已被 filter-repo 替换为占位符。**别再把这些串写回任何文件**。
- 已发布：**v0.1.0**（首发）、**v0.1.1**（已读未读标识）。Release 资产由 CI 自动构建：
  `Easy Email_<ver>_x64-setup.exe`（Windows/nsis）+ deb / AppImage / rpm（Linux）+ `SHA256SUMS`。
- **版本号必须手动升**（`Cargo.toml` 与 `src-tauri/tauri.conf.json` 两处），CI 不碰它。
  忘了升 = 产物还叫上一个版本（v0.1.1 第一次就是这么发错的，已作废重发）。流程写在 README「发版（维护者）」。
- 打 tag 后要 `gh release create vX.Y.Z --title … --notes-file … --latest`，否则只有 tag 没有 Release 页。

### 0.2 本次（相对上一版交接）新增/改动的平台层

Windows 行为**没有**任何退化，全部改动都在 `cfg` 分支后面：

- `crates/mail-core/Cargo.toml`：`rusqlite/serde/serde_json/thiserror/tokio` 从 `cfg(windows)` 移回公共段
  （原先只声明在 Windows 段却无条件引用，Linux 编译不过）；`keyring` 按 target 拆 ——
  Windows `windows-native`，其余 `sync-secret-service` + `crypto-rust`（D-Bus/libsecret）。
- `tokenstore.rs`：非 Windows 不再是"必失败"。改为机器标识（`/etc/machine-id`，回退
  `~/.config/easy-email/machine-key`）派生 SHA-256 流密钥 + 8 字节完整性 tag 的文件加密，
  目录 `0700`、文件 `0600`、tmp+rename 保留；`available()` 恒真，五个用例全平台真跑。
- `oauth.rs`：`SystemBrowser::open` 非 Windows 用 `xdg-open`（argv 直传，不经 shell）。
  `SO_EXCLUSIVEADDRUSE` 仍是 `cfg(windows)` 分支，**没有**为 Linux 加等价 socket 选项（不需要：
  未设 `SO_REUSEADDR` 的 socket 在 Linux 上同样拒绝二次绑定，测试 `another_process_cannot_take_the_port…` 已覆盖）。
- `main.rs`：`#![windows_subsystem]` → `#![cfg_attr(windows, windows_subsystem = "windows")]`；
  非 Windows 的 `--console` 只打印两行标记。
- Tauri 配置按平台拆（Tauri v2 自动合并，数组整体覆盖）：`tauri.conf.json` 放跨平台字段，
  `tauri.windows.conf.json`（nsis、webviewInstallMode、`additionalBrowserArgs`）与
  `tauri.linux.conf.json`（deb depends）为 override。
- `tools/check.ps1`：nsis 不变量改读 `tauri.windows.conf.json`；PE subsystem 检查在非 Windows 主机跳过。

### 0.3 最近两个功能块

1. **公开化**（`22812e7`、`b0a42aa`）：Linux 支持 + 历史清洗；门面补齐（badge、安装矩阵、
   `SECURITY.md`、`CONTRIBUTING.md`、自动生成的 `THIRD-PARTY-NOTICES.md` —— 439 个 crate 全量清单，
   含 MPL-2.0 / `self_cell` 双许可选边 / ring 的 NOTICE 传播）。
   注意 `reports/` **在 .gitignore 里**（许可审计是给维护者的工作文档，不随仓库发）。
2. **已读未读标识**（`59c6232`）：`Cache::unread_counts`（按文件夹，0 的不出现）+
   `folder_unread_counts` 命令 + 侧栏徽标（99+）+ 列表/会话/搜索行圆点（已读行保留隐形占位点防跳动）。
   刷新点是"重算"而不是"减一"：会话跨文件夹时，数属于当初取回它的那个文件夹。

### 0.4 两台机器的用法（按两台电脑思考，别跨文件系统编译）

- **WSL（Linux）**：rustup 装在 `~/.cargo`，工具链 stable（1.99），crates 走 tuna 镜像
  （`~/.cargo/config.toml`）。系统依赖已 apt 装好（webkit2gtk-4.1、gtk3、soup3、libdbus、rsvg、
  appindicator、build-essential、gcc-mingw 用于 windows-gnu 交叉 check）。
  GUI 跑：先 `systemctl --user start dbus.socket`（已建 default.target.wants 符号链接，重启仍在），
  然后 `./target/debug/easy-email`。**截图抓不动**（WSLg weston 不支持 wlr-screencopy，import -window root 也失败）——
  要视觉证据只能让用户在 Windows 桌面侧看，或用 Win+Shift+S。
- **Windows**：`/mnt/c/Users/72492/.cargo/bin`（rustc 1.98 MSVC）可用，但从 WSL 编译
  `/home/myroot/...` 下的源码**必失败**（9p 文件系统 + rustc 增量锁）。历史上是在
  `C:\Users\72492\easy-email-win` 同步一份副本编译测试的 —— **这已被用户明确否决**：
  约定改为「先在 WSL 提交并推送，Windows 侧自己 pull 后编译」。
- **gh 用 Linux 那个**：`/home/myroot/gh/bin/gh`（token 带 `workflow` scope，能推 workflow 文件）。
  `/mnt/c/Program Files\GitHub CLI/gh.exe` 是另一个 token（没有 workflow scope，推 `.github/workflows/` 会被拒）。

### 0.5 下一步（用户已有完整路线图，见 `design/19-roadmap.md`）

阶段表：M0 ✅ / M1a 大部分 ✅（差 Outlook 真机验证〔外部条件：Azure 账号〕、代码签名〔外部条件：
证书费用〕；~~自动更新~~ 已在 v0.1.2 完成）/ **M1b 已起步**（09 搜索树：正文索引 v0.1.2、
附件名索引第 71 轮；其余块未开始）/ M2、M3 ❌。
README「已知限制」是 Issue 级清单。~~候选里最小最独立的是离线提示；最值钱的是正文全文索引~~
——两者都已在 v0.1.2 做完（V2-B 失败可见性 / V2-A 后台正文索引），**不要再从这两件起步**；
**附件名索引（第 71 轮）与索引重建 UI（第 72 轮）也已完成**，09 树里剩下的是
`index_queue` 队列、FTS 损坏的自动探测/heal、查询语法（`from:`/`has:attachment` 等）、服务器端回退
（见 `design/09` 顶部那张实现状态表，它按目标逐条标了 ✅/❌，别再从已完成的那几项起步）。
`design/19` §4 有各块验收。另有三件卡在外部条件的真机验证（Outlook / 163·126 / 自建服务器，
清单见 §7 ④），不拦发版但每次接手时值得催一次。

下一批：**给 P3 拿真窗口基线**（需要你点：应用带 `--remote-debugging-port=9222` 构建 →
`powershell -File tools\bench-app.ps1`，P1/P3/P5/P6 四个数字进 `target\bench-app-history.jsonl`）；
**09 的语法三片已完成**（第 77/78/79 轮：文本前缀、结构化谓词、大小前缀 + 打错有提示），**04 离线意图已收口**（第 80/81 轮）：队列 + 有界重放 + 三个命令接线（仅网络类失败入队、本地即时生效）+ 每 30 秒一遍的 worker（崩溃 requeue、outbox 先于 flags）。**16 第二片已做**（第 83 轮：`log_line` 经 `format_log_line` 过 8 类脱敏，53 个调用点一次性覆盖；脱敏行尾注明"[已脱敏 N 项]"；剩余四个裸写出口在门禁里逐个记账）。剩下：诊断包本体（§3.3，含 preview_only 与清单）、日志轮转/tail、保存搜索（M2），以及**真机的 §9 验收**（断网混合操作 → 恢复 → 服务器一致，需要你点一次）。
**09 查询语法两片都已完成**（第 77/78 轮）：文本前缀 + `has:attachment` + 结构化谓词
（`after:`/`before:`/`is:unread`/`is:flagged`/`in:`/`account:`）+ 打错有提示（`search_hints`）；
解析住在独立的 `crates/mail-core/src/query.rs`，分词原语 `clauses_of` 与索引共用。
**第 78 轮的三个坑**（都是自己的测试当场抓住）：`account:` 写成叠加 ⇒ 与调用方作用域自相矛盾、
永远 0 行；EXISTS 不绑定却占占位符 ⇒ 后面的谓词全体错位；批量追加测试时把 cache.rs 截断、
丢了 36 个测试（是 PASS 数变小发现的）。**批量改文件要么只插入、要么先 `git checkout` 兜底。**
性能预算的**覆盖率**已变成机器检查（第 73 轮：`bench.ps1` 打印全部 16 项、自断言条数、
`check.ps1` 钉住），实测 P2/P4/P7/P9/P10/P12/P13/P14/P15，未测 P1/P3/P5/P6/P8/P11/P16 各自写了原因
——**别再把"没测"写成"有"，也别把"有仪表"写成"没仪表"**：第 73 轮我连错两次
（只读前 40 处 `println!` ⇒ 把实测的 P15 写成未测；只读 `bench.ps1` ⇒ 没看到
`tools/bench-app.ps1` 这个 P1/P5/P6 的测量仪）。**`bench.ps1` 的覆盖表现在是唯一权威**，
它每次都打印 16 项并自断言条数；盘点请读它，别读源码片段。
**P14 已知超标**（≈170% vs 预算 80%，逐字分词撑大中文文本，结构性、待拍板）——
改动索引前后请对照 `bench.ps1` 的这一行，别把它当成回归。
**门禁的两个洞已在第 73 轮补上**：`.ps1` 含中文必须有 BOM；**`Step` 会捕获异常**，
抛异常的步骤记 `FAILED` 而不是让整轮"全绿"。

**用户的工作方式**：先要一份可照做的 PLAN.md 再动手；在意 token 成本；不要重新读已注释过的库源码。
**未验证的事不要写"完成"**；测试必须能抓住它修的那个 bug。

---

## 1. 关于库 API：不要重新探测

`async-imap` / `async-native-tls` / `mail-parser` / `lettre` 的坑**已经全部写进代码注释和 Cargo.toml 注释了**。
不要再 `cargo fetch` 之后去 `~/.cargo/registry/src/...` 里翻源码 —— 那会烧掉大量 token，
而且结论和现有注释一致。要点位置：

- `crates/mail-core/Cargo.toml` —— 为什么两个 crate 都要 `default-features = false` + `runtime-tokio`
- `crates/mail-core/src/imap.rs` 顶部 —— 为什么没有 `connect()`、为什么要 `DebugStream`
- `crates/mail-core/src/smtp.rs` —— `body()` 不写 Content-Type、`references()` 是覆盖

真的需要查 API 时，**优先看编译错误**，其次才看源码。

## 2. 门禁：`.\tools\check.ps1`

前端语法（`src/*.js` + `tools/*.mjs`）→ index.html 引用的资源是否真的存在 → 前端 id 引用 → 正文默认仍然拦截远程图片
→ 正文的宽度规则仍然放过百分比布局 → 本地搜索仍然用能搜中文的分词器 → **前端调的每个命令都已定义且已注册**
→ **会话分组** → **向导对"验证没过"的判决**（`tools/wizard_verdict_test.mjs`）→ 样式表的 id 必须存在
→ auth 轴三条承诺 → 安装包配置 → `fmt` → `clippy -D warnings` → `cargo build`（抓 linker 警告，clippy 看不到）
→ `cargo test` → **负载契约**（`payload_keys.mjs`，含 `EndpointOverride`）→ **exe 的 PE 头 Subsystem 必须是 2**
（GUI，否则会有 cmd 窗口）→ **不得留下 `--remote-debugging-port`**（CDP 探针要用，但留下等于把 webview 交给
任何本机进程；这条**曾经被人忘在配置里并提交**，所以现在是机器检查）。
目前 **34 步**（33 离线 + 1 `-Network`）、**WSL 375 passed / 7 ignored，Windows 376 / 7**
（同一提交 `b623b61` 实测：mail-core lib 311〔WSL〕/312〔Windows〕，Windows 恒多一个
`cfg(windows)` 单测；loopback 50、tauri lib 8、reachability 4、oauth 1、bench 1）。
**v0.1.2 已发布**（2026-10-10）：三个 job 全绿，四个安装包 + `.sig` + `latest.json` 齐全，
`releases/latest` 解析到 `0.1.2`。
**`check.ps1` 已在真 Windows 上跑过**（2026-10-10 晚，第 70 轮；Windows PowerShell **5.1** ——
本机没有 `pwsh`，`powershell.exe` 从 WSL interop 调用即可；UNC 起始目录那句 CMD 警告无害）。
**第一次真跑就抓到门禁自己的两个缺陷并已修**：① `$IsWindows` 是 PS 7+ 变量，5.1 下恒 `$null`，
导致"PE subsystem = 2"那步在真 Windows 上自报非 Windows 跳过还打 `ok:`（改成
`[Environment]::OSVersion.Platform -eq 'Win32NT'`）；② `Get-Content` 默认按 ANSI/GBK 解码，
两处中文判据此前是哑的（29 处读取全部显式 `-Encoding UTF8`，并新增第 31 步把这两条钉死）。
复跑配方（复用别重新摸索）：克隆到 Windows 原生路径的一次性副本 → `.cmd` 里 `chcp 65001` →
`vcvars64.bat`（在 `Program Files (x86)\Microsoft Visual Studio\2022\BuildTools`）→
`[Console]::OutputEncoding=UTF8` 后跑门禁 → 全程重定向日志；`TEMP/TMP` 显式设成 Windows 目录。
**口径**（别再读成矛盾）：365/366 = 全 workspace（tauri lib 8 + mail-core lib 301〔WSL〕/302〔Win〕
+ loopback 50 + reachability 4 + oauth_flow 1 + bench_fixture 1）；PROGRESS 里某些轮次写的 29x
只数 mail-core 的 lib 单测。加 `-Network` 会额外跑 40 个真实端点（约 40 秒）。

改任何东西后跑它。加新 crate 依赖后也要跑，因为 `cargo build` 那一步会抓 MSVC linker 警告
（Tauri 模板默认的 `crate-type = ["staticlib", "cdylib", "rlib"]` 会在每次构建时产生一条，
桌面上只需要 `rlib` —— 已经改掉了）。

## 3. 这个环境的事实（不看会踩）

- **屏幕 2160×1440 物理**（DPI-aware 实测）。应用窗口约 1942×1256 物理，**webview 的 CSS 视口是
  993×621**，`devicePixelRatio` ≈ 1.935；三栏实际分到 **180 / 244 / 569**（2026-10-02 收窄侧栏、
  给读信栏让位之后的值）。
  注意：**DPI-unaware 进程（Windows PowerShell 5.1）看到的是 1440×960** —— 早前那份"720×480 逻辑像素"
  的记录就是被这个误差带偏的，别再照着它判断布局。
- **前端资源在编译期嵌进二进制**，运行时不再读磁盘。改 `src/` 下任何文件都必须重新 `cargo build`
  （实测：把 `src/theme.js` 移走、不重新构建直接启动已构建的 exe，界面照样完全正常）。
- **Gmail 从这台机器是"间歇性可达"，不是"连不上"**（2026-10-04 实测更正）：
  `imap.gmail.com:993` 5 次里 2 次通、`smtp.gmail.com:465` 2 次里 1 次通。收信时好时坏、发信基本不通。
  可达性测试特意把这种情况判成"不可达告警"而不是失败，**别把它改回硬失败**。
- **本机有系统代理（`ProxyEnable=1`、`ProxyServer=127.0.0.1:7897`），而应用必须走它。**
  `reqwest` 是我们唯一直连外网的客户端（OAuth 换令牌），而它的 **`system-proxy` 特性是载荷性的**：
  `default-features = false` 会把它一起丢掉，于是 reqwest 不去读
  `HKCU\Software\Microsoft\Windows\CurrentVersion\Internet Settings`（Edge 用的同一份设置），
  浏览器能打开 Google 而应用 30 秒超时 —— 症状是"浏览器显示登录完成、应用说没成功"。
  **别为了减小依赖树把它去掉**；`check.ps1` 抓不到它，只有真机登录能抓到。
  另外 IMAP/SMTP 是裸 TCP，**不**走 HTTP 代理：想稳定收发 Gmail 需要代理开 TUN/虚拟网卡模式。
- **cargo 走清华 tuna 镜像**（`~/.cargo/config.toml`）。`cargo search` 不可用（会报
  "crates-io is replaced with non-remote-registry source"），要查版本用 `cargo add` 让它自己解析。
- 工具链：rustc 1.98 (MSVC) / VS2022 BuildTools / WebView2 154 / Node 24。
- **`pwsh` 工具实际跑的是 Windows PowerShell 5.1**（`$PSVersionTable.PSVersion.Major` 是 5）。
  后果：`Set-Content -Encoding utf8NoBOM` 不存在；`Get-Content` 默认按 GBK 解码，
  所以读 UTF-8 中文文件要显式 `-Encoding UTF8`，往文件里写中文要用
  `[System.IO.File]::WriteAllText($p, $t, (New-Object System.Text.UTF8Encoding($false)))`。
  含反引号转义的命令串在这个传输层上容易被弄坏，**优先用单引号 + `[regex]::Replace`**。
- **重新构建后启动应用，要等足够久再连 CDP**：`Start-Sleep -Seconds 8` 有时不够
  （WebView2 还没把调试端口打开 → `ECONNREFUSED`），而且窗口还在自适应尺寸，此时截图会是错的尺寸。
  等 10~12 秒，或者先轮询 `http://127.0.0.1:9222/json/version`。
- **库 API 的答案记录在这里，省得再查一遍**（都是编译错误或一次窄范围查询换来的）：
  - `imap-proto` 0.17 的 RFC 6154 特殊用途属性是 **`NameAttribute` 的直接变体**
    （`All` / `Archive` / `Drafts` / `Flagged` / `Junk` / `Sent` / `Trash`），**不是** `Attribute::*` 扩展。
  - `NameAttribute` 上同时挂着 `NoSelect`（已有代码在用）。
  - `async_imap::types::Mailbox` 的 `UIDVALIDITY` 是**公开字段** `uid_validity: Option<u32>`，
    不是方法 —— 写 `selected.uid_validity()` 会编译不过。
  - **`rusqlite::Connection` 是 `!Sync`**（内部有 `RefCell`）。因此**任何跨 `.await` 持有
    `&Cache` 的 future 都不是 `Send`**，而 Tauri 命令的 future 必须是。
    换 `tokio::sync::Mutex` 也没用：它的 guard 只有 `T: Sync` 时才是 `Send`。
    解法见下一节。
  - **Tauri 命令的返回类型必须是 `IpcResponse`**，也就是里面每个类型都要 `Serialize`。
    漏一个会报成 `the method `async_kind` exists ... but its trait bounds were not satisfied`，
    很容易误读成 `Send` 问题 —— 看完整错误里的 `IpcResponse` 那一行才是真因。
  - **（2026-10-03 更正，M0-1 探针实测推翻）async-imap 0.12 的 IDLE *会* 投递 `* n EXISTS`。**
    旧结论（"`wait_with_timeout` 把它归到 `Response::Data { status: Ok, .. }` 那一支并 `continue`，
    所以 `NewData` 永远不会为新邮件触发"）**是错的**，实测与源码都不支持它：
    `MailboxData(Exists)` 落到通配分支 ⇒ `IdleResponse::NewData` 在推送后约 **0.2 秒**就返回
    （`tests/loopback.rs::probe_idle_delivers_exists_before_the_window_closes`，5 秒窗口下断言 < 2 秒）。
    ⇒ 通知延迟**不再由窗口决定**：窗口只是心跳与"连接还活着吗"的兜底（设计里定为 **120 s**），
    及时性来自推送本身。判断"有变化"时仍然要刷新基线，否则一次推送会被心跳再触发一次同步。
  - **`setup()` 不在 Tokio 运行时上下文里。** 在那里 `tokio::spawn` 会 panic
    （`there is no reactor running`）—— 而且因为没有控制台，现象就是"应用启动即消失"。
    用 `tauri::async_runtime::spawn`，它的 `JoinHandle` 也有 `abort()`。
    顺带说明：`install_panic_log` 记下的那行 panic 就是查出这个的唯一线索，别把日志钩子删掉。
  - **`for<'a> Fn(&'a mut T) -> BoxFuture<'a, _>` 这种约束闭包满足不了。** 闭包只要捕获了
    任何比 `'a` 短的引用，编译器就会要求它 `'static`。所以池子的"失败重试一次"写成了宏
    `mail_core::pool_op!`（把块展开两次），而不是接受闭包的函数。**不要改回函数形式。**
  - **IMAP 分部取件的响应解析不了，别再试。** `BODY.PEEK[]<start.length>` 请求合法，
    但服务器按 RFC 回显 `BODY[]<start.length>`，而 `imap-proto` 0.17 的 section 语法里
    **没有 `<origin.offset>`**，解析器直接报 `TakeWhile1`（实测）。让循环夹具不回显那个后缀
    能让测试变绿，但真实服务器都会回显 —— 那是"测试通过、线上失败"，比不做优化更糟。
    所以附件下载走了"整封取回 + 取 part"的路（峰值内存与邮件同量级，已写进 README 的已知限制）。
    `fetch_message_range` 保留着并带 `#[allow(dead_code)]`，注释写明了原因，就是防止有人再试一遍。
- **在沙箱 iframe 的隔离世界里，定时器不会触发。** 所以 `--frame=0` 的探针里**不能用 `await` /
  `setTimeout`**，一用就永远不返回、把调试器挂住。需要等待的地方改成同步：改完样式直接读
  `scrollWidth` / `getBoundingClientRect()` 会强制同步重排，本来就不需要等。
  真要等（比如图片加载），把探针放到**主 frame** 跑，那里的 `await` 是好用的。
- 抓运行中窗口截图用 `.\tools\capture-window.ps1`。它有两个必须保留的修正，别当成可选：
  1. **`SetProcessDPIAware()` 必须在任何窗口/GDI 调用之前调用。** 否则在 2160×1440 的屏上，
     DPI-unaware 进程看到的是虚拟化的 1440×960，`GetWindowRect` 只报 1295 宽，
     `CopyFromScreen` 于是**只拍到窗口左边三分之一** —— 这正是上一轮反复误判布局的原因
     （读信栏"被压成 0"其实是根本没拍进去）。
  2. **topmost 要一直保持到截图完成再释放。** 早前的写法是先 topmost→再取消→再截图，
     结果拍回来的是别的窗口（本机是 DSH 自己），除非前台窗口碰巧就是目标。
- **要"为什么"而不只是"什么样"，用 `.\tools\inspect-dom.mjs`（CDP）。** 支持三种用法：
  默认探针、`@file` 从文件读表达式（多步探针：点邮件→等待→再读 DOM，用 shell 引号传会很痛苦）、
  以及 **`--frames` / `--frame=0 @file` 进入子 frame**。
  后者是查看邮件正文的唯一途径：正文在 `sandbox` iframe 里、没有 `allow-same-origin`，
  应用自己的 JS 碰不到它，只有 CDP 不受同源策略限制。
  **用之前要临时把 `--remote-debugging-port=9222` 加回 `additionalBrowserArgs` 并重新构建，
  验完必须删掉**（那个端口任何本机进程都能驱动 webview）。
  **加的位置有讲究（这次踩了一次）**：`additionalBrowserArgs` 是**空格分隔**的命令行串，
  而 `--disable-features` 的**值**才是逗号分隔的。把 `,--remote-debugging-port=9222` 接在那个值的末尾，
  等于把它塞进了特性列表：构建成功、应用照常启动、端口**不开**，而且没有任何报错。
  正确写法（也是历史提交里的写法）：
  `"--remote-debugging-port=9222 --disable-features=msWebOOUI,msPdfOOUI,msSmartScreenProtection,AutofillServerCommunication"`。
  另外**先 `Stop-Process easy-email` 再启动**：正在跑的旧进程没有这个端口，而且看起来"应用明明是开着的"。

## 4. 工作方式（用户明确表达过）

用户喜欢：**先写一份 PLAN.md（机械到可以直接照做的程度），然后切到最低思考强度执行**，
执行完删掉 PLAN.md。所以被要求"先做计划"时，计划要包含：确切的文件/函数、替换前后的完整代码、
逐条验收命令和预期输出、以及一段"不需要动的东西"防止误改。

用户在意 token 成本。避免：重新读已经读过的源码、输出大段日志、重复验证已经验过的东西。

## 5. 决策记录

- **技术栈是用户从三选一里选的 Tauri + Rust**（备选 Electron+TS、C#+MailKit）。
- **明确不做**：自建邮件服务器、服务端 Sieve 界面、完整所见即所得 HTML 邮件设计器、
  把 POP3 当主线。这些是讨论过并否决的，不要重新提议。
- **Outlook 个人版 v0.1 放弃了**：微软 2024-09-16 起对个人账号彻底关闭基本认证，
  只能走 OAuth2。`providers.rs` 里已经把这条写进 `setup_note`，`auth: OAuth2` 会让 UI 显示
  "当前版本尚不支持"。要做的话是 v0.3 的事。
- 安全底线，不要为了省事绕过：密码只进 Windows 凭据管理器；邮件正文只在
  `iframe sandbox` + `CSP default-src 'none'` 里渲染；**远程图片默认不加载**。
- **主题是"应用自己说了算"，不是跟随系统。** 用户明确要求默认白天模式。原来那个
  `@media (prefers-color-scheme: dark)` 已删除 —— 它优先级高于任何 class/属性选择器，
  留着的话切换按钮点了也没用。现在由 `src/theme.js` 在 `<head>` 里同步写 `data-theme`
  （独立文件是必须的：CSP 是 `script-src 'self'`，内联脚本会被拦）。
  **`color-scheme` 必须跟着主题一起切**，否则 `<select>` 下拉框会黑底黑字/白底白字。
  暂不做"跟随系统"第三态，但 `theme.js` 特意保留"用户从未选择过"这个状态（启动时**不回写**
  localStorage），加第三态时不用改调用方。
- **exe 是 GUI 子系统的，没有控制台窗口。** Tauri 模板默认写成
  `cfg_attr(not(debug_assertions), windows_subsystem = "windows")`，于是每个 debug exe 都会带一个
  cmd 窗口。用户要的是"双击打开没有 cmd 窗口"，同时不想丢掉开发期的输出，所以：
  默认无控制台；`--console` 用 kernel32 的 `AttachConsole` 挂回父终端（不加 `windows-sys` 依赖）；
  **panic 写进 `startup-error.log`** 以免出现"窗口空白且毫无痕迹"。`check.ps1` 直接读 PE 头的
  Subsystem 字段防止被改回去。
- **`srcdoc` iframe 会继承父文档的 CSP，两条策略取交集。** 这是本项目目前最容易再犯的错误：
  正文的远程图片解锁**必须**靠外壳放宽 `img-src`（`index.html` 的 meta 和 `tauri.conf.json`
  的 `security.csp` 两处都要），因为里层策略再宽也过不了外层。默认拦截由里层策略保证，
  `check.ps1` 有一项专门钉住这个不变量 —— 别为了"修 bug"去放宽里层的默认值。
- **正文的宽度规则：只中和像素宽度，必须放过百分比宽度。** 真实邮件常是固定 720px 的桌面布局，
  塞进 ~570px 的读信栏会溢出被裁。中和内联 `style="width:720px"` 只能靠 `!important`，
  但一刀切 `width: auto !important` 会把 `<table width="100%">` 压成读信栏的 6%，毁掉正常邮件。
  所以规则里显式排除 `[width$="%"]` 和 `[style*="%"]`。`check.ps1` 钉住了这个排除。
  **也不要给图片加 `max-height`**：等比缩放会把 800×3765 的竖版横幅变成 89px 宽的细缝。
  改这些规则前先读 `src/app.js` 里 `buildSandboxedDocument()` 上方那段注释，里面有实测数据。
- **文件夹名是人读的，线路名不是。** 服务器发的是 `INBOX`/`Sent Messages`/`Deleted Messages`
  这类标识符，`folders.rs` 把它们变成本地化标签并给出侧栏顺序。两个信号：RFC 6154 属性优先
  （语言无关，德语的 `Gesendet` 也能认），英文名回退兜底（**QQ 一个属性都不发**，回退不是可选项）。
  排序**必须用回退后的有效角色**，只用属性会让不发属性的服务器把「收件箱」排到最后 ——
  loopback 测试抓到了这个 bug。`MailboxInfo::name` 永远保持线路形态，命令只认它。
- **同步的写法是被 `!Sync` 逼出来的，别"简化"回一个 `async fn`。** `sync.rs` 里是
  `SyncDriver` + 两个同步阶段 + 一个异步取件；驱动只持有自有数据（所以 `Send`），
  缓存在异步的两侧各开一次短连接。曾经写成 `async fn sync_folder(&mut client, &cache, ...)`
  在测试里能跑通，但**在 Tauri 里编译不过**（future 非 `Send`）。
  `src-tauri` 的 `refresh_folder` 和 loopback 的 `sync_once` 刻意保持同一种驱动形状 ——
  这两处不一致就等于测试覆盖的不是实际发货的路径。
- **`UID FETCH <n>:*` 在 n 超过最大 UID 时会返回最后一封，不是空集。** RFC 3501 规定区间无序，
  所以 `5:*` 等价于 `4:5`。信任服务器返回范围的同步会不断重复插入旧邮件，所以客户端必须按
  `uid > after_uid` 过滤。loopback 的假服务器**刻意忠实于这个行为**（`parse_uid_set` 里把区间排序），
  否则那条"第二次同步 fetched == 0"的断言测不到任何东西。
- **本地搜索的分词方案不要再改。** FTS5 默认的 `unicode61` 搜不到中文子串（连续汉字是一个 token），
  而所有资料推荐的 `trigram` **在这里也是错的**：它要求 ≥3 字符，中文词大量是 2 个字。
  可行方案是索引侧与查询侧都逐字符空格化（`to_tokens` / `to_query`）。三条路都实测过，见 README。
  **两边必须用同一套变换**，由单元测试钉住；`check.ps1` 另有一项拦 trigram 回归。
- **连接池的两条约束，改之前先读 `pool.rs` 的文档注释。** 一是复用前必须验活（`NOOP`），
  但**验活不够** —— 服务器刚关闭时写入仍会成功、`noop()` 返回 `Ok`，
  所以命令失败时要丢弃该连接并重试一次（`pool_op!` 宏）。二是锁不能跨 `.await` 持有，
  原因和 `Cache` 一样（`ImapClient` 是 `Send` 但非 `Sync`）。
- **推送的实现形状是"窗口 + 重新比对"，不是"等推送消息"。** 原因见上面的库限制。
  当前只监听 INBOX；要扩展成多文件夹就是多开几组 pump/handler。
  托盘与通知都在 `src-tauri`：托盘在 `setup` 里建，通知从 Rust 侧发（前端没有打包器）。
- **账号身份不再只信表单：`openid email` + 解 `id_token` 是载荷性的。**
  scope 少了 `openid email`，令牌响应里就没有 `id_token`，`oauth_sign_in` 那句比对会静默变成
  "服务商没说"（日志里有一行明说）。**别把它当成"多要了一个权限"而删掉** ——
  它换来的正是"表单里填的地址 == 浏览器里授权的账号"这个判断，而缺了它就会出现
  一个**永远认证失败**的账号（M1a-82 就是这么来的）。`TokenSet.id_token` 带
  `skip_serializing`：**不要**把它送到渲染层。
- **对话视图只有**一个**正文渲染面（`#body-surface`），它在两个视图之间被"搬动"，不是各渲染一份。**
  两个理由，缺一不可：① 沙箱 iframe 是"显示图片"那次解锁的边界，两份就意味着解锁能从一封泄漏到另一封；
  ② 20 封的会话不该有 20 份渲染器。`renderThreadReader()` 在清空气泡列表**之前**必须把 surface 挪回
  `#reader-body`——它正住在展开气泡里，`innerHTML = ''` 会把它（和活着的 iframe）一起扔掉。
  `check.ps1` 第 4 步现在钉住"`index.html` 里恰好一个 `#body-surface` / `#body-frame`"。
  **这条约束的直接后果：同时最多只能展开一封**（展开第二封会把第一封收起）。这不是偷懒，
  它是上面两个理由的算术结果；要真正多封同时展开，就得先解决"多个渲染器"或"第二个渲染路径"。
- **阅读模式的默认是普通模式，按钮文案是「✉ 普通模式 / 💬 对话模式」。**（用户 2026-10-06 要求：
  "顶上对话按钮改成对话模式，相应的默认改成普通模式"。）偏好仍在 `localStorage` 的 `ee.readerMode`，
  且**没选过就不写盘**，所以以后改默认值不会覆盖用户的真实选择。
- **对话模式里所有气泡默认折叠**（同上，用户 2026-10-06），点一下展开、点它的头部再收起。
  ⚠️ **气泡头和气泡本身都监听点击，所以头的处理函数必须 `stopPropagation`** ——
  否则一次点击 toggle 两次（先展开、再收起），现象是"点了没反应"，探针抓到的就是这个。
  展开气泡的**正文里**点击不会收起（那是一份用户正在读、正在选字的活文档）。
- **`.bubble` 必须 `flex: none`。** 在可滚动的 flex 列里 `flex-shrink` 默认为 1，而**自动最小尺寸只在
  `overflow: visible` 时生效** —— 只有展开气泡带 `overflow: hidden`（圆角裁切），于是只有它被压成 1px，
  旁边的预览气泡因为有自动最小尺寸而毫发无损。这个失败模式**测量脚本会正常报数**、只有看截图才像坏了。
- **正文的取件是"展开那封 + 有界预取 ≤8、顺序、就地更新"。** 会话里点开一封要立刻有正文，
  其余的慢慢填；每到一个就只改那一个气泡的预览（整体重渲染会搬动 surface 并重载用户正在读的 iframe）。
  就地更新**必须按 class 找预览**（`.bubble-preview` / `.bubble-note`），不能再用 `> div:last-child`：
  用户后来展开的那个气泡，最后一个子元素是住着活 surface 的 `.bubble-body`，覆盖它等于毁掉正在读的邮件。
  预取的"用户走开了"判据也改成了**成员列表本身**（`state.threadMembers !== members`）而不是展开项，
  否则一展开就放弃其余预览的填充。
  没有精确 `thread_key` 的旧行走前端近似分组（只看得见当前文件夹），**不**在 SQL 里猜 ——
  猜错会把两个不相干的会话永久合并。
- **「加载更多」必须先给本地缓存，再问服务器 —— 顺序就是这条功能的正确性。**
  列表一页 `PAGE_SIZE`（50），而首次同步存的是整个窗口（500），每次回填也往缓存里写，
  所以缓存里几乎总是比列表多。**旧实现直接问服务器**（`load_more_messages`），
  于是真机上的现象是：QQ INBOX 缓存里 192 封（最早 9/2），列表停在 10/2，
  点「加载更多」立刻显示"没有更早的邮件了"—— 142 封已经下载好的邮件**永远看不到**。
  规则抽到 `src/history.js`（`historyStep`），`tools/history_test.mjs` + 门禁第 10 步钉住三条：
  **缓存优先 → 缓存穷了问服务器 → 两边都穷了才算到底**。
  注意两个分支在界面上都表现为"行变多了"，所以看界面是看不出顺序错的。
- **对话模式对每一封都适用**（用户 2026-10-06 要求）：一封的会话也是会话，照样气泡显示。
  `loadConversation` 现在接受 `>= 1` 个成员（原来要求 `> 1`），`renderReader` 的判据是
  `threadMembers.length >= 1`。搜索命中与本地列表（草稿/发件箱）仍然不进对话模式 ——
  搜索命中跨多个文件夹，"这个会话"不是近似分组能看见的东西。
- **同一对方 + 同一主题、但彼此之间没有 `In-Reply-To`/`References` 的邮件不合并，这是有意的。**
  真机例子（2026-10-06）：`wangfang@example.com` 在一个月内发了 4 封主题都是「王芳 您好」的信，
  每封 `references=[]`、`in_reply_to=None` —— 它们真是 4 封独立邮件，`thread_key` 各不相同（各自是根）。
  只按主题合并会把同主题但不相干的信**永久**并到一起，所以宁可分成 4 行。
  诊断入口：`python tools/inspect-cache-detail.py <地址>`（打印每封的 `thread_key` 与线程头）。
- **会话分组是两条信号的**并集**：`References` 链（事实）+ `subject_key`（猜测，同一对方+同一主题）。
  用户 2026-10-06 选的。要点，缺一条就会做错：**
  ① **只对用户收到的邮件**算 `subject_key`（发件人是本账号的不参与）——发出去的新邮件收件人有多个，
  只按"主题+自己"合并会把互不相干的信并到一起；
  ② **必须限时间窗**（`THREAD_SESSION_GAP_SECS` = 7 天，相邻两封的间隔）：没有窗的实测结果是
  Google「安全提醒」24 封跨 4.7 年并成一行；加窗后 24 → 15 段，而用户报告的 4 封（跨 1 天）仍是 1 段；
  ③ **两侧同一规则**：Rust（`Cache::thread_summaries` 的闭包：链 → 会话 → 再链）与
  JS（`threads.js::groupConversations` 的 union-find，列表用它折叠）。不一致的症状是"行数 ≠ 气泡数"；
  ④ **先合并完再分组**：union-find 边合并边建组会把前面已经建好的组留在旧根上，一个会话被拆成两个
  （测试抓到过，别改回去）；
  ⑤ 迁移 v5 只用行内数据回填 `subject_key`（不联网），`messages` 一行不丢。
  诊断入口：`python tools/inspect-cache-detail.py <地址>`。
- **会话底部的回复框不走 `openComposer`**：它的意义就是"回一句话不用开对话框"。共用的部分一个不少 ——
  负载构造（`payload.outgoingMail`）、线程头（`src/reply.js` 的 `replyHeaders`）、发件箱（撤销窗口与
  "崩溃后仍会送达"都来自它）。"回给谁"和"主题前缀"两条规则在 `src/reply.js` 里，
  `tools/reply_test.mjs` + `check.ps1` 钉住：**回最新一封**在"最后说话的是自己"时会把信发给自己
  （所以要先跳过自己的邮件），而**丢了 `References` 链**的回复在对方那里是一个全新会话。
- **`cache.db` 的索引必须建在 `add_column_if_missing` 之后。** v2 老库到达迁移时 `messages` 表根本没有
  `thread_key` 列，把索引写进同一个 schema 批次会让 `Cache::open` 直接失败（`no such column`），
  连带两条迁移测试一起红 —— 已证伪过，别"整理"成一批。
- **Microsoft 的回调主机名固定 `localhost`，监听端**两个族都绑**（`127.0.0.1` + `::1`）。**
  用户 2026-10-04 实测 **Azure 拒绝 `http://127.0.0.1`**，所以回调主机名只能是 `localhost`；
  而 `localhost` 在 Windows 上可能先解析到 `::1`，只绑 IPv4 时"浏览器会不会回退"不是我们能保证的事，
  它失败的样子正好是"点了没反应"。**不要**为了"简化"退回单个 listener，也**不要**改成 `0.0.0.0`/`::`
  —— 通配绑定会让同网段的机器把授权码送到我们手上（A1-F36）。`SO_EXCLUSIVEADDRUSE` 两个族都设。
- **自定义服务器只做"手动"这一层，自动发现不做。** 域名 autoconfig / ISPDB / DNS SRV / Autodiscover
  都要向第三方发出**用户的域名**（`design/05` §11 反驳 5），而且需要网络层的额外工作；手动层就能覆盖
  自建邮局与表外服务商。要加的时候，入口是 `providers.rs` 里那一行与 `EndpointOverride`。
- **端点规则（清洗、端口、加密）的唯一出处是 Rust**（`normalise_host` / `validate_endpoint` /
  `endpoint_advisory`），前端**刻意不抄一份**：`save_account` 在本地、往返是毫秒级，
  抄一份就是第二个会写错的地方。加密选项列表也由 `endpoint_choices` 命令下发（线格式是 serde 的拼写），
  渲染层不硬编码 `ssl`/`start_tls`/`plain`。
- **校验必须在 `save_account` 里最先做**（写凭据与写 `accounts.json` 之前）。这不是风格：
  半创建的账号要靠"关掉对话框"来清理，而一个改了字段再重试的用户**本来就不该创建过它**。

## 6. 已经验到什么程度

**已验证**：**365 passed（WSL）/ 366 passed（Windows）+ 7 ignored**（第 70 轮两平台实测，构成带口径
见 §2）；`cargo build` 零警告；40 个真实服务器端点（TCP+TLS+协议问候语）。

**Gmail（2026-10-04）：认证轴第一次在真实 Google 端点上跑通，而且收信也通了。**
`realuser@gmail.com` —— OAuth 授权 → 刷新令牌 DPAPI 落盘 → **INBOX 实收 289 封**
（`folders.last_uid=289`、`synced` 是本次会话内，全部带 `thread_key`），`[Gmail]/` 八个文件夹也枚举出来了。
这一条路上抓到并修掉了**四个**真 bug（按钮没绑事件、`cmd` 截断授权 URL、reqwest 丢 `system-proxy`、
地址与授权账号不一致），叙述在 PROGRESS 的 M1a-79…M1a-83。
**未验**：Gmail 的**发信**（SMTP:465 基本不通，见上面的网络事实）；Outlook 的 OAuth 路径（代码就绪、
`setup_note` 已改成指向它，但**从没在真实 Outlook 账号上走过**）。

**并且已经用真实 QQ 邮箱账号跑通了完整收信闭环**（2026-10-02）：
登录 → 文件夹列表 → 邮件列表 → 显示真实邮件。过程中发现并修掉了三个真实问题，见 README。
所以"服务商是否真的接受我们这套"对 **QQ 系**已经有答案了。

**对话模式也验过了**（2026-10-04 首次；**2026-10-06 按新交互重验**）：真实 35 封会话聚成气泡，
**首屏 0 封展开**（全部折叠）、点一下展开（`▾` + `aria-expanded=true` + surface 搬进那一封）、
点它的头部再收起、**同时最多 1 封展开**（展开第二封时第一封收起，`Sent Messages` 那封也照样能展开 ⇒ 跨文件夹），
展开气泡正文里点击不会误收起；底部回复框显示"回复给 correspondent@example.com"（最新一封**不是自己**的那封），
空内容点发送只得到"先写点内容再发送。"且**发件箱没有多出任何东西**；切回普通模式后 surface 回到读信栏。
全场始终 **1 个 `#body-surface` / 1 个 `#body-frame`**。**没验到的**：真的点「发送」——
那会给真人发信，所以探针刻意不点（入队那条路由 `probe-outbox.mjs` 与 Rust loopback 覆盖）。
`cache.db` v3 → v4 的迁移**在真实库上实做过**：467 封邮件一封没丢、索引出现。

**真实邮件渲染也已经验过了**（2026-10-02，同一账号，CDP 探针 + 截图）：
扫了 INBOX 前 20 封 —— 20/20 都能解析出 HTML 与纯文本正文、0 错误；3 封的附件条正确显示
名字与大小；18/20 含远程图片，默认全部拦截、点"显示图片"后 **14/14 全部加载成功**
（`naturalWidth > 0`），并且解锁不会泄漏到下一封。过程中发现并修掉了两个 bug（见 README 第 8、9 条）。
注意：**这 20 封里没有任何一封使用内嵌（`cid:`）图片**，所以
`resolveInlineImages()` 至今**仍未被真实数据检验过** —— 它只有 loopback 假邮件的覆盖。

**界面语言与正文排版也验过了**（2026-10-02，同一账号）：
文件夹中文标签与顺序在真实 QQ 账号上核对过（收件箱/草稿箱/已发送/垃圾邮件/已删除/其他文件夹）；
正文宽度规则对着真实固定宽度邮件测过 —— 内容 752px → 554px（= 视口宽），越界元素 142 → 0，
横幅不再被裁；同时用合成夹具确认百分比布局未被破坏。

**本地缓存与搜索也验过了**（2026-10-02，同一账号，CDP 驱动真实界面）：
首次同步 149 封、二次同步 **0 封**（应用日志里的证据，同时证明游标与 UIDVALIDITY 判断都对）；
搜索框输入中文 2 字词命中 3 条、ASCII 子串 `openco` 命中、发件人地址 `cgbchina` 命中；
点击结果能打开且停留在搜索态；无匹配时界面明确说明覆盖范围；清除后恢复文件夹列表。
`cache.db` 里没有任何密码或授权码。

**连接池与推送也验过了**（2026-10-02，同一账号）：
应用启动日志出现 `已开始监听 me@example.com 的 INBOX`，说明 IDLE 在真实 QQ 服务器上
连上并选定了文件夹；随后**用应用自己的 SMTP 给自己发了一封邮件**（临时测试，已删除），
20 秒后日志出现 `同步 INBOX：新增 1 封，缓存共 150 封` 与
`推送：… 新增 1 封（未读 53）`，紧接着前端刷新（缓存命中、0 新增）。
**没验到的**：Windows 通知气泡是否真的弹出来（`.show()` 返回 Ok、日志无报错，但气泡本身要眼睛看），
以及托盘图标是否被 Windows 折叠进溢出区。

**附件下载也验过了**（2026-10-03，同一账号，临时测试已删除）：
真实 QQ 邮件上一份 `Invoice-XKOGBCCQ-0001.pdf`，`declared=23477 written=23477 on_disk=23477`，
文件头 `%PDF-`。
**没验到的**：前端点附件标签那条路径（后端路径已通；改的是把 span 换成 button + 一次 invoke，
契约由 `check.ps1` 新加的那项守着）。

**附件上传也验过了**（2026-10-03，同一账号，临时测试已删除）：
40KB 非 7bit 内容经真实 SMTP 发给自己，6 秒后到达（uid 5814），经真实 IMAP 取回后
`back=40000 written=40000 identical=true` —— 逐字节一致。
**没验到的**：前端「添加附件」按钮那条路径（文件选择器需要人点，无法自动化）。

> 测试期间给自己的邮箱发了两封自测邮件（标题以「Easy Email 推送自测」「附件往返自测」开头），
> 可以直接删掉。

**还没验证的**，别以为已经验过：

1. **除 QQ 系与 Gmail 以外的服务商（17 家），从没用真实账号登录过。** 尤其是 **163/126 的 `ID` 命令格式**
   —— `needs_id_command` 目前只有 loopback 假服务器验过，没对着真网易服务器验过。
   服务商表里的 `verified: true` 指的是**服务器地址**实测过（40 个端点的 TCP+TLS+问候语），
   **不是**"登录实测过"；这两个差别很大，别把前者当成后者。
2. **Outlook / Microsoft 365 的 OAuth 路径没走过**：代码与三个缺口都补齐了（M365 关 SMTP AUTH 的专门文案、
   `smtp_auth_rejected`、`localhost` 双绑定回环、Azure 步骤写进 `setup_note`），Microsoft 令牌端点也实测可达
   （`--ignored` 测试 3.7 秒拿到服务商答复），**2026-10-06 又按官方文档逐条核对了整条契约并钉进测试**
   （`the_microsoft_flow_matches_the_published_imap_smtp_oauth_contract`：三个资源 scope、`offline_access`、
   `common` + v2 端点、PKCE S256、公共客户端不带 secret、`http://localhost` 与"忽略端口"、
   以及**用微软文档里那个 XOAUTH2 示例的 base64 做逐字节比对**），但**仍然没有真实 Outlook 账号走完过一遍**。
   **卡在哪（别再重新诊断一遍）**：用户 2026-10-06 去注册应用时被 **Azure 门户自己的登录**挡住，报
   `AADSTS16000 … 用户帐户在租户"Microsoft Services"中不存在 … 应用 74658136-14ec-4630-ad9b-26e160ff0fc6`。
   那个 GUID 是 **`ADIbizaUX` = Azure 门户自己**的客户端（Ibiza 是门户的内部代号），报错来自**门户的登录层**，
   **与 Easy Email 无关**：含义是"登录用的账号不是该目录的成员 / 目录选错了 / 会话里是别的账号"。
   修法：用创建该目录的那个账号登录、门户右上角切对目录、或用 InPrivate 重登。**它不需要改我们任何代码。**
   步骤与预期输出见 README 的「真实账号手工验证」。
3. **`tauri build` 打成 NSIS 安装包没验过**，只验了 `cargo build` + 直接运行 exe。
4. **`@foxmail.com` 别名**：QQ 号地址能登录，同一授权码下 foxmail 别名被拒。
   已加了「登录用户名」覆盖字段来应对，但**没有实测确认**哪种填法能通。
   如果以后能拿到一个 foxmail 别名账号，这是第一件该验的事。
5. ~~**没有"自定义服务器"入口**~~ **已做**（M1a-87）：向导里可选「自定义 / 自建服务器」，
   主机/端口/加密方式手填，规则全在 Rust（`normalise_host` / `validate_endpoint`），
   真机探针四件事全过（`tools/probe-custom-server.mjs`）。**但没对着真实自建服务器登录过一次** ——
   `127.0.0.1` 与假地址验证的是"错的输入被正确拒绝"，不是"对的输入能连上"。

## 7. 下一步（用户已选定三件，①②③ 已完成）

### ④ 真实账号验证的三件小事 —— 都在等外部条件
按"成本最低 × 唯一未被真实服务商检验过"排序：

1. **Outlook 真机登录**（M1a-88 把代码与三个缺口补齐了）：需要用户自己在 Azure 注册一个应用，
   步骤与预期输出逐条写在 README 的「真实账号手工验证」。**这是唯一还没被真实服务商检验过的认证分支。**
2. **163/126 的真实登录**（`ID` 命令那条路）：成本极低，只需要一个网易授权码。
3. **自建服务器真机登录**（M1a-87 做了入口）：需要一个真实的 Postfix/Dovecot 之类。

### ⑤ 明确不做 / 留给下一轮
自动发现（autoconfig / ISPDB / SRV / Autodiscover）；"这个服务器不提供 SMTP"的开关
（只收信的自建账号现在还加不进来）；自定义账号的配额与大小上限（表里没有数）；
M365 的**管理员同意**（企业租户可能需要 `prompt=admin_consent` 或租户专属端点）。

### ① SQLite 本地缓存 + FTS5 全文搜索 —— ✅ 已完成（2026-10-02）
`cache.rs` / `sync.rs` / `imap.rs` 的三个取件原语 / Tauri 命令 / 前端搜索框，真实账号验过。
验收标准（断网可读、本地 FTS5 搜索）都达成。

**① 剩下的一小块**：历史回填。缓存只有最新 500 封，更早的没有机制往里加。
要做的话是新增一个"按 UID 区间往前翻页"的取件原语（`UID FETCH <a>:<b>`），
由"加载更多"触发，并且**不要动 `last_uid` 游标**（回填是往旧的方向走，与增量同步正交）。
另外界面还没使用 `sync_status` 命令，所以现在离线时不会显式提示"离线"。

### ② 连接池 + IMAP IDLE 推送 + 系统托盘 —— ✅ 已完成（2026-10-03）
`pool.rs`（复用 + 验活 + 失败重试）、`idle.rs`（有界窗口 + 退避重连）、Tauri 侧的命令改造与
`watch_inbox`/`stop_watching`、托盘菜单、Rust 侧 Windows 通知、前端 `mail:new` 监听。
真机验证见上一节。

**② 可以再补的**：只监听 INBOX（其他文件夹不通知）；~~推送延迟 50 秒受库限制~~（已更正为亚秒级推送）；
托盘图标可能被折叠。这三条都写进了 README 的已知限制。

### ③ 附件下载 / 上传 —— ✅ 都完成了（2026-10-03）

**下载**：`ImapClient::save_attachment` 取回整封邮件、用 `mail-parser` 已经解好码的字节直接落盘；
文件名由 `safe_file_name` 消毒（路径穿越、Windows 保留字符、设备名、超长、重名都处理了）。
真机验证：QQ 上一份发票 PDF 存下，`declared == written == on_disk == 23477`，文件头是 `%PDF-`。

**上传**：`OutgoingAttachment { name, mime, path, size }` 只带**路径**，`smtp::build_message` 在组装时
才读文件；有附件时外层是 `multipart/mixed`（正文作为第一个 part，有 HTML 时是 `alternative`）。
文件选择器走 `tauri-plugin-dialog`（Rust 侧），MIME 按扩展名猜、猜不出退回 `octet-stream`。
真机往返验证：40KB 非 7bit 内容发给自己，IMAP 取回 **逐字节一致**。

**还剩的**：没有大小上限检查（超限让服务器拒）；真正流式的下载做不到，原因见第 5 节。

## 8. 最后

QQ 系已经用真实账号跑通了，所以不用再纠结"地基没验过"。剩下最值得先做的一件小事是
**163/126 的真实登录**（验证 `ID` 命令那条路），成本很低，但它是唯一还没被真实服务商检验过的
协议分支。
