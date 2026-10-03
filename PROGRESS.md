# PROGRESS —— M0/M1a 实施进度（自动记录）

> 用户 2026-10-03 夜间指示："开工吧，一直干下去……明早起来看你干完了没"。
> 规则：每个块完成即更新本文件；**未验证的事不写"完成"**；每步都要让 `tools/check.ps1` 全绿。

## 环境与安全网
- 仓库原本**没有 git**，开工前建了基线提交 `83c1b0c`。每一步都可回滚
  （`git log --oneline` / `git diff <sha>`）。
- 设计文档在 `design/`；审计脚本 `python design/tools/audit.py`（当前 0 失败）。
- **测试数量**：基线 116 → 现在 **223**（181 lib + 38 loopback + 4 reachability）。
  `tools/check.ps1` **全绿**：**15 步**（8 项前端/契约不变量 + "命令共享一个数据库句柄" +
  "回填不得推进增量游标" + fmt + clippy -D warnings + build + test + PE=2）。

## 提交记录
共 **22 个提交**，从基线到最新（`git log --oneline` 看全量；下表只列里程碑）：

| SHA | 内容 |
|---|---|
| 最新 | M1a-18 第 (5) 件现状盘点（无代码改动） / M1a-17 `user.db` 拒绝打开更新版本 |
| — | M1a-16 服务器草稿镜像 / M1a-15 `sent_index` + 查看副本 / M1a-14 配额账本（user.db v2） |
| — | M1a-13 附件 sha256（路径不是身份；发送前补算与校验） |
| — | M1a-12 同步窗口选择器 + 冷启动自动回填一页 |
| — | M1a-11 已发送副本：先搜索再 APPEND（spool 即"未归档"标记） |
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

### M1a-18 第 (5) 件（认证轴/会话串/通知托盘/安装包）现状盘点 ✅（第 24 轮）

**这一轮没有写功能代码**，先把"第 (5) 件到底还剩什么"逐条对着源码查清楚——它是四件不同的事，
一部分已经在了、一部分被**外部条件卡住**（M0-8 需要你的 client_id），盲目开工很容易重做已有的东西，
或者撞上那个已知阻塞。下表每一条都在代码里核对过（不是回忆）：

| 子项 | 现状（已核对） | 还差什么 |
|---|---|---|
| **认证轴** | 数据轴已经在：`Provider.auth: AuthKind { Password, AppPassword, OAuth2 }`；判定与文案也在（`AuthHint::OAuth2Required` 明说"当前版本尚未支持 OAuth2，请改用应用专用密码"）。**没有任何 OAuth 代码**（无 bearer/refresh/端点调用）。 | ① OAuth 客户端本身（PKCE 授权码 + 刷新）；② 它能不能不带 `client_secret` 换令牌——**这正是 M0-8，需要你的 client_id 与网络**，结论只有两种，命令已在本文档 M0-8 一节。**在这一条有答案之前做 OAuth 是白做。** |
| **会话串** | 读侧原料齐了：`MessageDetail` 带 `message_id` 与 `references`；写侧从 M0 起就整链回填 `In-Reply-To`/`References`。**没有任何会话分组**（`fn thread*` 不存在）。 | 一个纯函数：由 `References`/`Message-ID` 算出稳定的会话键（取根 id），列表按它会话化，展开/折叠与未读聚合。**不碰网络、可以纯测**——这是第 (5) 件里最适合先做的一块。 |
| **通知托盘** | **基本在了**：托盘图标 + 菜单（打开/立即检查/退出）、`mail:new` 事件，且 `tauri_plugin_notification` 真的会发一条新邮件通知。 | 点击通知跳到那封邮件；按账号/文件夹的开关；富通知（双按钮、通知内动作）依赖 Windows App SDK 运行时——**M0-6，本机没装，已记录为外部阻塞**。 |
| **安装包** | NSIS 目标与图标已在（`bundle.active` + `targets: ["nsis"]` + `icons/icon.ico`），PE 子系统=2 由门禁钉住。 | 代码签名（需要证书，外部条件）；**自动更新完全没有**；以及 `design/17` §5 的 ≤20 MB 体积核对。 |

**结论与建议顺序**：第 (5) 件里**没有外部依赖、且能独立完成并验证的是"会话串"**（纯函数 + 列表会话化）；
"认证轴"的实质部分卡在 M0-8（等你的 client_id）；"通知托盘"只差点击跳转与开关；"安装包"差签名（外部）
与自动更新（可做但大）。下一次继续时**建议从"会话串"入手**，而不是认证轴——后者现在做，只能把已经存在的
文案再接一遍。

（说明：本轮上下文已到上限，做不了"会话串"这种跨 mail-core + 界面的完整改造，所以选择把这个盘点做实：
它避免下一轮重做已有部分或撞上已知阻塞。**没有代码改动，也没有假装完成什么。**）

---

### M1a-17 `user.db` 的"更新版本拒绝打开"守卫 ✅（第 23 轮）

M1a-16 收尾时顺带发现的安全缺口，这一轮补上。`cache.db` 一直有这道守卫
（`a_cache_from_a_newer_schema_is_refused_rather_than_misread`），`user.db` **没有**：它的
`migrate` 无条件跑 `create table if not exists`，于是一个更老的程序会把将来版本写的 user.db
当成自己的格式打开。

风险不在 schema——`if not exists` 不会动已有的表——而在**已有内容的含义**：某个列换了形状、
某个状态名现在指别的东西、某份草稿被这个版本按旧格式重写。**拒绝的代价是一条"请升级程序"，
误读的代价是用户的草稿。** 现在 `migrate` 先读 `user_version`，比自己新就返回
`用户数据库版本 X 比程序支持的 Y 更新，请升级程序后再打开`。

测试 `a_user_database_from_a_newer_version_is_refused_rather_than_misread`：正常开一个文件 →
把 `user_version` 改成 `SCHEMA_VERSION + 1` → 断言 `open` **失败**且消息里有"更新"。

**回退路径现在也说得出口**：打开失败时程序仍要能起窗（既有决定，没改成硬失败），但那条 `log_line`
明说"**本次运行不读写草稿/发件箱**"，并且**窗口自己会说**——`SyncStatus` 新增 `user_store_error`，
前端启动时读一次并红字提示。这一步非做不可：回退是唯一一处"不读用户自己的数据"的地方，
窗口显示空草稿箱会被读成"我的草稿没了"，而那正是相反的事实（草稿在磁盘上、只是这个版本拒绝误读）。

**数字**：`check.ps1` 15 步全绿；测试 **223**（181 lib + 38 loopback + 4 reachability）。

---

### M1a-16 服务器草稿镜像 `draft_mirror` ✅（第 22 轮）

`design/07` §3/§5.1：草稿**双保险**——本地 `user.db` 是真身，服务器 Drafts 是"在另一台设备上也能看到"
的第二份。两条规则随之而来，都写进了实现：
- 镜像**不带 `\Seen`**：草稿是没读过的；
- **先写新的，再删旧的**。反过来（先删后写）一旦写入失败，用户就没草稿了。

**存储**（`user.db` 新表 `draft_mirrors`）：记 `draft_id / account_id / folder / uid / message_id`。
必须有这张表，否则每次自动保存都会在用户草稿箱里多留一份。**`account_id` 也存**：光有文件夹名
选不出凭据，而且这条记录会比它描述的草稿活得久。
> 附带一个实测发现：`user.db` 的 `migrate` **没有** cache.db 那种"版本相等就早退"的守卫，
> 所以加表**不需要**升版本号；真机上 `draft_mirrors` 在打开时直接出现（下面有证据）。

**`mirror_draft`**（mail-core）：按角色取 Drafts 文件夹（新增 `drafts_mailbox()`，与 `sent_mailbox()`
同一套逻辑）→ 用 `draft_to_mail` 把草稿变成可构建的消息 → APPEND（不带 `\Seen`）→
`find_uid_by_message_id` 找回 UID（APPEND 不回 UID，A1-F43）→ 最后才删上一份。
- **没有 Drafts 文件夹 ⇒ 返回 `None`**，不是失败：本地草稿照样存好了，那才是要守住的承诺。
- 删旧副本**尽力而为**：新副本已经在服务器上了，旧副本删不掉不该让整次镜像算失败。
- `draft_to_mail` 是纯函数：收件人是人打的字符串（**半角/全角逗号与分号都要拆**），附件是
  JSON 数组；附件 JSON 读不出来 ⇒ **报错**而不是"悄悄镜像一封没有附件的草稿"。

**命令**：`mirror_draft`（尽力而为，失败让界面说"云端草稿待同步"）、`draft_mirror`（查链接）。
**`delete_draft` 现在会连服务器副本一起删**：一个叫"丢弃"的按钮不该在用户邮箱里留下谁也清不掉的一份
（`design/07` §6：丢弃是永久动作）。删副本走 `pool_op` 并**忽略所有错误**——本地行才是用户要删的东西，
网络失败不能把"已丢弃"变成"没丢弃"。

**界面**：写信窗口关闭时（本地保存**之后**）best-effort 镜像一次；失败显示设计要求的
`本地已保存（云端草稿待同步）`，关窗后改成 toast——只在对话框里显示的话，恰恰是用户需要它的时候它没了。

**证据**：
- loopback 两条（假服务器新增一个 `drafts` 开关，默认关，免得别的文件夹断言被悄悄改变）：
  镜像写到 Drafts 且 **APPEND 行里没有 `\SEEN`**；第二次镜像**旧的 UID 被 `UID STORE 4242` 标删**；
  没有 Drafts 文件夹的账号 ⇒ `None` 而不是错误。
- `sentcopy` 两条单元：草稿 → 消息的转换（**全角逗号/分号**、附件、references、无哈希的老草稿）；
  附件 JSON 坏了 ⇒ 报错。
- **真机**：应用起来后 `user.db` 里出现了 `draft_mirrors`（版本仍是 2，未升号），其余行数不变。

**数字**：`check.ps1` 15 步全绿；测试 **222**（180 lib + 38 loopback + 4 reachability）；
前端命令 **28** 个。

**已知缺口（下一轮先做这三件）**：
1. **发出后就没人清那份服务器草稿了**：worker 在发送成功后删的是本地草稿行（mail-core 层，没有 IMAP
   连接），`draft_mirrors` 那行和服务器副本会留下。修法很直接：镜像命令在"草稿已经不存在"时
   删掉旧副本并清行——`account_id` 已经在表里，正是为这种清扫准备的。
2. 只有**关闭时**镜像，没有 §5.1 的"每 30 秒或失焦"。
3. `user.db` 没有 cache.db 那样的"更新版本要拒绝打开"守卫（本次顺带发现），老程序打开将来写的
   user.db 会被当成自己的格式读。

---

### M1a-15 `sent_index`：发件箱 → 已发送副本的可追溯链接 ✅（第 21 轮）

`design/07` §5.4/§7。副本此前只是 `ensure_sent_copy` 的**返回值**，落不到任何地方——发出去的信
在"已发送"里找得到，但界面无从知道它在哪，用户只能自己翻文件夹。

- **`cache.db` v2 新增 `sent_index(account_id, message_id, folder, uid, local_at)`**，主键
  `(account_id, message_id)`：记录同一个 `Message-ID` 两次只会**替换**（归档步本身就拒绝写第二份副本，
  所以第二条记录只可能是同一份副本又找到了一次；两行会让链接有歧义）。
- **`uid` 允许为空**：APPEND 不回 UID 是既定事实（A1-F43），所以"归档了但说不出编号"必须能表示
  ——否则界面会去查一封明明已经存在的信。空 ≠ 没有副本。
- **记录写在 `file_sent_copies` 里**（不是命令层）：这一趟已经在 `user.db` 里删标记、删 spool，
  顺手把链接写进缓存是同一件事的一部分，也因此能被 worker 测试覆盖。**尽力而为**：这是派生数据，
  写不进去不能连累一封已经发出去的信（下一趟会再记一次）。
- **放在 cache.db 的理由**：删缓存丢的是"链接"，不是"副本"；而"要归档"的意图由 spool 的存在承载，
  在 `user.db` 里（M1a-11 的结论）。两者分工不混。

**界面**：发件箱里 `sent` 状态的行多一个**「查看副本」**——按冻结的 `Message-ID` 查索引 → 切到
那个文件夹 → 按 UID 打开阅读器。三种诚实降级：没有登记（可能清了缓存）⇒ 提示去「已发送」按主题找；
服务器没给 UID ⇒ 打开文件夹并说明；文件夹已不存在 ⇒ 明确报错。**不会给一个点了没反应的按钮。**

**证据**：
- cache 三条：记录/读回/按账号隔离/重复记录不产生两行；`uid = None` 仍算"有副本"；
  **v1 老库打开即获得新表**（与 user.db 同一套模式：`create table if not exists` 就是迁移，
  测试挡住"改成版本检查从而拒绝打开老库"这种会丢光用户缓存的做法）。
- worker 一条：归档成功后**索引里确实有那条链接**（folder/uid/local_at 都对）。
- **真机双库迁移**（这台机器上真实文件）：`tools/inspect-local-db.py` 前后对比 ⇒
  `cache.db` **v1→v2**、多了 `sent_index`、**`messages` 仍是 158 行**（用户缓存的邮件一封没动）；
  `user.db` 保持 v2 不变。
- 真机探针：`sent_copy` 对不存在的 `Message-ID` 返回**诚实的 `null`**（而不是报错），
  发件箱探针全链仍通过。

**数字**：`check.ps1` 15 步全绿；测试 **218**（178 lib + 36 loopback + 4 reachability）；
前端命令 **26** 个。

**未做**：一条真实的"发信 → 点查看副本"真机走查（需要真的发一封信，得先问你）；
副本 UID 在下次同步后的对账（`design/07` §5.4 提到的"找不回 UID 就下次同步对账"）。

---

### M1a-14 配额账本：到额度就停，而不是等服务器拒绝 ✅（第 20 轮）

`design/04` §5.7。硬事实：Gmail 的 IMAP 下载上限约 **2.5 GB/天**，超了会被限到近乎不可用、24 小时后
才重置。**这不是能优化的工程问题，是配额**——所以必须自己停，而不是等被拒绝（被拒绝时账号已经被罚了）。

**账本放在 `user.db`**（新增 `quota_ledger` 表，`USER_SCHEMA_VERSION` 1→2）：设计特意强调过，
放可删的 `cache.db` 里意味着"删缓存 = 忘掉今天已经下载了多少"，下一次立刻又超。
- `QuotaLedger { bytes_today, daily_budget, window_start, throttled_until, server_floor }`；
  `allows_download(now)` 把"今天用完了"和"服务器让我们退避到某时刻"两件事合起来回答。
- **窗口边界用滚动 24 小时**（从窗口内第一次请求算起）而不是日历日：严格更保守（任何窗口都不会
  放出超过预算的量），而且不需要时区数据库。代价是重置时刻会随当天首次使用时间漂移，
  界面文案"明天自动继续"如实覆盖。这条偏离写在这里，不藏在代码里。
- `daily_budget` 与 `throttled_until`/`server_floor` 的寿命不同：换窗口只清字节数，
  **服务器教给我们的事实（被退避到什么时候、能取到多早）继续保留**。

**预算从服务商表来**：新增 `Provider::daily_download_budget()`——**只有 Gmail 有数字**
（2.3 GB，留 8% 余量），其余全是 `None`。有意如此：**编一个上限会无缘无故停掉别人的邮件**，
而"未知"就该原样表示未知。测试断言 Gmail 的数落在文档上限内、且**其他每一家都必须是 `None`**。

**执行点**：`BackfillBudget` 增加 `max_bytes`，`BackfillStop` 增加 `QuotaSpent`，
`BackfillReport` 增加 `bytes`（本次落库邮件大小之和）。
- 走完一页（**先落库**）后先看字节数：到额度就返回 `QuotaSpent`——排在窗口判定之前，
  因为"窗口已满足"不是继续下载的理由。
- 命令 `backfill_to_window` 开跑前问账本：不允许下载就直接返回 `QuotaSpent`（**一个字节不取**），
  否则把剩余额度当作 `max_bytes`；跑完按**实际落库的字节**记账（取回但没落库的不收费）。
- 前端：`stop === 'quota_spent'` 时按设计的口径提示（带上服务商名），而不是当成失败静默。

**证据**：
- loopback 两条：给 1 字节预算、`max_pages: 40` ⇒ 停在**第 1 页**且 `QuotaSpent`，
  并断言 transcript 里**那一页之后只有 1 次 `UID FETCH`**（"不会在预算耗尽后继续发请求"——
  这正是设计要求的门禁）；另一条断言**花掉预算的那一页仍然落库**（否则下次会把同一页再取一遍）。
- userdb 四条：累加与 24 小时滚动（读不重置）、无上限的服务商永远不被我们拦、
  **重启与删缓存都不重置账本**、**v1 的老库在打开时自动获得新表**。
- **真机迁移验证**（这台机器上真实的 `user.db`，v1、有 2 条历史行）：
  `tools/inspect-user-db.py` 前后对比 ⇒ `user_version` 1→2、表里多了 `quota_ledger`、
  **原有行数不变**（outbox 2，迁移是无损的增量）。
- 顺手清掉了我自己留下的两个探针行（发件箱里曾是两条"已撤销"的假邮件），
  新工具 `tools/probe-cleanup.mjs` 只删"非排队中且主题带探针标记"的行；
  `probe-outbox.mjs` 现在也会清掉它自己那条撤销行，跑完 cleanup 复查 = 0 条。

**数字**：`check.ps1` 15 步全绿；测试 **215**（175 lib + 36 loopback + 4 reachability）；
真机 user.db 现在 `user_version = 2`、`outbox = 0`（探测残留已清）。

**未做**：网易系 `server_floor` 的提示文案（表列已就位、写入接口已就位，但还没有哪条路径在
"再也取不到更早"时写它并提示用户）、加账号向导里的体量预检、配额进诊断包。

---

### M1a-13 附件 sha256：路径不是身份 ✅（第 19 轮）

`design/07` §5.5/§7。草稿存的是**路径**（100 MB 附件不能把 `user.db` 变成第二份邮件），
而路径**不是身份**：文件可以在"选中"与"发送"之间被改、被改名、被删。发出去的是改过的新内容，
用户在写信界面看不出来（名字和大小还是旧的）。所以每个附件记一份选中时的 SHA-256，发送前比对。

**新增 `attach.rs`**（9 条测试）：
- `file_sha256()`：**分块**（256 KB，`design/07` §5.5 的数字）流式读，100 MB 附件不会为了算哈希
  被读进内存。测试用空文件的公开定型值钉住算法与十六进制编码，另有一条"跨块文件与整读结果一致"
  ——哈希循环漏读/重读一块仍然会算出**一个**哈希，只是不是对的那个。
- `check_attachment()`：内容变了 ⇒ `Changed`；文件读不到 ⇒ `Missing`；**没有记录哈希 ⇒ 什么都不说**
  （老草稿不该因为"我们没记过"被拒绝）。
- `prepare_attachments()`：**发送前补算**——没哈希的补上（并写回 payload，下次才能比对），
  比不上的返回可行动的中文错误（**点名是哪个文件**）。两个 reason key（`attachment_changed`/
  `attachment_missing`）有测试断言它们确实落在 `needs_user_decision` 里。

**接线**：
- 选附件时（`pick_attachments`）立刻算哈希——那是用户**刚刚看着**选择的那一版，也是唯一能记录它的时刻；
  算不出来就留空，发送时补算，而不是拒绝这个附件。
- 入队时（`enqueue_send`）先 `prepare_attachments`：这是"文件不是你选的那个"还能作为**问题**存在的
  最后一刻，之后字节就被冻结了。
- **重建 spool 时**（`worker::frozen_bytes`）再校验一次：冻结字节丢了，磁盘上的文件是唯一来源，
  这是最后一次能发现改动的地方 ⇒ 停下来问用户，绝不"照现在的样子重建"
  （新增 worker 测试：改动过的附件 ⇒ `needs_attention`、`sent=0`、**SMTP 一次都没碰**）。

**契约门禁补上了嵌套结构**：`tools/payload_keys.mjs` 之前只比对顶层 `OutgoingMail`/`Draft`，
`OutgoingAttachment` 是**嵌套**结构、同样带 `deny_unknown_fields`，错了也是运行期才炸
（正是 M1a-7 那类 bug 的藏身处）。现在 `payload.js` 有 `attachment()` 构造器，门禁数到
**5 个字段**；`probe-drafts.mjs` 的负载检查也带上了嵌套附件，真机走一遍确认能反序列化。

**依赖**：新增 `sha2 = "0.10"`（`default-features = false`）。它本来就在 `Cargo.lock` 里
（传递依赖），源码与 `.crate` 都已在本地 registry 缓存，构建全程 `--offline` 通过。

**数字**：`check.ps1` 15 步全绿；测试 **208**（170 lib + 34 loopback + 4 reachability）；
真机两个探针（草稿 / 发件箱）重跑均通过。

**未做**：附件大小预检（`design/07` §6 的 `ValidationIssue::TooLarge`，需要服务商表里的上限字段）、
spool 的 256 KB 分块流式写（现在仍是一次性 `formatted()`，峰值内存问题见 §8）、
以及 PROGRESS 里列的其余项。

---

### M1a-12 同步窗口选择器 + 冷启动自动回填一页 ✅（第 18 轮）

交接摘要的第 3 优先级。窗口此前是"存得住、读得出、也真的改变同步行为"（M1a-3/3b），但**界面上没有
入口**，冷启动也不会自动往回取——这一轮把这两件事补上，目标点 3 到此完整。

**1. 选择器**（侧栏「本地 / 同步窗口」一行显示当前值，点开对话）：
- 四个选项与后端能存的四种窗口一一对应：最近 N 年 / 每个文件夹最近 N 封 / 全部 / 只按需。
- 文案对"全部"是诚实的：它写到"一直往回取，直到服务器没有更早的"，因为**配额账本还没做**
  （`design/04` §5.7），现在没有任何东西给它节流。
- 提示写明"收窄窗口不会删除已经缓存的邮件"——一个会悄悄删信的设置是个陷阱（这是 M1a-3 就定下的语义）。

**2. 冷启动回填一页**（`design/04` §5.7 的"冷启动自动回填一页"）：
- 首个文件夹的第一页上屏**之后**才触发，每个账号每个会话一次（`backfilledAccounts`）；
  只有在真的取回邮件时才提示，失败**不弹错**——用户是来读信的，不是来看一个后台补信失败的红条
  （离线状态另有既有的提示）。
- 命令加了 `pages: Option<u32>`：`None` ⇒ **一页**（`BackfillBudget::default()`），
  `Some(n)` ⇒ 显式多走 n 页。**这个上界才是重点**：窗口可能是"全部邮件"，
  一个开窗就开始下载几千封信的客户端正是这个上界要防的东西（原来这里写死 `generous()`＝40 页）。

**真机证据**（新工具 `tools/probe-sync-window.mjs`，CDP 驱动真实窗口，**跑完把用户设置原样放回**）：
```
initial    : {"kind":"years","value":2} ⇒ 侧栏显示"最近 2 年"
cold_start : backfilledAccounts 里已有 me@example.com（冷启动确实跑了）
round_trip : years:3 / count:1234 / all / on_demand 四种**逐字往返成功**
             （serde 的 tag+content 对 unit 变体是运行期陷阱，这里四种都过了一遍）
picker     : 打开时预选当前类型；选"最近 5 年"保存后 get 回 {"kind":"years","value":5}，
             侧栏同步变成"最近 5 年"，弹窗关闭
restored   : 用户原来的 years:2 已恢复
```
启动日志另有一条真机网络证据：
`[easy-email] 回填到窗口边界 years:2：0 页 / 0 封（HistoryExhausted）` —— 命令真的走通了
IMAP → 缓存 → 报告这条链，停在"历史到底"这个正常出口（这个 profile 的缓存已到邮箱底部）。

**数字**：`check.ps1` 15 步全绿；测试 **198**（160 lib + 34 loopback + 4 reachability，
这一轮没有新增 Rust 测试——后端本来就齐了，新增的是界面与上界）；
前端命令 **25** 个（终于用上了 `get_sync_window`/`set_sync_window`/`backfill_to_window`）。

**未做**：配额账本（`design/04` §5.7，长走一回的节流）、`Some(n)` 多页走的界面入口（目前只有
`None` 一页在用）、以及 M1a-11/PROGRESS 里列的其余项。

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

**当前状态**：`tools/check.ps1` **15 步全绿**；`cargo test --workspace` **223 通过、0 失败**
（181 lib + 38 loopback + 4 reachability）；`python design/tools/audit.py` **0 失败**；
应用启动冒烟通过（10 秒存活）。**用户已确认**：真机发出去的信收到了（他自己试的）——
这是发件箱全链（入队 → worker → SMTP → 已发送副本）在真实账号上的第一次人证。
真机两个库都已随本轮升级：`user.db` **v2**（`quota_ledger`）、`cache.db` **v2**（`sent_index`），
迁移前后行数不变（缓存里 158 封邮件一封没动）。
所有改动都在 git 里（基线 `83c1b0c`，共 51 个提交）。

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
| **M1a-12** | **同步窗口选择器（四种窗口 + 收窄不删信）+ 冷启动自动回填一页（`pages: None` 一页，防"全部"窗口开窗就下载几千封）** | **`tools/probe-sync-window.mjs` 真机：四种窗口逐字往返 / 选择器预选与保存 / 侧栏同步 / 设置已还原；启动日志有真机 `回填到窗口边界 years:2：0 页 / 0 封（HistoryExhausted）`** |
| **M1a-13** | **附件 sha256：选中时记录、发送前补算与校验、重建 spool 时再校验；契约门禁补上嵌套的 `OutgoingAttachment`** | **`attach.rs` 9 条（含空文件定型值、跨块一致、变了/丢了/没记录三种判定）；worker 测试：改动过的附件 ⇒ `needs_attention`、SMTP 一次没碰；真机探针带嵌套附件负载通过** |
| **M1a-14** | **配额账本：`user.db` 新增 `quota_ledger`（v2）、滚动 24 小时窗口、按服务商给预算（只有 Gmail 有数）；`BackfillBudget.max_bytes` + `BackfillStop::QuotaSpent`；命令开跑前查、跑完按实际落库字节记账** | **loopback：1 字节预算 ⇒ 停在第一页且此后只有 1 次 `UID FETCH`（不会继续发请求），且那一页仍落库；真机 v1→v2 迁移无损（表增、行数不变）** |
| **M1a-15** | **`sent_index`（cache.db v2）：归档时记录副本的 folder/UID（UID 可空）、发件箱「查看副本」按钮、三种诚实降级** | **cache 三条（含 v1 老库打开即获新表）+ worker 断言索引里确有链接；真机 cache.db v1→v2、`messages` 仍 158 行；探针：查不到的 `Message-ID` 返回诚实的 `null`** |
| **M1a-16** | **服务器草稿镜像：`draft_mirrors`（user.db）、`mirror_draft`（不带 `\Seen`、**先写新的再删旧的**、无 Drafts 文件夹 ⇒ `None`）、`draft_to_mail`、丢弃草稿时一并删服务器副本、关窗后 best-effort 镜像** | **loopback：APPEND 到 Drafts 且没有 `\SEEN`、第二次镜像用 `UID STORE 4242` 标删旧副本、无文件夹 ⇒ `None`；真机 `user.db` 出现 `draft_mirrors` 且其余行数不变** |
| **M1a-17** | **`user.db` 的"更新版本拒绝打开"守卫**（与 cache.db 对齐）：拒绝的代价是"请升级程序"，误读的代价是用户的草稿 | **测试把 `user_version` 改成 `SCHEMA_VERSION + 1` ⇒ `open` 失败且消息可行动；回退日志明说本次运行不读写草稿/发件箱** |

**被外部条件卡住的两项（不是没做，是做不了；步骤已写清）**
1. **M0-6 WinAppSDK 通知**：本机没装 Windows App SDK 运行时，`src-tauri` 也没有 `windows` crate 依赖。
   恢复：`winget install Microsoft.WindowsAppRuntime.1.7` → 加 `windows` crate 的 AppNotifications
   features → Register + 双按钮 + `NotificationInvoked` → 量框架依赖/自包含的体积。
2. **M0-8 OAuth 端点探测**：需要你的 client_id 与网络。命令已写在本文档 M0-8 一节，
   结论只有两种：能不带 `client_secret` 换到令牌（PKCE 够）或不能（BYO 向导要加一个输入框）。

**下一步优先级**（每一件都自包含，做完各让 `check.ps1` 与测试保持全绿）
1. **镜像的收尾**（M1a-16）：发出后 worker 删的是本地草稿行，`draft_mirrors` 那行与服务器副本会留下
   ——镜像命令在"草稿已经不存在"时删旧副本并清行即可（`account_id` 已在表里正为此）；
   再有 §5.1 的"每 30 秒或失焦"镜像。
   （守卫与"显示给用户"都已在 M1a-17 落地。）
2. **发信上限预检**（`design/07` §6）：服务商表还没有 `smtp_max_message_bytes` / `append_limit_bytes`，
   所以"这封信太大了"只能等服务器拒绝（552 现在会落到 `needs_attention`，但那时已经晚一步）。
   附件哈希（M1a-13）让"算准要发多大"变得可做。
3. **网易系 `server_floor` 的提示**：表列与写入接口都就位了，但还没有哪条路径在"再也取不到更早"时
   写它并按设计给一次可操作提示（"去网页版把收取范围改大"）——**用户会以为是我们的 bug**。
4. **spool 分块流式写**（`design/07` §8）：现在建 spool 仍是一次性 `formatted()`，25 MB 附件的
   峰值内存还没按 §8 的预算收紧。
5. **真机验证**（需要先问你）：一条真实的"发信 → 点查看副本"走查；崩溃恢复时序（预置 spool 缺失的
   `sending` 行 ⇒ 期望 `needs_attention`，全程不碰 SMTP，安全）；"附件被改动"的提醒是否看得懂；
   真机上把写信窗口写点东西关掉 ⇒ 服务器草稿箱出现一份副本（这一步会真的往你邮箱里写东西）。
6. **第 (5) 件的顺序已查明**（M1a-18 的盘点）：**先做"会话串"**（纯函数 + 列表会话化，无外部依赖，
   可纯测）→ 通知托盘的点击跳转与开关 → 认证轴的 OAuth（**卡在 M0-8：需要你的 client_id**，
   在那之前做是白做）→ 安装包（签名是外部条件；自动更新完全没有，可做但大）。

**运行方式**
```
.\tools\check.ps1                     # 15 步门禁（前端不变量 / 负载契约 / fmt / clippy / build / test / PE=2）
cargo test --workspace                # 223 个测试
python design\tools\audit.py          # 设计文档一致性（对应 design/ 的 23 份文档）
python tools\inspect-local-db.py <标签> [user|cache]   # 真机库的版本/表/行数（迁移前后对比用）
git log --oneline                     # 51 个提交；git diff 83c1b0c HEAD 是本次全部改动

# 真机端到端（要临时把 --remote-debugging-port=9222 加回 tauri.conf.json 再 cargo build）
node tools/inspect-dom.mjs @tools/probe-drafts.mjs       # 草稿：写→自动保存→关闭保存→草稿箱→丢弃
node tools/inspect-dom.mjs @tools/probe-outbox.mjs       # 发件箱：入队→撤销→删除→写信点发送真的入队
node tools/inspect-dom.mjs @tools/probe-sync-window.mjs  # 同步窗口：四种窗口往返 + 冷启动回填（会还原设置）
node tools/inspect-dom.mjs @tools/probe-cleanup.mjs      # 清掉探针在真机上留下的历史行
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
