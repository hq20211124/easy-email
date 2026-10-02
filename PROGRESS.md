# PROGRESS —— M0/M1a 实施进度（自动记录）

> 用户 2026-10-03 夜间指示："开工吧，一直干下去……明早起来看你干完了没"。
> 规则：每个块完成即更新本文件；**未验证的事不写"完成"**；每步都要让 `tools/check.ps1` 全绿。

## 环境与安全网
- 仓库原本**没有 git**，开工前建了基线提交：`83c1b0c`。今晚每一步都可回滚
  （`git log --oneline` / `git diff <sha>`）。
- 设计文档在 `design/`，审计脚本 `python design/tools/audit.py`（当前 0 失败）。
- **测试数量**：基线 116 → 现在 **134**（102 lib + 28 loopback + 4 reachability）。
  `tools/check.ps1` 全绿（fmt / clippy -D warnings / build / test / PE=2 / 7 项前端不变量）。

## 提交记录
| SHA | 内容 |
|---|---|
| `1cc4aef` | M0-2a `db.rs`：单写线程 + 读池 + `DbSet` 双句柄 |
| `66957ad` | M0-7 建库 pragma 定稿 + M0-3 更正被推翻的推送结论 |
| `c726f98` | M0-5 修增量截断丢信 + M0-1 库能力探针 |
| `83c1b0c` | baseline（v0.2 + design/ 全部设计） |

---

## 已完成

### M0-5 修"增量截断丢信"bug ✅
**问题**：`fetch_headers_since` 在"一次到达多于一个批次（500）"时**保留最新一批**，
而游标按"已存储的最大 UID"推进 ⇒ 中间那些从未存储的 UID 永远落在游标之下，**永久丢信且无报错**。

**改动**（`crates/mail-core/src/imap.rs`）：
1. `since_range(after_uid, limit)`：把 `UID FETCH <n>:*` 收窄成 `<n>:<n+limit-1>`（`limit==0` 保持 `:*`）。
2. `keep_oldest_page(...)`：截断保留**最旧**（纵深防御，服务器若无视区间上限仍安全）。
3. `append()` 与 `find_uid_by_message_id()`：M1a 的两个原语（07 §5.4 要用，探针也需要）。

**证据**：单元测试 7 条 + 端到端回归 1 条（4 封冷同步 → 一次到 **600 封** → 第一批只存 500、
游标停 504、UID 必须**连续 1..=504**、第二批补齐 604）。**证伪验证**：临时还原旧行为 ⇒ 该测试失败
（`cursor 604 != 504`）⇒ 测试确实能抓到这个 bug。

### M0-1 库能力探针 ✅
| 探针 | 结论 |
|---|---|
| `probe_idle_delivers_exists_before_the_window_closes` | **成立**：5 s 窗口下推送在 ~0.2 s 变成 `PushEvent::Changed`（旧的"50 秒延迟"彻底作废） |
| `probe_append_uid_is_parsed_and_then_discarded` | **成立**：服务器回 `[APPENDUID 1 4242]`，`append` 返回 `()` ⇒ 靠 `find_uid_by_message_id` 取回 4242 |
| `probe_partial_fetch_with_offset_is_an_experiment` | **成立（实测复核）**：`BODY[]<5.10>` 让 imap-proto 解析失败 ⇒ 附件确实无法**真正**流式下载 |
| `probe_references_has_three_shapes` | **原文一半是错的**：0.11.9 下 `as_text_list()` 对单 id 返回 `Some(1)`（不是 None）；真正的坑是 `as_text()` 在多 id 上只返回**最后一个**。已更正 A1-F46 |
| `probe_lettre_never_sends_size` | **成立**：EHLO 宣告 `SIZE 10485760`，转录里 `MAIL FROM` 没有 `SIZE=` ⇒ 预检只能靠本地表 |

**夹具改造**：APPEND（**必须先回 `+` 续行再读 literal**，顺序错会死锁）、`UID SEARCH HEADER Message-ID`、
SMTP 命令转录、邮箱可增长（`RICH_MESSAGES + extra` 实时读取）。

### M0-7 建库 pragma 定稿 ✅
- `cache.rs`：`apply_pragmas()` + `auto_vacuum_state()` / `freelist_bytes()` / `vacuum_into_freelist()` /
  `rebuild_with_auto_vacuum()`；三条测试（新库读回 2、老库上 pragma 被静默忽略、重建后可回收空闲页）。
- **实测收紧了一条设计规则**：`auto_vacuum` 必须是**第一条 pragma**，排在 `journal_mode = wal` **之前**
  —— 切 WAL 会写数据库头，之后 SQLite 就静默忽略 auto_vacuum（两种顺序都实测过：先 journal 读回 0，
  先 auto_vacuum 读回 2）。`design/03` 与 `design/15` 已按此收紧，`cache.rs` 也改了顺序。

### M0-3 更正被推翻的结论 ✅
- `HANDOFF.md`：删掉"async-imap 不投递 `* n EXISTS`"，改成实测结论（~0.2 s、`NewData`），
  并写明"判断有变化时要刷新基线，否则心跳会再触发一次同步"；测试数字更新为 134。
- `README.md`：推送一节重写（亚秒级 + 探针引用），已知限制里"最多 50 秒延迟"划掉。
- `idle.rs`：IDLE 窗口 **50 s → 120 s**（它是心跳而不是延迟），注释写清更正的原因。

### M0-2a `db.rs`：数据库句柄 ✅（M0-2 的地基）
- **`Db`**：一个写线程（`std::thread` + `mpsc`，回值走 `oneshot`）+ `read_conns`（默认 3）条读连接。
  * `write` 全程包在一个事务里（`unchecked_transaction` —— `Transaction` 只有 `Deref`，没有 `DerefMut`），
    带 `SQLITE_BUSY` 重试；闭包是 `FnOnce(&Connection) + Send + 'static`（这是"在别的线程上跑"的
    诚实描述，不是 `pool_op!` 的借用陷阱）。
  * `read` 走 `spawn_blocking`；`read_blocking` 供启动期用。
  * **`Db` 内不得 `tokio::spawn`**（`setup()` 没有 Tokio 上下文，这正是 HANDOFF 记的"启动即消失"）。
- **`DbSet`**：`cache` / `user` 两个句柄，各自 pragma（`normal` / `full`），**不 ATTACH**。
- **`MailError::Busy`**：在 `From<rusqlite::Error>` 里统一分类锁竞争（`SQLITE_BUSY`/`SQLITE_LOCKED`）。
- 测试 5 条：失败写整笔回滚、50 个并发写全落地且串行、**WAL 下读不被长写阻塞**（同时断言未提交行不可见）、
  两个库各持自己的 pragma 与文件、`quick_check` 正常。
- 顺带发现并记录：**读连接在文件初始化之前打开时 `pragma auto_vacuum` 会读回陈旧值** ⇒
  `auto_vacuum_state()` 走写线程（诊断不能撒谎）。

---

## 下一步（M0-2b，下一轮接着做）
**目标**：把 `Cache` 与 `src-tauri` 从"每条命令 `Cache::open`"切到共享的 `Db`/`DbSet`。
1. `Cache` 内部字段 `conn: Connection` → `db: Db`；约 40 个方法体包进 `self.db.read(...)` / `write(...)`。
   （`Cache::open_in_memory()` 保留为 `Db::open_in_memory` 的薄包装，134 个既有测试是安全网。）
2. `src-tauri/src/lib.rs` 的 `AppState` 持有 `DbSet`；`setup()` 里打开一次，命令里不再开连接。
3. `user.db` 第一次真正落地：`drafts`/`outbox`/`pending_ops`/`rules`/`contacts` 表（按 `design/03` §3.1）。
4. 门禁：命令里出现 `Cache::open` 的次数 == 0（设计 §10 的新检查）。

## 待办（M0 剩余 + M1a）
- [ ] **M0-2b** 见上
- [ ] **M0-4** 是否 `[patch.crates-io]` 修 imap-proto 的 `<origin.offset>`（M0-1 探针已证实"不修就不能流式取附件"）
- [ ] **M0-6** WinAppSDK 通知 spike（需要 Windows App SDK 运行时；若离线做不了会如实记录）
- [ ] **M0-8** OAuth 令牌端点探测（需要真实端点 + 凭据 ⇒ 只能把探针写好并标"待有网+凭据时跑"）
- [ ] M1a：窗口同步/历史回填（含根 `PLAN.md` 的既有计划）、认证轴、草稿与发件箱、会话串、通知托盘、安装包与自动更新
