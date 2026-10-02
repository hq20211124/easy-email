# 10 规则、垃圾邮件与自动化

> **目标**：让"邮件自动归位"成为可能，而不是让用户每天手动拖 50 封。
> **前提纪律**：规则是**本地执行**的（红线 6：不做服务端 Sieve UI），所有动作都走本地队列（`pending_ops`），
> 因此**离线也生效**，联网后收敛。
>
> 覆盖 ID：RULE-01…07、SRCH-05（保存搜索与虚拟文件夹的数据基础）。
> 上游：03（rules/blocklist/allowlist 表）、04（同步后触发）、09（保存搜索）。下游：13（通知规则）。

---

## 1. 目标与非目标

**目标**
1. **可视化规则编辑器**：条件 + 动作 + 排序 + 启停 + **试运行**（先看会命中哪些邮件，再决定启用）。
2. **覆盖常见条件**：账号/文件夹/发件人/收件人/主题/正文/附件名/有无附件/大小/日期/是否已读/是否含标签。
3. **动作安全**：移动/复制/删除/归档/标已读/加旗标/加标签/转发/通知/不下载附件 —— **破坏性动作可撤销**（记 undo token）。
4. **垃圾邮件**：以服务器判定为准（`\Junk`/`$Junk` 文件夹），本地只做**白/黑名单**与"不是垃圾"的一键纠正。
5. **退订**：识别 `List-Unsubscribe`（含 `List-Unsubscribe-Post`）并一键执行；**执行前告诉用户会发生什么**。
6. **自动归档**：按时间/已读状态批量归档（"30 天前的已读邮件"），干跑 + 可回滚。
7. **通知规则**：按账号/文件夹/发件人静音（RULE-07，与 13 共用同一份策略）。

**非目标**
- 不做贝叶斯自学习垃圾过滤（收益低、误判成本高、需要训练界面）。
- 不做"运行外部程序"这类动作（安全与可移植性都不允许）。
- 不做服务端过滤器同步（Sieve/ManageSieve，已明确否决）。

## 2. 用户可感知行为

| 场景 | 用户看到 |
|---|---|
| 新建规则 | 一行行拼条件："当 **发件人包含** `@github.com` **且** 主题包含 `[easy-email]` → **移动到** `项目/GitHub`，**并** 标为已读" |
| 点"试运行" | "这个规则会命中最近 200 封中的 **43 封**"，并列出前 10 封 + "查看全部" |
| 启用后 | 新邮件到达时自动归位；规则列表显示"今日已应用 12 次" |
| 想撤销一次自动动作 | 通知/列表行上有"撤销"（10 秒内），或设置里"最近自动动作"可回滚一批 |
| 收到垃圾邮件 | 邮件在"垃圾邮件"文件夹（服务器判定）；右键"这不是垃圾" → 移回收件箱 + 加白名单 |
| 想拉黑某人 | 右键"屏蔽此发件人" → 以后直接进垃圾邮件（可改成直接删除，需确认） |
| 想退订 | 邮件顶部有"退订"按钮（识别到 `List-Unsubscribe`）→ 点击后说明"将向对方发送退订请求 / 打开网页" |
| 收件箱太满 | 设置 → 自动化 → "自动归档 30 天前的已读邮件" → 干跑："将移动 412 封到归档，预计释放列表长度" → 确认 |

## 3. 数据结构

03 已有 `rules`/`blocklist`/`allowlist`；本模块补：

```sql
-- 规则的执行历史（撤销与审计）
create table if not exists rule_runs (
    id integer primary key autoincrement,
    rule_id text not null, account_id text not null,
    applied_at integer not null, matched integer not null,
    actions_json text not null,          -- 实际执行的动作摘要
    undo_json text,                      -- 反向操作所需信息（移动回哪、旗标原值…）
    undone_at integer
);
create index if not exists rule_runs_recent on rule_runs(applied_at desc);

-- 退订记录（避免重复打扰）
create table if not exists unsubscribe_log (
    id integer primary key autoincrement,
    account_id text not null, sender text not null,
    kind text not null,                  -- http | mailto | one_click
    done_at integer not null, result text
);
```

```rust
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Rule {
    pub id: String, pub name: String, pub enabled: bool, pub sort_order: i32,
    pub conditions: RuleConditions, pub actions: Vec<RuleAction>,
    pub stop_processing: bool,           // 命中后不再执行后续规则
    pub apply_to: ApplyScope,            // NewMailOnly（默认）| AllExisting | Folder(String)
    pub created_at: i64, pub updated_at: i64, pub last_run_at: Option<i64>,
}
/** 规则条件。**故意不实现 `Default`**（第二轮评审）：
 *  `derive(Default)` 会把 `match_all` 变成 false（OR），与"默认 AND"的语义打架，
 *  而空条件集在 AND 下恒真 ⇒ 半成品规则会命中全部邮件。用 `new()` 让默认值只有一个来源。*/
pub struct RuleConditions { /* …字段同下… */ }
impl RuleConditions {
    pub fn new() -> Self { /* match_all: true，其余 None/空 */ }
    /// 是否有"正向条件"（至少一个能独立成立的匹配条件）。
    /// `evaluate()` 的第一条规则：**没有正向条件 ⇒ 永不匹配**（见 §11 反驳 1）。
    pub fn has_positive_condition(&self) -> bool;
}

    pub account_ids: Vec<String>,
    pub folders: Vec<String>,
    pub from: Vec<String>,               // 子串或 @domain 或完整地址（统一 contains 语义）
    pub to: Vec<String>, pub cc: Vec<String>,
    pub subject_contains: Vec<String>, pub subject_not_contains: Vec<String>,
    pub body_contains: Vec<String>,
    pub header_exists: Vec<String>,      // 例如 List-Unsubscribe
    pub has_attachment: Option<bool>,
    pub attachment_name_contains: Vec<String>,
    pub size_over: Option<u64>, pub size_under: Option<u64>,
    pub date_after: Option<i64>, pub date_before: Option<i64>,
    pub is_unread: Option<bool>, pub is_flagged: Option<bool>,
    pub has_label: Vec<String>, pub has_attachment_type: Vec<String>,  // 例如 application/pdf
    pub match_all: bool,                 // true=AND（默认）；false=OR
}
#[derive(Serialize, Deserialize, Clone, Debug)]
pub enum RuleAction {
    MoveTo(String), CopyTo(String), Archive, **MoveToTrash**, MarkSeen(bool), Flag(bool),
    // **第二轮评审修正**：原来这里是 `Delete`（永久删除）。IMAP 一旦 EXPUNGE 就**无法撤销**，
    // 而 §1/§6 承诺"破坏性动作可撤销" ⇒ 规则动作里只能有"移到垃圾箱"（可恢复）。
    // 永久删除是**单独的、非规则的动作**，需要显式确认（见 §6 与 §12 F1）。
    AddLabel(String), RemoveLabel(String), Forward(String), Notify(NotifyHint), SkipAttachments,
}
#[derive(Serialize, Clone, Copy, Debug)] pub enum ApplyScope { NewMailOnly, AllExisting, Folder }
```

## 4. 接口（函数级）

```rust
// ── 规则 CRUD 与测试 ─────────────────────────────────────────
pub async fn rules_list(db: &Db) -> Result<Vec<Rule>>;
pub async fn rule_save(db: &Db, r: &Rule) -> Result<()>;
pub async fn rule_delete(db: &Db, id: &str) -> Result<()>;
pub async fn rule_reorder(db: &Db, ids: &[String]) -> Result<()>;
/// 试运行：只读，不改任何东西（**必须是默认入口**）
pub async fn rule_dry_run(db: &Db, r: &Rule, limit: u32) -> Result<DryRunReport>;
pub struct DryRunReport { pub matched: u32, pub scanned: u32, pub samples: Vec<ThreadRow>, pub per_condition: Vec<(String, u32)> }
/// 对历史邮件批量执行（后台任务，可暂停/取消，**可撤销**）
pub async fn rule_apply_existing(ctx: &AppCtx, rule_id: &str, job: &JobCtx) -> Result<RuleApplyReport>;
pub async fn rule_undo_run(ctx: &AppCtx, run_id: i64) -> Result<UndoReport>;

// ── 执行引擎（同步落库后由 04 触发；纯函数 + 一次批量写）──────
pub struct RuleContext<'a> { pub summary: &'a MessageRow, pub body: Option<&'a BodyRow>, pub attachments: &'a [AttachmentRow], pub account: &'a AccountConfig }
/// **空条件集或无正向条件 ⇒ 返回空 Vec（永不匹配）**（§11 反驳 1 的硬规则 1）
pub fn evaluate(rules: &[Rule], ctx: &RuleContext) -> Vec<RuleMatch>;      // 纯函数（可单测）
pub struct RuleMatch { pub rule_id: String, pub actions: Vec<RuleAction> }
pub async fn apply_matches(db: &Db, matches: &[RuleMatch], ctx: &RuleContext) -> Result<RuleRunReport>;
/// 动作翻译成"本地立即生效 + 入队服务器操作"（复用 03 的 pending_ops）
pub fn actions_to_ops(m: &RuleMatch, ctx: &RuleContext) -> Vec<PendingOp>;

// ── 垃圾邮件 / 黑名单 ────────────────────────────────────────
pub fn is_junk_folder(role: Option<Role>) -> bool;
pub async fn mark_not_junk(ctx: &AppCtx, key: &MsgKey) -> Result<()>;      // 移回收件箱 + 加白名单 + 记日志
pub async fn mark_junk(ctx: &AppCtx, key: &MsgKey) -> Result<()>;          // 移到垃圾文件夹
pub async fn blocklist_add(db: &Db, kind: BlockKind, value: &str, action: BlockAction) -> Result<()>;
pub enum BlockAction { MoveJunk, Delete, MarkRead, Archive }

// ── 退订 ─────────────────────────────────────────────────────
pub struct UnsubscribeOffer { pub kind: UnsubKind, pub target: String, pub one_click: bool, pub sender: String }
pub enum UnsubKind { Http, Mailto }
pub fn find_unsubscribe(headers: &str) -> Option<UnsubscribeOffer>;        // 解析 List-Unsubscribe / List-Unsubscribe-Post
pub async fn unsubscribe(ctx: &AppCtx, offer: &UnsubscribeOffer) -> Result<UnsubResult>;

// ── 自动归档 ─────────────────────────────────────────────────
pub struct ArchivePolicy { pub older_than_days: u32, pub only_read: bool, pub keep_flagged: bool, pub folders: Vec<String>, pub target: String }
pub async fn archive_dry_run(db: &Db, p: &ArchivePolicy) -> Result<DryRunReport>;
pub async fn archive_apply(ctx: &AppCtx, p: &ArchivePolicy, job: &JobCtx) -> Result<RuleApplyReport>;
```

## 5. 状态机与主流程

### 5.1 新邮件到达时的规则执行
```
04 落库（一个事务：messages + index_queue）→ 事务提交后：
  ├ 取该账号启用中的规则（按 sort_order）
  ├ evaluate()（纯函数，只读 summary/body/attachments）
  ├ 命中 → actions_to_ops() → 与 03 的 pending_ops 一起写（本地立即生效）
  │        记录 rule_runs（含 undo_json）
  ├ stop_processing → 停止后续规则
  └ 被规则改写的结果（移动/删除）→ 通知（13）据此决定是否弹
要点：**规则不阻塞同步**（一条规则最多 5 ms，200 封一批 1 秒内跑完）；
      规则执行失败（例如目标文件夹不存在）→ 跳过该动作并记日志（不中断其它动作）。
```

### 5.2 试运行 → 启用 → 撤销
```
rule_dry_run（默认入口，只读）→ 用户看到"命中 43 封"与样本 → 启用
首次启用可选"对已有邮件也执行一次"（apply_to=AllExisting）→ 后台任务 + 进度 + 可暂停
每个 run 都写 rule_runs.undo_json → 用户可在"最近自动动作"里回滚（10 分钟内的批量动作）
```

### 5.3 垃圾邮件与退订
```
服务器判定优先：邮件落在 \Junk/$Junk 文件夹 → is_junk=true（只反映，不额外打分）
用户点"这不是垃圾" → mark_not_junk：移到收件箱 + allowlist(purpose=spam) + 记日志
用户点"退订" → find_unsubscribe：
  ├ one_click（List-Unsubscribe-Post=List-Unsubscribe=One-Click）→ POST 一次（RFC 8058）
  ├ http → 打开系统浏览器（**不自动 GET**，避免"点一下就是一次确认"）
  └ mailto → 预填一封退订邮件（不自动发送，用户自己点发送）
记录 unsubscribe_log，避免重复提示
```

## 6. 失败模式与降级

| 触发 | 用户看到 | 处理 |
|---|---|---|
| 规则目标文件夹被删 | 规则旁黄点 + "目标文件夹不存在" | 跳过该动作，其它动作继续；提示修复 |
| 规则命中过多（一次 > 2000 封） | "命中 3,412 封，建议缩小条件" | 试运行阶段拦住（超过阈值要求显式确认） |
| 自动动作与用户手动操作冲突 | 无感 | 都进 pending_ops，按时间顺序执行；破坏性动作带 conflict 检查（03 §5.3） |
| 撤销失败（服务器已变化） | "部分动作无法撤销（服务器上已变化）" | 报告哪些成功、哪些没做 |
| 退订请求被拒 | "对方拒绝了退订请求（可以改为屏蔽发件人）" | 提供"屏蔽"作为替代 |
| `List-Unsubscribe` 指向可疑域名 | 显示目标域名并要求确认 | 不自动请求（防钓鱼确认链接） |
| 规则循环（A 移到 B、B 移回 A） | 检测到"同一封邮件在 5 秒内被移动 ≥ 3 次" | 直接停止并报错，且不写入 undo（避免撤销风暴） |
| 规则把重要邮件挪进垃圾箱 | 撤销（`rule_undo_run` 能把它们移回去） | 规则动作只有"移到垃圾箱"；**永久删除不属规则**，所以不存在"删了撤不回"的承诺 |

## 7. 迁移与兼容

- `rules`/`blocklist`/`allowlist` 随 03 的 v3 迁移建表；v0.2 没有规则功能，无迁移负担。
- 规则 JSON 带 `schema_version`，便于将来加条件而不破坏旧规则（未知字段忽略 + 提示）。
- 导入/导出：设置导出里包含规则（06 §4），导入时可选"合并/替换"。
- 与 `folders.role` 的关系：`Archive` 动作需要知道归档文件夹（`role=archive`；没有就用"全部邮件"或提示用户指定）。

## 8. 性能预算

| 指标 | 预算 |
|---|---|
| 单封规则评估（20 条规则） | ≤ 5 ms |
| 一批 200 封的规则执行 | ≤ 1 s（含写 pending_ops） |
| 试运行（扫 10 万封） | ≤ 20 s（只读索引列，不解码正文；需要正文条件时 ≤ 60 s） |
| 批量执行（10 万封） | ≤ 5 分钟（可暂停，含服务器队列写入） |
| 规则编辑器打开 | ≤ 100 ms |
| 退订网络请求 | ≤ 10 s（超时给明确结果） |

## 9. 测试计划

| 层 | 用例 |
|---|---|
| 单元 | `evaluate()` 真值表（AND/OR、大小写、子串/域名/完整地址、附件类型、日期边界、多规则顺序与 stop_processing）；`find_unsubscribe` 6 种头部样本；`actions_to_ops` 的映射 |
| 集成 | 假服务器：规则命中 → 本地立即生效 + 服务器队列收敛；撤销 run → 状态回到执行前 |
| 边界 | 目标文件夹被删、规则循环、命中过多、删除类动作（强提示）、正文条件与"正文未缓存"（跳过正文条件而不是判为不命中？）→ **设计决定：正文未缓存时该条件视为"未知"，规则不命中并记 skipped（避免误删）** |
| 真机 | GitHub/Jira/银行三类真实邮件各建一条规则；退订一条真实营销邮件（one-click 与 http 各一） |
| 安全 | 退订链接域名校验；规则不能被执行外部程序（枚举约束）；撤销不越权（只能撤销自己产生的动作） |

## 10. 门禁新增项

0. **空条件不匹配（第二轮评审新增，最高优先级）**：真值表测试覆盖"空条件集 / 只有否定条件 / OR+否定 /
   AND 正常"四种；`rule_save` 拒绝"无正向条件 + 破坏性动作"的组合；`RuleConditions` 不得实现 `Default`。
1. **试运行是默认**：`rule_apply_existing` 的类型上要求 `DryRunReport`（结构体字段强制）。
2. **正文未缓存不误判**：单测断言"正文条件在无正文时不命中且记 skipped"。
3. **无外部程序动作**：`RuleAction` 枚举里不得出现 `RunProgram`/`Shell`（枚举 + 正则）。
4. **规则循环保护**：单测覆盖"5 秒内同封邮件移动 ≥ 3 次 → 停止"。
5. **破坏性动作二次确认**：前端源码中 `Delete` 动作的保存路径必须带确认参数（源码检查）。

## 11. 逆向反驳（红队）

**反驳 1（评审后升级为 BLOCKER 的修正）：规则引擎的空条件集=匹配全部，等于"默认放行"，配上 Move/Delete 就是批量误删。**
- 证据：`AND over ∅ = true`；一条还没填完条件的规则（编辑器保存的是整个 `Rule`）会命中所有邮件；
  更隐蔽的是 `match_all=false`（OR）配上本结构自带的否定条件（`subject_not_contains`、`is_unread: Some(false)`、
  `size_under`）几乎恒真。另外 `derive(Default)` 给出 `match_all=false`，与注释里"默认 AND"**自相矛盾**。
- **改设计（三条硬规则）**：
  1. **空条件集 ⇒ 永不匹配**（写进 `evaluate()` 的第一行）；**没有任何"正向条件"（即全是排除条件）⇒ 永不匹配**。
  2. `RuleConditions` **去掉 `#[derive(Default)]`**，改成显式 `RuleConditions::new()`（`match_all: true`），
     让"默认 AND"只有一个来源，编译期就不会再出现"默认值 vs 注释"打架。
  3. **OR 模式 + 否定条件的组合**：保存时给出警告并要求勾选"我知道这条规则可能命中几乎所有邮件"；
     试运行若命中 > 20% 的邮件，也必须显式确认。
- 门禁：真值表测试（空条件、只有否定条件、OR+否定、AND 正常）；`rule_save` 不得接受"无正向条件 + 破坏性动作"的组合。
- 让步：编辑器在保存时若发现"无正向条件"，直接把它当作**未完成规则**禁用（比警告更安全）。

**反驳 1b：本地规则只能处理"已经到了本地"的邮件，用户以为它能像 Gmail 过滤器那样在服务器端生效
（即"手机上也不显示"）。**
- 证据：确实如此。服务器端过滤需要 Sieve（已否决）或服务商网页规则。
- 缓解：①UI 明确写"规则在本机生效，其它设备上的客户端仍会看到这封邮件（除非它支持同样的规则）"；
  ②对"移动到垃圾邮件/归档"这类动作，给出"同时建议在服务商网页端建一条同样的规则"的引导链接；
  ③把"本地规则"定位为**工作流自动化**（归档、标记、通知），而不是"服务器端过滤"。
- 让步：如果用户强烈需要服务器端过滤，M3 再评估 Sieve（ManageSieve 只对部分服务商可用）。

**反驳 2：规则引擎是"用户自己写的删除脚本"，很容易造成批量误删。**
- 缓解：①`Delete` 动作在编辑器里默认是**禁用**的（需要显式展开"危险动作"）；②强提示建议改用"移动到垃圾邮件"；
  ③所有批量执行都先试运行；④每个 run 有 undo；⑤"最近自动动作"面板常驻，可随时回滚。
- 让步：**默认规则集**（我们预置的示例）里不含任何删除动作。

**反驳 3：垃圾邮件以服务器判定为准，那本地规则还有意义吗？**
- 有：服务器判定管"是不是垃圾"，本地规则管"怎么归位"（项目文件夹、通知、标签）。
- 缓解：不重复造"垃圾评分"，只在服务器没判、但用户反复标记时提供"屏蔽该发件人"（明确、可控）。

**反驳 4：一键退订可能是钓鱼确认（点一下就告诉对方"这个地址是活的"）。**
- 证据：真实存在的攻击面。
- 缓解：①`http` 类型**不自动请求**，只打开系统浏览器让用户自己看；②只有 `List-Unsubscribe-Post=One-Click`
  才由我们发一次 POST（这是 RFC 8058 的标准做法，收件人本来就该这么做）；③mailto 只预填草稿；
  ④退订前显示目标域名；⑤可疑域名（新注册/与发件人域不符）给黄色警告。
- 让步：设置项"一律不自动 POST，只打开网页"给保守用户。

**反驳 5：规则与"稍后提醒/标签/虚拟文件夹"边界模糊，会让产品概念变多。**
- 缓解：概念收敛为三条：**规则**（到达时自动做动作）、**保存搜索**（随时看的视图）、**稍后提醒**（时间维度）。
  规则的动作里不包含"稍后提醒"（那是用户手动决定的事）。
- 让步：若实测用户混淆，把"自动归档"从规则里独立成单独入口（它本来就是独立策略，见 §4）。

## 12. 未决问题

| # | 问题 | 影响 | 建议 |
|---|---|---|---|
| F1 | 规则能否永久删除 | 误删风险 | **不能**（评审后已定）：规则只提供"移到垃圾箱"；永久删除是独立动作 + 显式确认 + 不能撤销（因此不做 undo 承诺） |
| F2 | 正文未缓存时的规则语义 | 误判 | 视为未知、不命中、记 skipped（§9） |
| F3 | 是否预置示例规则 | 上手速度 | 只给"模板"（点击后由用户确认才创建），不自动启用 |
| F4 | 退订是否允许自动 POST | 安全 vs 便利 | 仅对 One-Click 自动 POST，其余打开浏览器 |
| F5 | 自动归档的默认策略 | 收件箱整洁 | 默认关闭，给三档预设（30/90/365 天）供选择 |
| F6 | 规则命中通知是否合并 | 打扰 | 每批合并为一条（"规则移动了 12 封"） |

## 13. 验收清单

- [ ] 规则编辑器：条件/动作/排序/启停/试运行全部可用；试运行与实际执行结果一致（同一批邮件）。
- [ ] 10 条典型规则的真实邮件验证（GitHub/Jira/银行/营销/同事/自己发的/带附件/大附件/中文主题/英文主题）。
- [ ] 破坏性动作有二次确认、有 undo、有"最近自动动作"面板且可回滚。
- [ ] 垃圾邮件：服务器判定正确反映；"这不是垃圾"与"屏蔽发件人"都生效并收敛到服务器。
- [ ] 退订：One-Click / http / mailto 三种路径表现正确且安全（可疑域名有警告）。
- [ ] 规则循环、目标丢失、命中过多三种异常都有明确行为且不损坏数据。
- [ ] §8 六项预算达标；`check.ps1` 五项新增检查通过。
