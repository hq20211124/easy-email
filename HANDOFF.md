# HANDOFF

给下一个会话。**只记录从这个仓库里读不出来的东西** —— 架构、模块职责、为什么这样写，
都在代码注释和 `README.md` 里。先读 `README.md`，再读这份。

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
→ `fmt` → `clippy -D warnings` → `cargo build`（抓 linker 警告，clippy 看不到）→ `cargo test`
→ **exe 的 PE 头 Subsystem 必须是 2**（GUI，否则会有 cmd 窗口）。目前 **111 passed**（3 + 83 + 21 + 4）。
加 `-Network` 会额外跑 40 个真实端点（约 40 秒）。

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
- **Gmail SMTP（smtp.gmail.com:465）从大陆连不上，IMAP 通。** 这是网络事实，不是配置错误。
  可达性测试特意把这种情况判成"不可达告警"而不是失败，**别把它改回硬失败**。
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
  - **async-imap 0.12 的 IDLE 不投递 `* n EXISTS`。** `Handle::wait_with_timeout` 把它归到
    `Response::Data { status: Ok, .. }` 那一支并 `continue`，所以 `IdleResponse::NewData` 永远不会
    为"新邮件"触发。**不要试图等 `NewData`**（实测过：夹具推了 `* 5 EXISTS`，10 秒无结果）。
    现在的做法是「有界 IDLE 窗口 + DONE 后重新 `SELECT` 比对 `Mailbox::exists`」，窗口 =
    通知延迟（当前 50 秒）。`ResponseData` 是私有类型，也不要在外面 match 它。
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

## 6. 已经验到什么程度

**已验证**：**104 个测试**；`cargo build` 零警告；40 个真实服务器端点（TCP+TLS+协议问候语）。

**并且已经用真实 QQ 邮箱账号跑通了完整收信闭环**（2026-10-02）：
登录 → 文件夹列表 → 邮件列表 → 显示真实邮件。过程中发现并修掉了三个真实问题，见 README。
所以"服务商是否真的接受我们这套"对 **QQ 系**已经有答案了。

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

1. **除 QQ 系以外的服务商，从没用真实账号登录过。** 尤其是 **163/126 的 `ID` 命令格式** ——
   `needs_id_command` 目前只有 loopback 假服务器验过，没对着真网易服务器验过。
2. **`tauri build` 打成 NSIS 安装包没验过**，只验了 `cargo build` + 直接运行 exe。
3. **`@foxmail.com` 别名**：QQ 号地址能登录，同一授权码下 foxmail 别名被拒。
   已加了「登录用户名」覆盖字段来应对，但**没有实测确认**哪种填法能通。
   如果以后能拿到一个 foxmail 别名账号，这是第一件该验的事。

## 7. 下一步（用户已选定三件，①② 已完成）

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

**② 可以再补的**：只监听 INBOX（其他文件夹不通知）；推送延迟 50 秒受库限制；
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
