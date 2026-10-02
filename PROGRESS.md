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

---

## 下一步
### M0 剩余
- [ ] **M0-4** imap-proto 的 `<origin.offset>` 决策：M0-1 已证实"不修就不能真流式取附件"。
  明天做：评估 `[patch.crates-io]` 挂本地 fork（3 行改动）的维护成本，做就顺手实现
  `fetch_message_range` 的分块下载；不做就把"整封取回"的代价写进 README。
- [ ] **M0-6** WinAppSDK 通知 spike：需要 Windows App SDK 运行时（离线可能装不上）。
  做不了会如实记录，并保留 13 §3.1 的回退分支。
- [ ] **M0-8** OAuth 端点探测：需要真实端点 + 你的凭据 ⇒ 把探针写好、标明"待有网+凭据时跑"。

### M1a（按 design/19 §4）
1. **窗口同步 + 历史回填**：根 `PLAN.md` 里已有逐行改造计划（before/after 代码、锚点、验收），
   直接执行；它同时解锁 README 里"缓存只覆盖最新 500 封"这条限制。
2. `user.db` 第一次真正落地：`drafts`/`outbox`/`pending_ops`/`rules`/`contacts` 表（design/03 §3.1）
   —— `DbSet` 已经在 `db.rs` 里，接线即可。
3. 认证轴（`AuthMethod` + 授权码/应用专用密码文案，P2 已拍板）→ 草稿与发件箱 → 会话串 →
   通知托盘 → 安装包与自动更新。
