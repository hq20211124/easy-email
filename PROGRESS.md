# PROGRESS —— M0/M1a 实施进度（自动记录，按时间倒序追加）

> 用户 2026-10-03 夜间指示："开工吧，一直干下去……明早起来看你干完了没"。
> 规则：每个块完成即更新本文件；**未验证的事不写"完成"**；每步都要让 `tools/check.ps1` 全绿。

## 环境与安全网
- 仓库原本**没有 git**，我在开工前建了基线提交：`83c1b0c baseline: v0.2 + design/ 全部设计`。
  今晚的每一步都可回滚（`git log --oneline` / `git diff`）。
- 设计文档在 `design/`，审计脚本 `python design/tools/audit.py`（当前 0 失败）。

---

## 已完成

### M0-5 修"增量截断丢信"bug ✅（2026-10-03 夜）
**问题**（design/04 §5.3 + A2 第二轮评审 BLOCKER）：
`fetch_headers_since` 在"一次到达的邮件多于一个批次（500）"时**保留最新一批**（`split_off`），
而游标按"已存储的最大 UID"推进 ⇒ 两次之间那些**从未存储的 UID 永远落在游标之下**，
再也不会被取回：静默、永久地丢信，没有任何错误可展示。

**改动**（`crates/mail-core/src/imap.rs`）：
1. 新增 `since_range(after_uid, limit) -> String`：把 `UID FETCH <n>:*` 收窄成 `<n>:<n+limit-1>`
   （`limit == 0` 保持 `:*` 的开放语义）。这既避免服务器白送一批我们马上要丢掉的头部，
   也顺带修掉了丢信：服务器不再回超过一页的量。
2. 新增 `keep_oldest_page(summaries, limit)`：截断**保留最旧**（`truncate`）而不是最新，
   作为纵深防御（服务器若无视区间上限，行为仍然安全）。
3. 两处都写了"为什么"的注释，并把历史（旧行为会丢信）钉在测试里。

**证据**：
- 单元测试 7 条（`imap::tests`）：保留最旧 / 少于一批 / limit=0 不限制 / 正好一批 /
  区间有界 / 无界 / UID 空间顶端不回绕（`since_range(MAX-1, 10)` 不会产生倒序区间）。
- 端到端回归测试 1 条（`tests/loopback.rs::a_burst_larger_than_one_batch_leaves_no_hole`）：
  冷同步 4 封 → 一次到达 **600 封** → 第一次增量必须只存 500 封且游标停在 504，
  缓存 UID 必须是**连续的 1..=504**（有洞即失败）→ 第二次增量补齐到 604。
- **证伪验证**：把两处改动临时还原成旧行为，该测试**失败**并明确指出
  `cursor 604 != 504`（即旧代码确实会跳到最新 UID 并留下空洞）；恢复后通过。
- 夹具（`tests/loopback.rs`）随之改造：邮箱大小改为 `RICH_MESSAGES + extra` 且**实时读取**，
  UID > 4 的邮件按需合成（唯一 Message-ID）；这样"到达"的邮件真的可下载
  —— 旧夹具只能改变 EXISTS 计数，下载不到新邮件。
- 全量：`cargo test -p mail-core` → 94 lib + 23 loopback + 4 reachability = **121 通过，0 失败**
  （基线是 116）。

**遗留**：无。M0-5 闭环。

---

### M0-1 库能力探针 ✅（2026-10-03 夜）
把设计依赖的**库事实**钉成可执行断言（放在 `tests/loopback.rs` 末尾的"库能力探针（M0-1）"一节，
复用同一套假服务器 —— 另开一个文件就得再维护一份夹具）。

| 探针 | 结论 | 对应 |
|---|---|---|
| `probe_idle_delivers_exists_before_the_window_closes` | **成立**：5 s 的 idle 窗口下，一次 `* n EXISTS` 推送在 **< 2 s** 内就变成 `PushEvent::Changed`（实测约 0.2 s）；"推送延迟 = 窗口"的旧说法彻底作废 | A1-F41 |
| `probe_append_uid_is_parsed_and_then_discarded` | **成立**：假服务器回了 `[APPENDUID 1 4242]`，而 `append` 返回 `()` ⇒ 唯一拿回 UID 的路是 `find_uid_by_message_id`（`UID SEARCH HEADER Message-ID`），探针证明它能取回 4242 | A1-F43 |
| `probe_partial_fetch_with_offset_is_an_experiment` | **成立（实测复核）**：`BODY[]<5.10>` 让 imap-proto 解析失败（`TakeWhile1`），错误从 `fetch_message_range(1, 5, 10)` 冒出来 ⇒ 设计里"附件无法真正流式下载"的判断是对的 | A1-F42 |
| `probe_references_has_three_shapes` | **原文一半是错的**：0.11.9 下 `as_text_list()` 对单 id 也返回 `Some(1)`（不是 None）；真正的坑是 `as_text()` 在多 id 上只返回**最后一个**。已更正 A1-F46，规则改为"读引用一律用 `as_text_list()`" | A1-F46 |
| `probe_lettre_never_sends_size` | **成立**：假服务器 EHLO 宣告 `SIZE 10485760`，转录里 `MAIL FROM` **没有** `SIZE=` ⇒ 发送前预检只能靠本地表 + 硬上限 | A1-F45 |

**顺带实现的两个 M1a 原语**（探针需要它们，反正 07 §5.4 也要）：
- `ImapClient::append(mailbox, raw, seen)` —— 明确返回 `()`，文档写清"拿不到 UID，必须用搜索找回"。
- `ImapClient::find_uid_by_message_id(mailbox, message_id) -> Option<u32>` —— 取最大 UID（同一 Message-ID 可能多份）。

**夹具改造**：新增 APPEND（**必须先回 `+` 续行再读 literal** —— 顺序错会死锁，这是我第一次写错的地方）、
`UID SEARCH HEADER Message-ID`、SMTP 命令转录。

**证据**：`cargo test -p mail-core` → 94 lib + 28 loopback + 4 reachability = **126 通过**；
`tools/check.ps1` **全绿**（fmt / clippy -D warnings / build / test / PE=2）。

---

## 待办（按 design/19-roadmap.md §4 的 M0 顺序）
- [ ] **M0-2** `DbSet`（cache/user 双句柄 + 单写线程 + 读池）替换"每条命令开连接"
- [ ] **M0-7** 建库 pragma 定稿（`auto_vacuum=incremental` 必须在建表前设）
- [ ] **M0-3** 更正 README/HANDOFF 与代码注释里**已被推翻**的结论（"推送 50 秒延迟"等）
- [ ] **M0-4** 是否 `[patch.crates-io]` 修 imap-proto 的 `<origin.offset>`（决定附件能否流式）
- [ ] **M0-6** WinAppSDK 通知 spike（需要 Windows App SDK 运行时；离线可能做不了，会如实记录）
- [ ] **M0-8** OAuth 令牌端点探测（需要真实端点与凭据；只能写好探针并标"待在有网络+凭据时跑"）
- [ ] M1a：窗口同步/历史回填（含根 `PLAN.md` 的既有计划）、认证轴、草稿与发件箱、会话串、通知托盘、安装包与自动更新
