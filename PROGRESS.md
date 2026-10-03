# PROGRESS —— M0/M1a 实施进度（自动记录）

> 用户 2026-10-03 夜间指示："开工吧，一直干下去……明早起来看你干完了没"。
> 规则：每个块完成即更新本文件；**未验证的事不写"完成"**；每步都要让 `tools/check.ps1` 全绿。

## 环境与安全网
- 仓库原本**没有 git**，开工前建了基线提交 `83c1b0c`。每一步都可回滚
  （`git log --oneline` / `git diff <sha>`）。
- 设计文档在 `design/`；审计脚本 `python design/tools/audit.py`（当前 0 失败）。
- **测试数量**：基线 116 → 现在 **198**（160 lib + 34 loopback + 4 reachability）。
  `tools/check.ps1` **全绿**：**15 步**（8 项前端/契约不变量 + "命令共享一个数据库句柄" +
  "回填不得推进增量游标" + fmt + clippy -D warnings + build + test + PE=2）。

## 提交记录
共 **22 个提交**，从基线到最新（`git log --oneline` 看全量；下表只列里程碑）：

| SHA | 内容 |
|---|---|
| 最新 | M1a-11 已发送副本：先搜索再 APPEND（spool 即"未归档"标记） |
| — | M1a-10 崩溃恢复：先查已发送再决定重发（含按角色找 Sent 文件夹） |
| — | M1a-9 发件箱界面 + 写信改走队列（撤销窗口真正对用户可见） |
| — | M1a-8 发件箱后端（存储层 + spool + worker + 命令 + 5 秒 tick） |
| — | M1a-7 修发信负载大小写 bug + 写信接草稿 + 草稿箱 |
| — | M1a-6 发件箱决策逻辑 / M1a-5 草稿命令 / M1a-4 草稿落库 / M1a-3b 窗口生效 / M1a-3 窗口模型 / M1a-2 `user.db` / M1a-1b 回填接线 / M1a-1a 回填原语 |
| `62d806c` | M0-2b `Cache`/`src-tauri` 切到共享 `Db`（去掉每条命令开连接）+ 新门禁 |
| `1cc4aef` | M0-2a `db.rs`：单写线程 + 读池 + `DbSet` |
| `66957ad` | M0-7 pragma 定稿 + M0-3 更正推送结论 |
| `c726f98` | M0-5 修增量截断丢信 + M0-1 库能力探针 |
| `83c1b0c` | baseline（开工前的可回滚点） |

---

### M1a-11 已发送副本：先搜索，再 APPEND ✅（第 17 轮）

`design/07` §5.4。要同时避免两种失败：**发出去的信在"已发送"里找不到**（用户最不能接受的失败之一）
与**重复副本**（"服务器会按 Message-ID 去重"只在 Gmail 成立，Dovecot/Exchange/163/QQ 会老实收下第二份）。
两种错误不对称，所以规则只能是一条：**先搜索，搜不到才写**，搜索做**有界**的几次
（服务器替我们归档也是收下之后一小会儿的事）。

**`sentcopy.rs`**：`ensure_sent_copy(client, raw, message_id)`：
- 先按角色拿已发送文件夹（M1a-10 的 `sent_mailbox()`），账号根本没有该文件夹 ⇒ 返回 `None`
  （这是账号的事实，不是这封信的失败）。
- 搜到 ⇒ `{uid, appended:false}`，**一个字节都不写**。
- 搜不到（3 次，累计 ~1.6 秒）⇒ `append(..., \Seen)`，再 `find_uid_by_message_id` 找回 UID。
  **找不回 UID 不算失败**：副本已经在文件夹里了，再写一次才是这个函数存在要防的事
  （APPEND 不回 UID 是 A1-F43 的既定事实）。
- 轮询阶梯有测试钉住"有界且远低于设计的 30 秒上限"——用户正盯着那一行，不该为它等半分钟。

**两处刻意的设计偏离（都写了理由）**：
1. **没有加 `provider.auto_files_sent` 标志**。它唯一的作用是给"确定不归档"的服务商省掉轮询；
   而搜索优先在两种情况下都对，所以它对正确性没有贡献，却多一份会写错的服务商数据——一旦写错
   成"不归档"，代价正是**重复副本**。要加也只能作为优化，不能作为正确性依赖。
2. **没有 `sent_index` 表**（§5.4 步骤 0）。它存在的理由是"崩在中间要知道这封还没归档，而不是
   重复 APPEND"——而"先搜索再写"已经让 APPEND 幂等了。这里改用**更强的标记**：spool 文件不再在
   发送成功后删除，**它的存在本身就等于"已投递但还没归档"**，一次成功的归档才删掉它。
   于是崩溃在任何位置都只是一次重跑，不需要额外的表、不需要 cache.db 迁移。
   **代价**（也是下一步）：`sent_index` 还能给界面"查看已发送副本"的链接与事后对账提供 UID，
   这部分推迟到需要它的界面一起做。

**接线**：`run_due` 成功后**不再删 spool**（草稿照删）；新增 `file_sent_copies` 这一趟，
`src-tauri` 在每次发送趟之后为每个账号跑一次（走连接池 + `ensure_sent_copy`），日志里带上
"归档 / 无法归档 / 待重试"。离线 ⇒ `deferred`，**决定不了就不决定**，字节留在原地等下一趟。

**证据**：
- `loopback.rs::an_already_filed_message_is_found_instead_of_appended_again`：真 IMAP 会话。
  服务器已归档 ⇒ `appended:false`，并且 transcript 里**有 `MESSAGE-ID` 搜索、没有 `APPEND`**
  ——"重复副本"这个失败模式只有这样才看得见。
- worker 三条：归档成功删 spool 并清标记 / 连不上 ⇒ `deferred` 且字节保留 /
  没有已发送文件夹 ⇒ `skipped` 且不再重试。另有轮询阶梯一条。
- 既有测试同步更新：`run_due` 成功**保留** spool（loopback 与单元测试各一条都断言了这一点）。

**数字**：`check.ps1` 15 步全绿；测试 **198**（160 lib + 34 loopback + 4 reachability）；
启动冒烟 10 秒存活、spool 目录干净。

**未做**：`sent_index`（见上，连带界面的"查看副本"）、附件 sha256、崩溃恢复时序的真机验证、
服务器草稿镜像、窗口选择器界面。

---

### M1a-10 崩溃恢复：先查已发送，再决定重发 ✅（第 16 轮）

`design/07` §5.3.1。崩在"正在发送"和"记录结果"之间的行，`state` 会永远是 `sending`；不处理它们
就是**再也发不出去**，处理错了就是**收件人收到两封**——第二次 SMTP 提交对收件人来说就是又一封信
（"靠 Message-ID 去重"只在 Gmail 上成立）。

**判定（纯函数 `recovery_verdict`，6 条测试）**——顺序本身就是结论：
1. 没有冻结的 `Message-ID` ⇒ **问用户**（没有可核对的身份，猜"没发出"就是赌）。
2. 服务器说"已发送文件夹里有它" ⇒ **判 sent，绝不重发**（这一条压过下面所有情况，包括大附件暂停：
   暂停只是为了避免猜测，而这里不需要猜）。
3. 不在已发送里 **且** 大小超过 5 MB ⇒ **问用户**（§5.3.1 的默认暂停：大附件的暴露窗口是
   TLS + 慢速上传的几分钟，正好是 Windows 更新重启的时候）。
4. 不在已发送里且不大 ⇒ **回到 `queued`**（这封信还欠用户）。
5. **服务器问不到**（断网/文件夹打不开）⇒ **原样不动**，报告 `unresolved`；下次再问。
   这里"不知道"绝不能滑向"没发出"。

**已发送文件夹按角色找，不按名字**：新增 `ImapClient::sent_mailbox()`（复用 `list_mailboxes_with_roles`
的 RFC 6154 属性优先 + 英文名回退）——QQ 叫它 `Sent Messages`，服务器也可能只给属性、名字全本地化。
原来 `list_mailboxes` 把 role 丢掉了（因为界面不需要），现在保留在内部的 `*_with_roles` 里。

**接线**（`src-tauri`）：worker 启动后**先跑一次恢复，再进循环**（§5.3.1 的"启动时"）。放在
`start_outbox_worker` 里而不是 `setup` 里，是为了不让开窗等一次网络往返。**没有 in-flight 行的账号
直接跳过**（不能让启动对每个账号都开一条连接）。`sent_folder_has()` 用连接池
（`pool_op!`）+ `find_uid_by_message_id`。

**证据**：
- `loopback.rs::crash_recovery_finds_the_message_in_the_sent_folder_and_does_not_resend_it`：
  真 IMAP 会话。先断言 `sent_mailbox()` 从 `\Sent` 属性解析出 `Sent`；再把一行置为 `sending`，
  跑恢复 ⇒ `already_sent=1`、`requeued=0`、行变 `sent`、`sent_message_id` 记录正确、
  **在 `i64::MAX/2` 时刻也不可再发**；最后断言服务器 transcript 里**真的有一条
  `SEARCH ... MESSAGE-ID`**（靠猜的实现不会留下这条痕迹）。
- 单元测试 3 条 runner 行为（找到 ⇒ 已发出；查不到 ⇒ 重新排队且立即可发；问不到 ⇒ 保持 `sending`）
  + 1 条"冻结字节丢了 ⇒ needs_attention"。

**仍未验证的一处**：worker 里"恢复先于第一轮发送"的**时序**只有代码保证，没有真机探针
（要造一条 >5 MB 的 in-flight 行才安全，否则恢复后会真的去发）。下次真机验证时可以这样做：
预置 spool 缺失的行 ⇒ 期望启动后变 `needs_attention`，全程不碰 SMTP。

**数字**：`check.ps1` 15 步全绿；测试 **193**（156 lib + 33 loopback + 4 reachability）。

**未做**：发送后归档到已发送（§5.4）、附件 sha256、服务器草稿镜像、窗口选择器界面。

---

### M1a-9 发件箱界面 + 写信改走队列 ✅（第 15 轮）

M1a-8 把后端做完了但**用户还走不上那条路**（composer 仍在直接 `send_mail`）。这一轮把界面接上，
并让发送真的经过队列。

**1. 发件箱视图**：侧栏"本地"下新增「发件箱 (N)」，列表显示收件人 / 主题 / 状态 / 已试次数 /
失败原因（红边 + 服务器原话），行内按钮按状态给：`queued` ⇒ **撤销**，
`failed`/`needs_attention`/`cancelled` ⇒ **重试**，另有**删除**。
- **倒计时只改文本节点**：撤销窗口每秒变一次，但整行重建会在指针底下换掉正要点的"撤销"按钮
  ⇒ 只更新 `[data-undo]` 的 textContent。
- 状态文案集中在 `OUTBOX_LABELS`；行预览从 `payload_json` 读（那份负载就是将要发出的信，
  所以**不**再复制一份 subject/收件人到别的列）；负载读不出来也必须能渲染。
- `outbox:changed` 事件 ⇒ 重读 `user.db` 并同时刷新草稿徽标（发送成功会删掉那封草稿）。

**2. 写信改走 `enqueue_send`**（`design/07` §5.1 的唯一发送路径）：
- 点"发送"后弹带**「撤销」按钮**的 toast（"已加入发件箱（5 秒内可撤销）"），撤销按最新一条
  仍在排队的行处理；如果 worker 已经抢先，接口如实返回 false，界面说"已经交给服务器了"。
- **草稿生命周期变了**：不再在点发送时删除（现在删它的是 worker，且只在**真正发出**之后）。
  但入队前会**再存一次草稿**——否则"打完最后几个字就点发送"会让草稿箱里留下两秒前的版本，
  而发送失败时用户重新打开的正是它。
- **删掉 `send_mail` 命令**：两个入口里有一个会绕过撤销窗口、退避策略和崩溃恢复。
  `smtp::send` 保留给对比测试。

**真机证据**（新工具 `tools/probe-outbox.mjs`，CDP 驱动真实窗口；用 `@example.invalid`
收件人 + 30 秒窗口内立即撤销，**不发任何东西**）：
```
enqueue      : state=queued, spool 已记录(336 B), payload 里的主题可读回
outbox_view  : 标题=发件箱, 行已渲染, 徽标=1, 状态=等待发送, 倒计时="30 秒内可撤销", 按钮=[撤销, 删除]
undo         : 点行内"撤销" ⇒ state=cancelled, DOM 显示"已撤销", 徽标清空
discard      : 点"删除" ⇒ 行消失, 列表归零
composer     : 打开写信窗口填好点"发送" ⇒ 真的入队(queued), 弹窗关闭, 随后撤销成功
```
（探针自己清理：删掉了写信留下的那条草稿。`tools/probe-drafts.mjs` 的负载契约检查已改指
`enqueue_send`——同一个 `OutgoingMail`，两边仍然一个字段都不差。）

**数字**：`check.ps1` **15 步全绿**；测试 **186**（150 lib + 32 loopback + 4 reachability，
新增 3 条 `userdb` 队列语义测试：徽标只数未完成、重试回 `queued` 而永久失败不回、
撤销窗口内不可被认领）；前端命令 **22** 个。

**未做**：发送后归档到已发送（§5.4）、崩溃恢复 `sending` 行（§5.3.1）、附件 sha256、
服务器草稿镜像、窗口选择器界面。

---

### M1a-8 发件箱后端：存储层 + spool + worker + 5 秒 tick ✅（第 14 轮）

交接摘要的第二优先级做了一半（后端全通，界面还没接，见"未做"）。

**1. 存储层（`userdb`）**：`OutboxItem` / `NewOutbox` + 入队 / 单条 / 列表 / **到期查询** /
认领 / 成功 / 重试 / 永久失败 / 取消 / 重排 / 删除。
- **写代码时抓到一个设计级 bug**：`design/07` §5.3 的到期查询是 `state='queued'`，而
  M1a-6 的 `OutboxState::Failed` 注释写着"可重试失败，看 `next_attempt_at`"。两者不能同时成立
  ——如果 `failed` 也算到期，一次**永久失败**（5xx）就会被无限重试。**定死为**：`queued` = 还在等
  （撤销窗口/定时/退避，全在三个时间列里），`failed`/`needs_attention` = 终态；重试是
  **回到 `queued`** 并把 `next_attempt_at` 推后。测试 `a_permanent_failure_stops_and_never_retries_itself`
  用 `due_outbox(i64::MAX/2)` 断言它再也不会被捞起来。
- 三个时间列直接比较（`undo_until <= now` 等），不用 `is null or`——它们是 NOT NULL 就是为了这个。

**2. `spool.rs`：字节在入队时冻结**（`design/07` §5.3 第二轮评审的结论）。重试、重启、重建
都不会改变用户批准的那封信；`<outbox id>.eml` 放在自己的目录里，不塞进 SQLite。

**3. `worker.rs`：一轮队列处理**。认领（只有仍到期的行能被认领 ⇒ 两个窗口/两次 tick 不会都发）
→ 取冻结字节（**丢了才**从 `payload_json` 重建，并把新路径写回）→ 交给 SMTP → 落库：
- 成功 ⇒ `sent` + 记录冻结的 Message-ID + 删 spool + **删草稿**（否则会提示"再发一次"）。
- 失败 ⇒ `verdict_for_failure`：552 归 `needs_attention`（要人决定：压缩/链接/去掉附件），
  5xx 永久 ⇒ `failed`，**其余一律重试**（判不准就重试——丢件是这功能唯一不能犯的错）。
- 发件人/收件人：信封收件人取自 `payload_json`（**Bcc 只在信封里**，头部绝不出现）。

**4. 顺手修掉一个真实缺陷：我们的邮件以前没有 `Message-ID`。**
lettre 只在被要求时才写（`build()` 只补 `Date`），而 `design/07` §5.4/§5.3.1 的"已发送副本
按 Message-ID 找回""崩溃恢复先查再发"都靠它。现在 `build_message` 总是写一个（域名取发件人域），
`OutgoingMail` 增加 `message_id`（`#[serde(default)]`）让发件箱把身份**冻结**在入队时
——否则重建 spool 会生成一个新 ID，等于换了一封信。`src/payload.js` 同步加上该键，
契约门禁现在数到 **10 个字段**（两边都对得上）。

**5. `src-tauri`**：`enqueue_send`（冻结 Message-ID → 建 spool → 落库 → 唤醒 worker）、
`list_outbox` / `cancel_send` / `retry_send` / `discard_outbox`；启动时拉起 worker
（**5 秒 tick + 入队/撤销/重试立即唤醒**，播报 `outbox:changed`）。`outbox_count` 改成只数
`queued|sending`——表里还有 sent/cancelled 的历史行，全数会让徽标永远不归零。

**证据**（`crates/mail-core/tests/loopback.rs` 两条端到端，打的是**真** SMTP 会话，不是桩）：
- `a_queued_message_reaches_smtp_through_the_worker`：`user.db` 行 → spool → worker → `send_spool`
  → 服务器 `DATA`。断言**冻结的 Message-ID 就是线上那个**、**Bcc 不在头部**、spool 与草稿都被
  清理、`sent_message_id` 记录正确、**只有一个 SMTP 连接**。
- `a_send_cancelled_inside_its_undo_window_never_opens_a_connection`：窗口内取消后再让窗口过期，
  **连接数是 0**——空 DATA 证明不了"没尝试过"（AUTH 失败也是空 DATA），连接计数才能。
- 单元测试另有：撤销窗口未到不发、认领不可重复、spool 丢失可从 payload 重建、
  4xx 退避 30 秒后回来、5xx 终止、552 要人处理。

**真机冒烟**：应用启动 10 秒存活，`%APPDATA%\com.easyemail.desktop\spool\` 已创建，
`startup-error.log` 没有新增错误（worker 空队列时保持安静）。

**数字**：`check.ps1` **15 步全绿**；测试 **183**（147 lib + 32 loopback + 4 reachability）。

**未做（下一次接着做，按顺序）**
1. **前端还没接队列**：composer 目前仍直接调 `send_mail`，所以用户还没走上"撤销窗口"这条路。
   要一起做：发件箱视图（`list_outbox` + 撤销/重试/删除）、发送后的"正在发送…（N 秒内可撤销）"、
   把 composer 切到 `enqueue_send`。**这三件必须同时落地**——只切发送而没有发件箱界面，
   失败的信会没有地方可看。
2. **发送后归档到已发送**（`design/07` §5.4 `ensure_sent_copy`：写 `sent_index` → 按 Message-ID
   有界轮询 → `append` + `find_uid_by_message_id` 找回 UID）。
3. **崩溃恢复 `sending` 行**（§5.3.1）：启动/worker 启动时先按冻结 Message-ID 去已发送里查，
   查到就判 sent（**不重发**），查不到才回 `queued`；>5 MB 的默认先暂停。
4. 附件 sha256 校验（`design/07` §9 的"发送前补算"）与 `attachment_changed` 判定。

---

### M1a-7 发信负载大小写 bug + 写信真正接草稿 + 草稿箱 ✅（第 13 轮）

这一轮先撞上一个**上线级 bug**，再完成交接摘要里的第一优先级。

**1. `send_mail` 从 v0.1 起就不可能成功**（真机 CDP 探针实测，不是推理）：
渲染层发的是 `bodyText`/`inReplyTo`，而 Tauri 只把**命令参数名**做 camelCase↔snake_case
转换，参数**值**里的结构体是 serde 按 Rust 字段名逐字反序列化的 ⇒ 每次点"发送"都得到
`invalid args \`mail\` for command \`send_mail\`: missing field \`body_text\``。编译器、`cargo test`、
`node --check` 全都看不见它——这就是它活了这么久的原因。
- **单一来源**：新增 `src/payload.js`，`send_mail`/`save_draft` 的负载只在这里构造（键就是
  Rust 字段名），`app.js` 只能通过它构造。
- **新门禁**（`check.ps1` 第 8 步）：`tools/payload_keys.mjs` 把 `payload.js` 里每个构造器的**键集合**
  与 Rust 里对应的 `pub struct` 字段集合**双向**比对（多一个键、少一个字段都红）。
  **证伪验证**：把 `body_text` 改成 `bodyText` ⇒ 门禁同时报"struct 里没有这个字段"和
  "payload 少了这个字段"（两侧都报）；改回即绿。
- **运行时也变响**：`OutgoingMail` / `OutgoingAttachment` / `Draft` 加 `#[serde(deny_unknown_fields)]`
  —— 否则一个拼错的**可选**字段（如 `bodyHtml`）会被静默忽略，富文本邮件悄悄变成纯文本。
  3 条新 Rust 测试钉住"正确负载能反序列化 / 拼错的键必须报错"。
- **真机探针** `tools/probe-drafts.mjs`（`node tools/inspect-dom.mjs @tools/probe-drafts.mjs`）：
  正确负载 ⇒ 报 `找不到账号「…」`（**已经进了命令体**，证明负载被接受）；v0.1 的
  camelCase 负载 ⇒ 报 `unknown field \`bodyHtml\`, expected one of … \`body_text\` …`（证明探针真能抓到这个 bug）。

**2. 写信接草稿**（`design/07` §3/§5.1 的本地草稿部分）：
- 2 秒去抖自动保存（`DRAFT_AUTOSAVE_MS`）+ **失焦/关闭时保存**（`openModal` 新增 `onClose` 钩子，
  在拆 DOM **之前**读字段）；发送成功后才 `delete_draft`，且**先等排队中的保存落地**再删，
  否则一次跑在删除之后的自动保存会在草稿箱里留下幽灵行。
- **空草稿不入库**：没写任何字的写信窗口关闭后不该留下一行；已经落过库的草稿即使被清空也
  保留（`design/07` §9：宁可多留一份）。
- 草稿 id 用 `newDraftId()`（时间戳+随机）而不是 `crypto.randomUUID()`——webview 算不算
  secure context 是运行期细节，为它赌上"第一次击键就抛异常"不划算。
- 附件只存路径+元数据，`sha256` 记 `null`（**未做**：`pick_attachments` 还没算哈希，
  `design/07` §9 的"发送前补算"仍待做）。
- 状态行：`草稿 · 已保存 14:32` / 失败时红字提示（关窗后改用 toast，因为状态行已经不在 DOM 里）。

**3. 草稿箱界面**：侧栏"本地 / 草稿箱 (N)"一行；列表按 `updated_at` 倒序、显示收件人与主题、
点行续写、行内"丢弃"（`confirm` 后永久删除）；切账号会重新读该账号的草稿，搜索/选文件夹则
回到邮件视图（`state.view`）。

**真机端到端证据**（CDP 驱动真实窗口，探针自己清理，不留测试数据）：
`save_draft` 接受 ⇒ 计数 +1、附件与 References JSON 原样存回；草稿箱渲染 1 行 + 徽标 1 + 标题
"草稿箱"；点开续写把 收件人/主题/正文/1 个附件 全部填回，状态行 `草稿 · 上次保存 09:28`；
改正文后 2.6 秒 ⇒ 状态行 `已保存` 且库里是新正文；点 ✕ 关闭 ⇒ 库里是**关闭前最后一版**；
点"丢弃" ⇒ 行消失、计数回到开跑前。
（探针还抓到一个真实小 bug：初始状态行写在 `openModal` 之前，落到了上一个对话的 DOM 上，
所以续写时状态行是空的——已修成在 `onReady` 里写。）

**数字**：`check.ps1` **15 步全绿**；测试 **166**（132 lib + 30 loopback + 4 reachability）；
前端命令 **18** 个（新增 `save_draft`/`list_drafts`/`delete_draft` 的调用）；启动冒烟 9 秒存活。

**未做**：附件 sha256；服务器草稿镜像（`draft_mirror`）；发件箱 worker（下一优先级）。

---

### M1a-6 发件箱的决策逻辑（纯函数 + 7 条测试）✅（第 12 轮，最后一个自动轮）
新模块 `outbox.rs` 把发件箱 worker 里**该做决定的部分**从 I/O 里拆出来，变成可单测的纯函数：
- `OutboxState`（queued / sending / sent / failed / cancelled / **needs_attention**）+ `parse_state`：
  **读不懂的状态一律落到 needs_attention，绝不能变成 queued**（一条无法解释的行不该被自动发出去）。
- `next_attempt_delay_secs`：30 秒起、翻倍、**6 小时封顶**；断言"永不回缩、到顶不再涨"，
  并覆盖 `u32::MAX` 这种被写坏的尝试次数（不会溢出成负数延迟）。
- `is_permanent_failure`：4xx 可重试；**421/450/451/452 虽然属于 5xx/4xx 的边界但按可重试处理**；
  5xx 属永久（含 **552 "邮件太大"** —— 重试只会再失败）；**超出 SMTP 范围的码按可重试**，
  免得一个解析小毛病把排队的邮件悄悄丢掉。
- `needs_user_decision`：附件被改/丢失、服务器嫌太大 ⇒ 交给用户，不自动重试。
- 测试 **163**（lib 129 + loopback 30 + reachability 4）；`check.ps1` 全绿。

**未做**：worker 本体（SMTP 发送、撤销窗口、开机时先查已发送再决定重发）与前端发件箱界面。

## 交接摘要（先读这里）

**当前状态**：`tools/check.ps1` **15 步全绿**；`cargo test --workspace` **198 通过、0 失败**
（160 lib + 34 loopback + 4 reachability）；`python design/tools/audit.py` **0 失败**；
应用启动冒烟通过（10 秒存活）。发件箱这条链现在是完整的：写信 → 入队（撤销窗口）→ worker 发送 →
**崩溃恢复先查已发送再决定重发** → **已发送副本先搜索再 APPEND**。所有改动都在 git 里
（基线 `83c1b0c`，共 36 个提交）。

**磁盘上已验证的三件事**（不是文档结论，是真机查过的）：
- `%APPDATA%\com.easyemail.desktop\cache.db`：`auto_vacuum = 2`（老库已后台重建为 incremental）、
  表与 `user_version` 完好 —— "清理缓存真的能腾出磁盘"这条对老用户也成立。
- `%APPDATA%\com.easyemail.desktop\user.db`：`synchronous = 2 (FULL)`、五个表齐全
  —— 与 `cache.db`（NORMAL）各自独立，草稿/待发送不会因为"删缓存"而丢。
- **草稿真的走通了**（CDP 探针驱动真实窗口，`tools/probe-drafts.mjs`）：写 → 2 秒自动保存 →
  关闭时保存 → 草稿箱列出 → 点开续写填回 → 丢弃删除，全链路通过；同一探针还证明
  `send_mail` 的负载契约是真的——v0.1 的"发送"按钮**从来没能成功过**（见 M1a-7）。

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
| M1a-5/6 | 草稿命令；发件箱决策纯函数（状态解析 / 退避 / 永久失败判定 / needs_attention） | 7 条测试（含 `u32::MAX` 尝试次数与超范围 SMTP 码） |
| **M1a-7** | **修 `send_mail` 负载大小写 bug（v0.1 起发送从未成功）+ 写信接草稿（2 s 自动保存 / 关闭保存 / 发送后删草稿）+ 草稿箱界面 + 契约门禁** | **`tools/probe-drafts.mjs` CDP 全链路通过；门禁可证伪（改回 camelCase 即红）** |
| **M1a-8** | **发件箱后端：`userdb` 队列操作 + 入队冻结 spool + worker（认领/退避/needs_attention/成功后删草稿）+ 5 条命令 + 5 秒 tick 与唤醒；补上一直缺失的 `Message-ID`** | **loopback 两条端到端打真 SMTP：冻结 Message-ID 上线、Bcc 不在头部、spool/草稿被清理、撤销后 SMTP 连接数为 0** |
| **M1a-9** | **发件箱界面（状态/倒计时/撤销/重试/删除）+ 写信改走 `enqueue_send`（撤销窗口对用户可见）+ 删掉会绕过队列的 `send_mail`** | **`tools/probe-outbox.mjs` 真机全链路：入队→界面→点撤销→删除→**写信窗口点发送真的入队**（全程不发信）** |
| **M1a-10** | **崩溃恢复 `sending` 行：按冻结 Message-ID 查已发送 ⇒ 已发出判 sent（绝不重发）/ 查不到回队列 / 大附件问用户 / 问不到原样不动；`sent_mailbox()` 按 RFC 6154 角色找文件夹** | **loopback 真 IMAP：解析出 `Sent` → 恢复判 `sent` 且不可再发 → transcript 里确有 `SEARCH ... MESSAGE-ID`** |
| **M1a-11** | **已发送副本：先搜索再 APPEND（幂等）、spool 即"未归档"标记、`file_sent_copies` 一趟 + 离线延后；刻意不加 `auto_files_sent` 标志与 `sent_index`（理由见 M1a-11）** | **loopback：服务器已归档 ⇒ `appended:false`，transcript 里有 `MESSAGE-ID` 搜索、**没有 `APPEND`**（重复副本这个失败模式只有这样才看得见）** |

**被外部条件卡住的两项（不是没做，是做不了；步骤已写清）**
1. **M0-6 WinAppSDK 通知**：本机没装 Windows App SDK 运行时，`src-tauri` 也没有 `windows` crate 依赖。
   恢复：`winget install Microsoft.WindowsAppRuntime.1.7` → 加 `windows` crate 的 AppNotifications
   features → Register + 双按钮 + `NotificationInvoked` → 量框架依赖/自包含的体积。
2. **M0-8 OAuth 端点探测**：需要你的 client_id 与网络。命令已写在本文档 M0-8 一节，
   结论只有两种：能不带 `client_secret` 换到令牌（PKCE 够）或不能（BYO 向导要加一个输入框）。

**下一步优先级**（每一件都自包含，做完各让 `check.ps1` 与测试保持全绿）
1. **附件 sha256**（`design/07` §9"发送前补算"）：现在草稿里 `sha256` 记 `null`，
   所以 `attachment_changed`/`attachment_missing` 这两条 `needs_attention` 分支还没有触发源。
   同时是"发送失败后重试前校验附件没被改过"的前提。
2. **`sent_index`（cache.db）+ 界面的"查看已发送副本"**：现在副本的 UID 只存在于返回值里，
   没有落库。要加就按 §5.4 步骤 0 的顺序（先写 `pending_file` 再归档），并给 cache.db 加一次真迁移。
3. **真机验证**：崩溃恢复时序（预置 spool 缺失的 `sending` 行 ⇒ 期望 `needs_attention`，全程不碰 SMTP）
   + 已发送副本在这台机器的 QQ 账号上真的能落进"已发送"（这一条会真的开一次 IMAP + 可能 APPEND
   到用户自己的邮箱，跑之前要先问）。
4. **服务器草稿镜像**（`draft_mirror`，APPEND + `find_uid_by_message_id`，best-effort 不阻塞写信）。
5. **窗口选择器界面** + 冷启动自动回填一页（并接配额账本，`design/04` §5.7）。
6. 认证轴（`AuthMethod`：应用专用密码为主 + BYO，P2 已拍板）→ 会话串 → 通知托盘 → 安装包与自动更新。

**运行方式**
```
.\tools\check.ps1                     # 15 步门禁（前端不变量 / 负载契约 / fmt / clippy / build / test / PE=2）
cargo test --workspace                # 198 个测试
python design\tools\audit.py          # 设计文档一致性（对应 design/ 的 23 份文档）
git log --oneline                     # 36 个提交；git diff 83c1b0c HEAD 是本次全部改动

# 真机端到端（要临时把 --remote-debugging-port=9222 加回 tauri.conf.json 再 cargo build）
node tools/inspect-dom.mjs @tools/probe-drafts.mjs    # 草稿：写→自动保存→关闭保存→草稿箱→丢弃
node tools/inspect-dom.mjs @tools/probe-outbox.mjs    # 发件箱：入队→撤销→删除→写信点发送真的入队
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
