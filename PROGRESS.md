# PROGRESS —— M0/M1a 实施进度（自动记录）

> 用户 2026-10-03 夜间指示："开工吧，一直干下去……明早起来看你干完了没"。
> 规则：每个块完成即更新本文件；**未验证的事不写"完成"**；每步都要让 `tools/check.ps1` 全绿。

## 环境与安全网
- 仓库原本**没有 git**，开工前建了基线提交 `83c1b0c`。每一步都可回滚
  （`git log --oneline` / `git diff <sha>`）。
- 设计文档在 `design/`；审计脚本 `python design/tools/audit.py`（当前 0 失败）。
- **测试数量**：基线 116 → 现在 **135**（103 lib + 28 loopback + 4 reachability）。
  `tools/check.ps1` **全绿**：13 步（7 项前端不变量 + **新增"命令共享一个数据库句柄"** + fmt +
  clippy -D warnings + build + test + PE=2）。

## 提交记录
| SHA | 内容 |
|---|---|
| `62d806c` | M0-2b `Cache`/`src-tauri` 切到共享 `Db`（去掉每条命令开连接）+ 新门禁 |
| `9d95c6f` | PROGRESS 记录 |
| `1cc4aef` | M0-2a `db.rs`：单写线程 + 读池 + `DbSet` |
| `66957ad` | M0-7 pragma 定稿 + M0-3 更正推送结论 |
| `c726f98` | M0-5 修增量截断丢信 + M0-1 库能力探针 |
| `83c1b0c` | baseline |

---

## 交接摘要（先读这里）

**当前状态**：`tools/check.ps1` **14 步全绿**；`cargo test --workspace` **156 通过、0 失败**
（122 lib + 30 loopback + 4 reachability）；`python design/tools/audit.py` **0 失败**；
应用启动冒烟通过（7–10 秒存活）。所有改动都在 git 里（基线 `83c1b0c`，共 22 个提交）。

**磁盘上已验证的两件事**（不是文档结论，是真机查过的）：
- `%APPDATA%\com.easyemail.desktop\cache.db`：`auto_vacuum = 2`（老库已后台重建为 incremental）、
  表与 `user_version` 完好 —— "清理缓存真的能腾出磁盘"这条对老用户也成立。
- `%APPDATA%\com.easyemail.desktop\user.db`：`synchronous = 2 (FULL)`、五个表齐全
  —— 与 `cache.db`（NORMAL）各自独立，草稿/待发送不会因为"删缓存"而丢。

**已完成的块**
| 块 | 内容 | 关键证据 |
|---|---|---|
| M0-5 | 修增量截断丢信（保留最旧 + 收窄取件区间） | 回归测试能证伪：还原旧行为即失败（`cursor 604 != 504`） |
| M0-1 | 5 条库能力探针 | IDLE ~0.2 s；APPEND 拿不到 UID；`BODY[]<0.10>` 解析失败；A1-F46 一半被实测推翻；lettre 不发 `SIZE=` |
| M0-2a/2b | 单写线程 + 读池 + `DbSet`；`Cache`/`src-tauri` 全部切到共享句柄 | 门禁：`Cache::open` 只许出现 1 次（当前 13 处共享） |
| M0-7 | 建库 pragma 定稿（`auto_vacuum` 必须是第一条 pragma）+ 老库一次性重建 | 真机 cache.db 由 0 → 2 |
| M0-3 | 更正 README/HANDOFF/注释里被推翻的推送结论（50 s → 亚秒级；窗口 120 s） | 探针引用写在注释里 |
| M0-4 | 分部取件决策：不 vendor fork，改为"拒绝 + 记录"+ 写明三行补丁位置 | 探针断言"拒绝且连接仍可用" |
| M1a-1 | 历史回填：`fetch_older_headers` + `oldest_uid` + `load_more_messages` + 前端按钮 | 门禁：回填不得推进游标 |
| M1a-2 | `user.db` 落地（`UserStore`） | 真机 synchronous=2 |
| M1a-3/3b | 可配同步窗口：模型 + 持久化 + `backfill_older` 窗口语义 + 命令 | `Count(20)` 停在正好 20 封；`Years(2)` 一页跨边界即停 |
| M1a-4 | 草稿落库（upsert / 列表 / 删除 / 重启存活）+ 三个命令 | 5 条测试，含"重启后草稿还在" |

**被外部条件卡住的两项（不是没做，是做不了；步骤已写清）**
1. **M0-6 WinAppSDK 通知**：本机没装 Windows App SDK 运行时，`src-tauri` 也没有 `windows` crate 依赖。
   恢复：`winget install Microsoft.WindowsAppRuntime.1.7` → 加 `windows` crate 的 AppNotifications
   features → Register + 双按钮 + `NotificationInvoked` → 量框架依赖/自包含的体积。
2. **M0-8 OAuth 端点探测**：需要你的 client_id 与网络。命令已写在本文档 M0-8 一节，
   结论只有两种：能不带 `client_secret` 换到令牌（PKCE 够）或不能（BYO 向导要加一个输入框）。

**下一步优先级**（每一件都自包含，做完各让 `check.ps1` 与测试保持全绿）
1. **写信接草稿**：`src/app.js` 的 composer 目前把状态放在闭包里、不落库；
   接 `save_draft` / `list_drafts` / `delete_draft` 三个命令（自动保存 + 关闭时保存 + 草稿箱列表）。
2. **发件箱**：`outbox` 表已就位；`design/07` §5.3 的 worker（撤销窗口、退避重试、`needs_attention`）
   与"发送后归档到已发送"（`append` + `find_uid_by_message_id` 已实现）。
3. **窗口选择器界面** + 冷启动自动回填一页（并接配额账本，`design/04` §5.7）。
4. 认证轴（`AuthMethod`：应用专用密码为主 + BYO，P2 已拍板）→ 会话串 → 通知托盘 → 安装包与自动更新。

**运行方式**
```
.	ools\check.ps1                     # 14 步门禁（前端不变量 / fmt / clippy / build / test / PE=2）
cargo test --workspace                # 156 个测试
python design	oolsudit.py          # 设计文档一致性（对应 design/ 的 23 份文档）
git log --oneline                     # 22 个提交；git diff 83c1b0c HEAD 是本次全部改动
```

---

## 已完成

### M0-5 修"增量截断丢信"bug ✅
`fetch_headers_since` 一次到达多于一个批次（500）时**保留最新**、而游标按最大已存 UID 推进
⇒ 中间的 UID 永久落在游标之下、静默丢信。改成：取件区间收窄为 `<n>:<n+limit-1>`，并**保留最旧**一批。
**证伪验证**：临时还原旧行为 ⇒ 新回归测试失败（`cursor 604 != 504`）；恢复后通过。
回归测试：4 封冷同步 → 一次到 **600 封** → 断言缓存 UID 必须**连续 1..=504**、第二批补齐 604。

### M0-1 库能力探针 ✅（5 条）
IDLE `NewData` ~0.2 s（推翻"50 秒延迟"）· APPEND 回 `APPENDUID` 但客户端拿不到（靠
`find_uid_by_message_id` 取回 4242）· `BODY[]<5.10>` 让 imap-proto 解析失败（附件确实无法真流式）·
**References 的 A1-F46 一半是错的**（`as_text_list()` 对单 id 返回 `Some(1)`；真正的坑是 `as_text()`
只返回最后一个）· lettre 不发 `SIZE=`（预检只能靠本地表）。
顺带实现 `ImapClient::append` 与 `find_uid_by_message_id`（07 §5.4 的原语）。

### M0-7 建库 pragma 定稿 ✅（并在真实数据上验证）
- 新库：`auto_vacuum = incremental` 必须是**第一条 pragma**，排在 `journal_mode = wal` **之前**
  —— 切 WAL 会写数据库头，之后 SQLite 静默忽略（实测两种顺序：先 journal 读回 0，先 auto_vacuum 读回 2）。
  设计 03/15 已按此收紧。
- **老库（v0.2 建的）**：`auto_vacuum` 无法就地开启 ⇒ 新增 `needs_auto_vacuum_rebuild()`，
  启动时检测到就**后台跑一次全量 VACUUM**（`std::thread`，不阻塞开窗）。
- **端到端验证**：本机真实的 `%APPDATA%\com.easyemail.desktop\cache.db`（212 KB、v0.2 建的、
  原本 `auto_vacuum=0`）在启动后被改为 **2**，`user_version=1` 与全部表完好
  ⇒ "清理缓存真的能腾出磁盘"这条从今天起在**老用户身上也成立**。

### M0-3 更正被推翻的结论 ✅
HANDOFF/README 里"async-imap 不投递 EXISTS / 推送最多 50 秒"全部改掉；`idle.rs` 窗口 50 s → **120 s**
（它是心跳，不是延迟）。

### M0-2a `db.rs`：数据库句柄 ✅
`Db` = 一个写线程（`std::thread` + `mpsc`，回值 `oneshot`/std 通道）+ 读池（默认 3 条）。
`write`/`write_blocking` 整批一个事务（`unchecked_transaction`）；`exec_blocking` 给 VACUUM 这类
不能进事务的操作；`read_guard()` 把一条池连接借给同步读（**借用而非闭包** ⇒ 不改所有权、不需要
`Send + 'static`）；`MailError::Busy` 统一分类锁竞争。**`Db` 内不得 `tokio::spawn`**
（`setup()` 没有 Tokio 上下文 —— HANDOFF 记的"启动即消失"）。

### M0-2b `Cache` / `src-tauri` 切到共享句柄 ✅
- `cache.rs`：字段 `conn: Connection` → `db: Db`（`Clone`）。读方法 2 行改动（`read_guard`），
  写方法走 `write_blocking`（闭包 + 拥有所有权的参数）。`refresh_search_row` 改成**收 `conn` 的
  关联函数**，在调用方事务里跑（否则一个批次会退化成每行一个事务）。
- `db.rs` 的 in-memory 走 **direct 模式**（一条连接 + 互斥锁、无线程）：内存库没有文件头，
  `auto_vacuum` 永远读回 0，所以相关测试改用真文件（这本身也是一条被实测纠正的假设）。
- `src-tauri`：`AppState` 持有 `cache: Cache`，`setup` 里**只开一次**（失败退回内存缓存），
  16 处 `Cache::open` 全改成 `state.cache.clone()`；`refresh_folder`/`announce` 改收 `&Cache`。
- **新门禁**：`check.ps1` 新增一步"命令共享一个数据库句柄"——`Cache::open` 只允许出现 1 次
  （启动），且必须有多处共享；当前输出：`cache opened once at startup, shared by 11 call sites`。
- **启动冒烟**：`target\debug\easy-email.exe` 启动后 8~10 秒仍存活（没有"启动即消失"）✓


### M0-4 imap-proto 分部取件：**已决策**（不 vendor，改为拒绝 + 记录）✅
- 实测收紧（比研究结论更严重）：`imap-proto` 0.17 的 `msg_att_body_section` 只认
  `BODY [section] <number> " " nstring`，**连 `BODY[]<0.10>` 都解析失败** ⇒ 服务器回答分部取件时
  通常带 `.offset`，所以**补丁前分部取件完全不可用**（原研究只说"不接受 offset"，不够准）。
- 决定：**M1 不 vendor fork**（fork 的维护成本应记在流式附件这一功能上，而它是 M2）。改为把
  `fetch_message_range` 变成**直接返回可行动错误**，不再尝试——因为尝试会让解析失败并**关闭连接**，
  等于拿一条连接池会话去换一次必然失败。
- 书面记录：探针 `probe_partial_fetch_is_refused_instead_of_killing_the_connection` 断言
  "拒绝 + 连接仍可用"；补丁位置写在 `design/19` M0-4（`imap-proto-0.17.0/src/rfc3501/body.rs`
  的 `msg_att_body_section`，约 3 行：把 `<` 后的 `number` 改成 `number ('.' number)?`）。

### M0-6 WinAppSDK 通知 spike：**离线阻塞**（已量清前置条件）⚠️
实测本机情况（2026-10-03 夜）：
- `src-tauri/Cargo.toml` **没有 `windows` crate 依赖**（只有 `tauri-plugin-notification`），
  要调 `Microsoft.Windows.AppNotifications` 得先加该 crate 与对应 feature。
- 本机**未安装 Windows App SDK 运行时**（`%ProgramFiles%\WindowsApps` 与
  `%LOCALAPPDATA%\Microsoft\WindowsApps` 下都没有 `WindowsAppRuntime*`）。
⇒ 这套 spike 需要两样当前环境不具备的东西（新依赖 + 运行时），**按纪律如实记为阻塞**，不假装做完。
**恢复时照着做**（两条分支已在 `design/19` M0-6 写清）：
1. `winget install Microsoft.WindowsAppRuntime.1.7`（或对应版本）装运行时；
2. `src-tauri` 加 `windows = { version = "0.6x", features = ["Microsoft.Windows.AppNotifications",
   "Microsoft.Windows.AppNotifications.Builder", "Win32_UI_Shell", "Win32_UI_WindowsAndMessaging"] }`；
3. 最小 spike：`AppNotificationManager::Default().Register()` → `AppNotificationBuilder` 加两个按钮
   → 发一条 → 点按钮断言 `NotificationInvoked` 回调里的参数；
4. 量体积：框架依赖 vs 自包含（后者会影响 17 §5 的 ≤20 MB 预算）。

### M0-8 OAuth 端点探测：**需要你的凭据**（命令已写好）⚠️
要回答的问题只有一个：**Google 的令牌端点是否接受"只有 `client_id` + PKCE、不带 `client_secret`"**
（决定 BYO-Gmail 的向导要不要多一个输入框，见 `design/05` §3.1.0）。有网+自己的 client_id 时跑：
```
# 1) 授权码：浏览器打开（把 <CLIENT_ID> 换成你自己的，redirect_uri 是桌面客户端的回环地址）
https://accounts.google.com/o/oauth2/v2/auth?client_id=<CLIENT_ID>&response_type=code
  &scope=https%3A%2F%2Fmail.google.com%2F&redirect_uri=http%3A%2F%2F127.0.0.1%3A<PORT>
  &code_challenge=<S256>&code_challenge_method=S256&access_type=offline&prompt=consent
# 2) 换令牌：故意不带 client_secret
curl -s -X POST https://oauth2.googleapis.com/token \
  -d client_id=<CLIENT_ID> -d code=<CODE> -d code_verifier=<VERIFIER> \
  -d grant_type=authorization_code -d redirect_uri=http://127.0.0.1:<PORT>
```
- 返回 `access_token` + `refresh_token` ⇒ **PKCE 足够**，向导只问 client_id（少一步）。
- 返回 `invalid_client` / `client_secret is missing` ⇒ **必须带 secret**，向导要加一个输入框
  （存进 DPAPI vault，与令牌同级保护）。
把结论写进 `design/A1`（带日期）与 `design/05` §3.1.0，然后 M0-8 关闭。

---

### M1a-1a 历史回填的后端原语 ✅（2026-10-03 夜，第 5 轮）
`imap::fetch_older_headers(mailbox, before_uid, limit)` + `cache::oldest_uid()` + `keep_newest_page()`。
**两条不变量**都写进了端到端测试：
1. **往前看要拿"离阅读位置最近"的那一批**（不是最老的）——两步走：`UID SEARCH 1:<before-1>` 拿 UID 编号，
   再 `UID FETCH <编号集合>` 只取这一页（直接 `FETCH 1:<before>` 会让服务器把整个旧邮箱推过来）。
2. **历史页绝不动增量游标**：`last_uid` 始终指向最新已存邮件，所以回填永远不可能让增量同步跳过邮件。
   测试断言 `last_uid` 在回填后仍是 104（`probe`/`gate` 层面也会钉住，见下）。
顺带修了夹具的一处不真实：`UID SEARCH` 以前无视范围、永远返回整箱 UID，**恰好会掩盖**
"调用方信任范围"的 bug —— 现在 `ALL` 与 `<n>:<m>` 都按规格回答。

**未完（下一步，照 `PLAN.md` §6/§7/§8 做）**：
- `src-tauri` 新命令 `load_more_messages`（读 `oldest_uid` → `fetch_older_headers` → `upsert_summaries`
  → 返回这一页 + `history_done`），并把 `list_messages` 的 `MAX_LIST_ROWS` 上限换成"分页"语义。
- `src/app.js`：`btn-load-more` 现在只是把 `state.limit` 加大后重新列缓存（超过窗口就没东西了）；
  改成调用新命令并把结果**前置**到列表，`history_done` 时禁用按钮并显示"没有更早的邮件了"。
- `tools/check.ps1`：加一条门禁 —— 源码里不得出现"回填路径调用 `set_cursor`"（游标只能由增量同步推进）。

### M1a-1b 历史回填接通（命令 + 前端 + 门禁）✅（第 6 轮）
- **命令**：`load_more_messages(accountId, mailbox, limit)` —— 读 `oldest_uid` → `fetch_older_headers`
  → `upsert_summaries` → 返回这一页与 `done`。**只读 `oldest_uid`，不碰增量游标**。
- **前端**：`btn-load-more` 以前是"把 `limit` 加大后重新列缓存"（超过窗口就没东西了），现在调用新命令
  并把结果**前置**到列表；新增 `state.historyDone` —— 到底时按钮变「没有更早的邮件了」并禁用，
  失败时恢复可重试，切文件夹/刷新时重置。
- **新门禁**：`check.ps1` 检查 `load_more_messages` 的函数体里**没有** `set_cursor`、没有 `last_uid` 赋值，
  且**必须**用 `fetch_older_headers` —— 把"回填不得推进游标"这条不变量钉成机器检查（不靠人记）。
- **接线证明**：门禁"每个前端命令都已注册"数出 **15** 个命令（原 14），全部已定义并注册。
- 启动冒烟：7 秒存活 ✓；`check.ps1` 全绿；测试 **136**。

**M1a 第一步（窗口同步 + 历史回填）到此完整可用**：后端往前翻页 → 命令 → 按钮 → 门禁。
剩下的 M1a 部分：可配同步窗口（`design/04` 的 `SyncWindow`，让"最近 2 年/全部"真的可选）、
`user.db` 落地（草稿/发件箱/规则/联系人）、认证轴、会话串、通知托盘、安装包。

### M1a-2 `user.db` 落地 ✅（第 7 轮）
`design/03` §3.1 的 D4 决定（**双库分家**）现在在磁盘上成立了，而不只是文档里：
- 新模块 `crates/mail-core/src/userdb.rs`（`UserStore`）：表 `schema_migrations` / `kv` / `drafts` /
  `outbox` / `pending_ops`；迁移全程一个事务（版本号必须描述一套完整 schema）。
- `outbox` 的 `undo_until` / `scheduled_at` / `next_attempt_at` 都是 **NOT NULL DEFAULT 0** ——
  第二轮评审实测过 `null <= now` 恒为 NULL，可空列会让待发送邮件永远取不到。
- `src-tauri`：启动时同时打开两个句柄（各自失败都不拦启动，退回内存实现），`AppState.user`；
  `sync_status` 增加 `drafts` / `pending_ops` 计数。
- **真机验证**：`%APPDATA%\com.easyemail.desktop\user.db` 已生成 —— `user_version=1`、
  **`synchronous=2 (FULL)`**、五个表齐全；而 `cache.db` 仍是 `NORMAL`（1）。
  这正是"草稿/待发送不能因掉电丢、缓存可以重建"这条设计在磁盘上的体现。
- 测试 **140**（新增 4 条）；`check.ps1` 全绿；启动冒烟 8 秒存活。

**M1a 剩下的部分**（下一轮起）：
1. 可配同步窗口（`SyncWindow`：全部 / 最近 N 年 / 最近 N 封），落进 `user.db` 的 `kv`（键 `sync_window`）。
2. 草稿与发件箱：`drafts` / `outbox` 表已就位，接写信与发送流程（含撤销窗口、定时、重试）。
3. 认证轴（`AuthMethod` + 授权码/应用专用密码文案，P2 已拍板）、会话串、通知托盘、安装包与自动更新。

### M1a-3 可配同步窗口（`SyncWindow`）：模型 + 契约 + 持久化 + 命令 ✅（第 8 轮）
- **模型**：`SyncWindow{All, Years(u16), Count(u32), OnDemand}`，默认 `Years(2)`（设计 P9）。
  `since_timestamp(now)` / `max_messages()` / `is_outside(date, now)` —— 其中
  **日期未知的邮件一律保留**（把"未知"当成"太旧"会藏起真邮件，而代价只是多一行头部）。
- **存储形式**：`as_key()`/`from_key()`；**读坏的值回退到默认而不是报错** —— 一个偏好不值得拒绝同步。
- **序列化契约**：serde 用 `tag` + `content`（`{"kind":"years","value":2}`）。**这是一个只在运行期
  才会炸的坑**：带载荷的 newtype 变体用内部 tag，serde 会拒绝序列化。所以加了 JSON 双向往返测试
  把线格式钉死，并断言确切的字符串。
- **持久化**：`UserStore::sync_window()/set_sync_window()`，存 `user.db` 的 `kv`（键 `sync_window`）
  ⇒ 删缓存**不会**重置同步偏好。测试覆盖：新装默认 2 年、往返、重启后仍在。
- **命令**：`get_sync_window` / `set_sync_window`（已注册）。设置窗口**不删已缓存的邮件** ——
  收窄只是停止未来的回填，腾空间是另一个显式动作。
- 测试 **148**（lib 115）；`check.ps1` 全绿；启动冒烟通过。

**未完（下一步，M1a-3b）**：把窗口真正接进同步路径 —— `refresh_folder` 的初始窗口与历史回填循环
按窗口边界停（Years 用邮件日期判断、Count 用条数、All 走配额账本），以及设置界面里的选择器。
在那之前，窗口"存得住、读得出、传得通"，但**还没有改变同步行为**——这一步不能算完成。

### M1a-3b 同步窗口**真正生效**（回填引擎 + 窗口语义 + 命令）✅（第 9 轮）
上一轮说过"窗口存得住但还没改变同步行为"，这一轮把它补上：
- 新模块 `backfill.rs`：`backfill_older(..., window, budget, now)`，一次调用只做有界的工作，停在三种
  **正常**情况（窗口满足 / 历史到底 / 预算用尽）。默认预算 = **1 页**（一次点击不会变成十分钟下载）。
- **测试抓到两个真实缺陷**（这就是为什么"能跑通"不等于"对"）：
  1. 页大小原来固定 `INITIAL_SYNC` ⇒ 为了满足"20 封"的窗口，它把整个邮箱（104 封）都拉了下来。
     改成按窗口差多少取多少。
  2. 测试的第二个场景用了只有 4 封的夹具 ⇒ "一页跨过日期边界"是**空断言**；补成和第一个场景一样的
     104 封夹具后才有意义。
- 端到端断言：`Count(20)` ⇒ 停在**正好 20 封**（UID 85..=104，而不是邮箱底部）；`Years(2)` ⇒
  第一页越过日期边界即停。**窗口现在真的决定同步行为**。
- 命令 `backfill_to_window`（读 `user.db` 里的窗口、跑有界预算、返回报告），并踩实了 HANDOFF 记的那个
  坑：Tauri 命令的返回类型必须 `Serialize`，而报错长得像 `Send` 问题。
- 测试 **151**（lib 117 + loopback 30）；`check.ps1` 全绿；启动冒烟通过。

**仍未做**：设置界面里的窗口选择器；冷启动后自动回填一页并由配额账本节流（目前是用户触发）。

### M1a-4 草稿落库（`drafts` 真正可用）✅（第 10 轮）
`userdb` 增加 `Draft` 类型与四个操作：`save_draft`（**upsert**）/ `load_draft` / `list_drafts` /
`delete_draft`。
- **upsert 而不是 insert**：写信时是自动保存，同一个草稿 id 会反复送来；这里报"主键冲突"就等于丢一次击键。
  测试同时断言 `created_at` 被保留、`updated_at` 更新。
- **附件只存路径 + sha256，不存字节**：带 100 MB 附件的草稿不能把用户库变成邮件的第二份拷贝。
- 列表按 `updated_at` 倒序且按账号隔离；删除是永久动作（界面里没有撤销）并返回"是否真的删到了"。
- 5 条测试，其中一条专门验证**重启后草稿还在** —— 这正是草稿不放在"可删缓存"里的理由。
- 测试 **156**（lib 122 + loopback 30）；`check.ps1` 全绿。

**未做**：写信界面接草稿保存（当前 composer 的状态在闭包里，不落库）；发件箱的发送/重试/撤销逻辑
（`outbox` 表已就位）。这两件是 M1a 里"草稿与发件箱"的剩余部分。

## 下一步（M0 只剩两个被外部条件卡住的项，主线转到 M1a）

**M0 收口情况**：M0-1/2/3/4/5/7 **已完成且全绿**；M0-6（WinAppSDK 通知）与 M0-8（OAuth 端点探测）
**不是没做，而是当前环境做不了**：前者缺运行时与依赖、后者缺你的凭据 —— 两者的恢复步骤与判定标准
都已写在本文件上面（含可直接复制的命令）。按纪律，它们**不阻塞** M1a 的开工。

### M1a 第一步：窗口同步 + 历史回填（根 `PLAN.md` 的既有计划）
`PLAN.md`（约 2300 行）已经写了逐行改造：before/after 代码、精确锚点、每步验收。
**开工前必须先做一件事**：`PLAN.md` 是 M0-2 之前写的，里面的 `cache_page`/`Cache` 调用要对着
现在共享句柄（`state.cache.clone()`、`write_blocking`）核一遍——凡是"每条命令开连接"的写法都要改掉，
否则会把刚拆掉的东西又装回去。核完就按它的步骤执行（它同时解锁 README 里"缓存只覆盖最新 500 封"）。

### M1a 第二步：`user.db` 第一次真正落地
`DbSet` 已经在 `db.rs` 里；按 `design/03` §3.1 建 `drafts`/`outbox`/`pending_ops`/`rules`/`contacts`
（`user.db` 用 `synchronous = full`，与 `cache.db` 分开），并把「清理缓存只清 `origin='server'`」这条
接进清理入口。

### 之后
认证轴（`AuthMethod` + 授权码/应用专用密码文案，P2 已拍板）→ 草稿与发件箱 → 会话串 →
通知托盘 → 安装包与自动更新（`design/19` M1a 的块顺序）。
