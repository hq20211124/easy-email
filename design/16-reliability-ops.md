# 16 可靠性与运维

> **目标**：把"出错"变成**可理解、可恢复、可诊断**的三件事：用户看得懂原因、数据不会因为出错而损坏、
> 出问题时我们能在一份诊断包里看到全部现场。
>
> 覆盖 ID：REL-01…10、SEC-12（日志脱敏）、UI-20（关于/诊断入口）。
> 上游：03（自愈）、04（重试与退避）、13（后台任务）。下游：17（升级）。

---

## 1. 目标与非目标

**目标**
1. **错误可行动**：任何用户可感知的失败都有"原因 + 该怎么办"的中文文案（延续既有 `error.rs` 传统，扩展成 `ApiError`）。
2. **部分失败隔离**：一个账号/文件夹坏掉不影响其它（现在是"一个账号失败弹一个 toast"，要升级成"可见的状态 + 局部重试"）。
3. **数据自愈**：索引、游标、会话计数、待同步队列都能自检与修复（03 的 `heal`）；库损坏有备份重建路径。
4. **一键诊断**：版本、配置（脱敏）、同步状态、任务快照、网络台账、最近错误、DB 统计 —— 用户可预览再导出。
5. **升级可回退**：schema/设置迁移失败保留旧数据（03/17 已定），并有"回退到旧版本"的说明。
6. **测试矩阵与门禁**：把"这个项目最容易再犯的错"变成 CI 里的一项检查（既有传统）。

**非目标**
- 不做遥测/崩溃上传（红线；`14` §12 反驳 7）。
- 不做"自动修改服务器数据来修复本地不一致"（一切服务器写操作都走用户可见的队列）。

## 2. 用户可感知行为

| 场景 | 用户看到 |
|---|---|
| 一个账号密码过期 | 该账号在侧栏变灰 + 红色小点；点开是"Gmail 登录已过期，重新登录"；**其它账号照常收信** |
| 一个文件夹同步失败 | 文件夹旁小感叹号；tooltip 给原因；右键"重试"/"查看详情"；其它文件夹不受影响 |
| 离线 | 状态栏"离线"；界面照常；恢复后自动收敛（04 §5.5） |
| 索引损坏/落后 | 无感（夜检自愈）；设置 → 高级显示"索引：12,431/12,500（后台补）" |
| 库损坏 | 启动时明确报告："本地缓存损坏，已备份为 `cache.db.corrupt-20261003` 并重建，正在重新同步（账号与草稿不受影响）" |
| 出问题要反馈 | 设置 → 关于 → "导出诊断包"：可**预览**内容（脱敏）、含全部现场、一键复制路径给作者 |
| 升级后异常 | 设置 → 关于 → "回退说明"（含"保留旧库、装回旧版本"的步骤） |
| 反复出错 | 顶部横幅（不刷屏）："有 2 个账号需要处理"→ 点开是清单（每条一句话 + 动作） |

## 3. 数据结构与接口

### 3.1 错误模型（02 §6.1 已定，这里给**映射表**的落点）

```rust
/// MailError → ApiError 的单一映射点（可单测、可枚举覆盖）
pub fn to_api_error(e: &MailError, ctx: ErrorContext) -> ApiError;
pub struct ErrorContext { pub account_id: Option<String>, pub folder: Option<String>, pub uid: Option<u32>, pub operation: &'static str }

/// 每个 code 的 UI 契约（前端只认 code，不解析 message）
pub struct ErrorPresentation { pub code: ErrorCode, pub title: String, pub hint: Option<String>, pub retryable: bool, pub action: Option<ErrAction> }
pub enum ErrAction { Retry, Reauth, OpenSettings(&'static str), OpenWebSettings { url: String }, ContactAdmin, RetryLater }
```

### 3.2 健康状态与自愈

```rust
#[derive(Serialize, Clone, Debug)]
pub struct HealthReport {
    pub db: DbHealth, pub index: IndexHealth, pub accounts: Vec<AccountHealth>,
    pub jobs: Vec<JobSnapshot>, pub queue: QueueHealth, pub net: NetState,
    pub storage: StorageReport, pub last_heal: Option<HealReport>,
}
#[derive(Serialize, Clone, Debug)] pub struct DbHealth { pub ok: bool, pub schema_version: i64, pub integrity: Option<String>, pub size_bytes: u64, pub wal_bytes: u64, pub slow_queries: u64 }
#[derive(Serialize, Clone, Debug)] pub struct IndexHealth { pub indexed: u64, pub pending: u64, pub failed: u64, pub bytes: u64, pub last_optimize_at: Option<i64> }
#[derive(Serialize, Clone, Debug)] pub struct AccountHealth { pub account_id: String, pub state: AccountState, pub last_ok_at: Option<i64>, pub last_error: Option<ApiError>, pub folders: Vec<FolderHealth> }
#[derive(Serialize, Clone, Copy, Debug, PartialEq, Eq)] pub enum AccountState { Ok, Degraded, NeedsReauth, Paused, Offline, Disabled }
#[derive(Serialize, Clone, Debug)] pub struct QueueHealth { pub outbox_pending: u32, pub outbox_failed: u32, pub pending_ops: u32, pub imports_running: u32 }

pub async fn health(state: &AppState) -> Result<HealthReport>;
/// 每日/启动后空闲时执行；也可手动触发
pub async fn maintenance_tick(state: &AppState, job: &JobCtx) -> Result<MaintenanceReport>;
```

### 3.3 诊断包

```rust
pub struct DiagOptions {
    pub include_logs: bool, pub log_days: u32,          // 默认 3 天
    pub include_settings: bool, pub include_accounts_redacted: bool,   // 默认 true
    pub include_sample_headers: bool,                   // 默认 false（要用户显式勾，且只含头部不含正文）
    pub preview_only: bool,                             // 先在 UI 里显示内容，用户确认后再落盘
}
pub async fn diagnostics_export(state: &AppState, opts: &DiagOptions) -> Result<DiagnosticsBundle>;
pub struct DiagnosticsBundle { pub path: PathBuf, pub bytes: u64, pub manifest: DiagManifest, pub preview: String }
pub struct DiagManifest { pub app_version: String, pub os: String, pub webview: String, pub files: Vec<DiagFile>, pub redactions: u32, pub created_at: i64 }
```

### 3.4 日志

```rust
pub struct LogPolicy { pub level: Level, pub file_max_bytes: u64 /* 8 MB */, pub keep_files: u32 /* 7 */, pub redact: bool /* true */ }
pub fn log_init(policy: &LogPolicy) -> Result<()>;                 // tauri-plugin-log + 自定义脱敏层
pub fn log_line(msg: &str);                                        // 既有 startup-error.log 的正式化
pub fn tail(n: usize) -> Vec<String>;

> **实现注记（第 86 轮）**：轮转已落地。`crates/mail-core/src/logrot.rs` 持有纯规则
> （8 MB / 7 份 / `rotation_action` / `tail_lines`），`src-tauri` 的 `log_line` 在**每次写入前**
> 检查并轮转（不是只在启动时——否则一次长会话会把文件撑到下次重启）。阈值判定 `>=`
> （"差一字节"没有意义，测试钉住边界）。**轮转失败一律忽略**：日志绝不能成为应用启动不了的原因。
> 取舍记录：轮转途中崩溃 ⇒ 内容不丢但分两份，`recent_log_lines` 会少看一段，**不做合并**。                              // 设置页"查看日志"（最多 500 行）
```
**脱敏规则**（`redact()`，测试钉住 8 类）

> **实现注记（第 82 轮）**：`crates/mail-core/src/redact.rs` 落地，8 类各一个 `mask_*`
> （私钥块→邮箱→令牌→Windows 用户名→手机号→证件/银行卡→16 位授权码→IPv4），顺序是契约：
> 私钥块必须第一个（跨行、体内含 base64）。**两处只掩一半**：邮箱留域名、路径留 `Users\`。
> 定长规则（11 位手机、16 位授权码）不匹配时也要**吃掉整段**（`Span::verbatim`）——否则长串里
> 的子串会被掩（17 位码含 16 位子串，第一版就这么错了）。中文邮箱的本地部分允许非 ASCII。
> "正常行一字不变"与"跑两次结果相同"都是测试。 **尚无调用方**：日志与诊断包的接线是下一片。：邮箱地址（保留域名、掩码本地部分）、
令牌/密码形态（`ya29.*`、`1//*`、长 base64 串）、Windows 路径中的用户名、
手机号、身份证/银行卡形态、PGP 私钥块（`-----BEGIN PGP PRIVATE KEY-----` 起止整段）、
授权码形态（16 位连续字母数字）。

## 4. 主流程

### 4.1 启动自检（顺序）
```
1. 打开 cache.db / user.db（失败 → 03 的隔离重建路径）
2. 读 schema_version → 需要则迁移（失败 → 保留旧库 + 明确报错 + 不启动同步）
3. 修正在途状态：sync_jobs(running→paused)、outbox(sending→queued)、import_jobs(running→paused)
   —— 都是幂等操作；outbox 的 `sending` 回退可能造成重复发送（07 §11 反驳 4，接受）
4. quick_check（异步，不阻塞 UI）
5. health() → 有 NeedsReauth/失败账号 → 顶部横幅（**一次性，不刷屏**）
6. 启动后台任务（04 的调度器）与 IDLE（亚秒级推送，A1-F41）
7. 30 s 后检查更新（17）+ 6 h 周期
```

### 4.2 日常维护（每 24 h 或空闲时）
```
maintenance_tick:
  ├ 03 的 heal::check：孤儿 bodies/索引、游标校正、会话计数抽查（1%）
  ├ 09 的 fts::consistency：索引行数与 messages 对账（差异 → 重建差异部分）
  ├ quota::plan + 必要时 apply（**干跑数字与实跑一致**）
  ├ FTS optimize + `pragma incremental_vacuum`
  ├ 过期任务清理：sync_jobs 完成 > 7 天、notification_log > 30 天、
  │   import_dedupe > 30 天、已 sent 的 outbox.spool 文件清理
  └ 写 MaintenanceReport 进日志与 health()
```

### 4.3 失败分类与重试策略（统一表）

| 类别 | 例子 | 重试 | 用户可见 | 动作 |
|---|---|---|---|---|
| 瞬时网络 | 超时、连接重置、DNS 抖动 | 指数退避（1s→60s，上限 5 次/任务） | 无（除非持续 > 3 次） | 自动 |
| 服务商限流 | 429/`NO [LIMIT]`/`BYE too many` | 退避 + 降并发（04 §5.7 配额账本） | 状态栏"服务器繁忙，N 分钟后重试" | 自动 |
| 凭据失效 | 401/`invalid_grant`/`AUTHENTICATIONFAILED` | **不重试** | 账号红点 + "重新登录" | `Reauth` |
| 权限/策略 | SMTP AUTH 被关、管理员未同意、网易协议未开 | **不重试** | 具体中文指引（05 §6） | `OpenSettings`/`ContactAdmin` |
| 数据问题 | 解析失败、附件损坏、MIME 异常 | 不重试 | 该封/该附件上标"无法解析" | 手动 |
| 本地资源 | 磁盘满、DB busy 持续、内存压力 | 退避 + 暂停相关任务 | 横幅 + 存储设置入口 | `OpenSettings("storage")` |
| 本地数据损坏 | quick_check 失败、vault 损坏 | 不走重试 | 明确报告 + 重建/重登 | `OpenSettings("advanced")` |
| 程序缺陷 | panic、`Internal` | 不重试 | "内部错误（已记录）"+ 诊断包入口 | 手动 |

## 5. 失败模式（本模块自身的失败也要设计）

| 触发 | 用户看到 | 处理 |
|---|---|---|
| 诊断包导出失败（磁盘满） | 明确原因 + 换个目录 | 不静默 |
| 日志暴增（死循环打日志） | 单文件 8 MB 上限 + 每天 1 个文件 + 保留 7 个 | 超限丢弃并计数（health 里显示"丢弃 N 行"） |
| health() 自身超时 | 显示"正在收集…" + 部分结果 | 分项超时（每项 2 s），不阻塞设置页 |
| 维护任务与同步抢写 | 无感 | 写队列串行（02 §4.1）+ 维护任务低优先级 |
| 修复动作误删数据 | —— | 所有 heal 只删"确认无父行的孤儿"，且**先备份到 `heal-quarantine/`**（可手动恢复） |
| 错误文案缺失（新 code 没写文案） | 显示兜底文案 + code | 门禁要求全枚举有文案（02 §6.1） |
| 回退到旧版本（schema 更高） | 启动报"本地数据由更新版本创建" + 指引 | 明确不自动降级（避免损坏新数据） |

## 6. 性能预算

| 指标 | 预算 |
|---|---|
| 启动自检（不含迁移） | ≤ 300 ms |
| quick_check（10 万封） | ≤ 5 s（异步） |
| health() | ≤ 500 ms |
| 维护任务（10 万封） | ≤ 60 s，仅空闲时，可中断 |
| 诊断包（3 天日志 + 配置） | ≤ 5 MB，导出 ≤ 5 s |
| 日志写入 | ≤ 1 ms/行（异步、有界队列） |
| 错误横幅出现延迟 | ≤ 1 s（事件驱动） |

## 7. 测试矩阵（本项目"验证"二字的落地形态）

| 层 | 范围 | 何时跑 |
|---|---|---|
| 单元 | 纯逻辑：映射表、脱敏、指纹、线程串、分词、规则匹配、时间/时区 | 每次（`cargo test`，秒级） |
| 假服务器 | IMAP/SMTP/POP3/OAuth/HTTP(Graph·JMAP·CalDAV) 的协议交互、断线、限流、取消、事务原子性 | 每次（`cargo test`，分钟级） |
| 迁移 | v1/v2/v3 真实库样本升级、失败回滚、损坏隔离 | 每次 |
| 性能回归 | 冷启动/切文件夹/搜索/滚动/索引吞吐/内存（**CI 用 1 万封夹具**，与 15 §8 一致；10 万封只在本机/里程碑跑） | 每次 CI（阈值放宽 2×，看趋势） |
| UI 冒烟 | CDP 驱动：启动→列表→读信→写信→发送→搜索→设置；含"显示图片"与"附件另存" | 每次 CI（10 分钟内） |
| 真机端点 | 21 家服务商的 TLS+握手+能力矩阵（A1-F43 那张表） | 手动/夜间（`check.ps1 -Network`） |
| 真机功能 | 至少 4 家（QQ/163/Gmail/Outlook）的收/发/同步/附件往返 | 每个里程碑前手动一次 |
| 长稳/混沌 | 24 h 连续 + 随机断连/500/慢响应；断言数据一致与无泄漏 | 里程碑前 |
| 安装/升级 | 干净安装、覆盖升级、更新失败回滚、卸载残留 | 每个发布候选 |

**"没验过"的纪律**：任何在本矩阵之外的结论（尤其是库行为、服务商策略）必须写进 `HANDOFF.md` 的"还没验证的"，
不许用"应该没问题"糊过去（这次 IDLE 的教训就是活证据，见 A1-F41）。

## 8. 门禁新增项（`tools/check.ps1`）

1. **`ErrorCode` 全枚举有文案**（02 §6.1 的测试）。
2. **日志脱敏**：8 类敏感形态各有断言；且 `log_line` 的实现必须经过 `redact()`（源码检查）。
3. **无遥测**：源码/依赖里不得出现已知遥测 SDK 的名称；出站端点集合 ⊆ `NetPurpose`（14 §11 门禁 4）。
4. **在途状态修正**：单测覆盖 `sending→queued`、`running→paused` 两条启动修正。
5. **维护任务不联网**（除更新检查外）：断言维护路径零网络调用。
6. **诊断包清单**：导出内容必须与 `DiagManifest.files` 一致（防止偷偷多带东西）。

## 9. 逆向反驳（红队）

**反驳 1：把错误做成"横幅 + 清单"会让人觉得软件毛病很多（现在至少是安静的）。**
- 证据：安静地失败更糟——用户会在几天后才发现"某些邮件一直没收进来"。
- 缓解：①横幅只在"需要用户动作"时出现（Reauth/权限/磁盘），**纯瞬时错误不打扰**；
  ②每条都带一个动作按钮；③解决后自动消失并留一条日志；
  ④设置页的"健康"页给愿意看的用户更多细节（不给所有人看）。
- 让步：首次出现的瞬时错误不弹横幅，连续 3 次才升级为可见。

**反驳 2：`heal` 自动"修复"可能删掉用户数据（例如误判孤儿）。**
- 缓解：①只删"父子关系确认缺失"的行（bodies/attachments/index 引用不存在的 message），
  绝不删 messages 行本身；②删除前把行导出到 `heal-quarantine/*.json`；
  ③维护报告里列出"隔离了 N 行"，用户可一键恢复；④索引重建是逻辑删除+重建，本身可重复。
- 让步：**messages 行、drafts、outbox、contacts、rules 永不被 heal 删除**（写进门禁：heal 模块不得出现对这些表的 delete）。

**反驳 3：诊断包含"同步状态 + 头部样本"可能泄露隐私；不含又难排障。**
- 缓解：①默认**不含**任何邮件头部/正文；"包含样本头部"是显式勾选（并说明"只含头部，不含正文"）；
  ②邮箱地址掩码（保留域名）；③导出前**预览**（前 200 行 + 文件清单）；
  ④诊断包是本地文件，**我们不会自动上传**（用户自己决定发给谁）。
- 让步：若未来提供"一键发送给作者"，必须先给用户看完整内容再确认（不做后台静默上传）。

**反驳 4：把"没验过"写进 HANDOFF 是流程，不是产品 —— 用户不会因此受益。**
- 但它决定**我们会不会重犯 IDLE 那类错误**（A1-F41：一条错误的库结论让产品少了一个卖点、
  多了一个"50 秒延迟"的公开限制）。缓解：①把"库能力"做成**可执行的探针测试**（不是文档结论）：
  `tests/library_probe.rs` 断言"IDLE 能在 1 s 内收到 NewData"等关键行为，
  库升级时立刻发现行为变化；②README/HANDOFF 里的每条库结论都标注"哪次实测、什么版本"。
- 让步：探针测试只覆盖"我们真正依赖的库行为"（IDLE/EXISTS、APPEND、SEARCH 透传、分部取件的失败模式），
  不追求覆盖全部 API。

**反驳 5：性能回归测试在 CI 上不稳定（机器差异导致假警报）。**
- 缓解：①阈值**宽松但绝对**（例如冷启动 ≤ 3 s 报警，而预算是 1.5 s）；
  ②记录历史曲线（每次 CI 存数值），看**趋势**而不是单点；③只在明显退化（> 30%）时失败；
  ④把精确测量留给 `tools/bench.ps1`（手动、带说明）。
- 让步：CI 只跑"冒烟级性能"（能跑完且不超宽松阈值），精确基准手工跑。

## 10. 未决问题

| # | 问题 | 影响 | 建议 |
|---|---|---|---|
| R1 | 横幅的触发阈值（连续 3 次？） | 打扰程度 | 连续 3 次或"需要用户动作"两类 |
| R2 | 诊断包是否需要"一键发邮件给作者" | 反馈闭环 | M2：用用户自己的默认邮件客户端打开草稿（我们不代发） |
| R3 | 健康页给多少细节 | 认知负担 | 默认折叠，高级用户可展开 |
| R4 | 是否引入本地崩溃转储解析（minidump） | 排障能力 | M3 评估（需要符号与工具链，收益有限） |
| R5 | 日志保留策略（7 天/8 MB）是否够 | 排障 vs 磁盘 | 先用默认值；诊断包里可让用户选择时间范围 |
| R6 | 回退到旧版本的自动化程度 | 支持成本 | 只提供文档步骤（不自动降级数据） |

## 11. 验收清单

- [ ] 20 个代表性失败场景（覆盖 §4.3 每类至少两条）都给出"原因 + 怎么办"，且没有裸库错误字符串。
- [ ] 一个账号密码过期时，其它账号照常收信（真机/假服务器验证）。
- [ ] 库损坏注入测试：备份 + 重建 + 重新同步，账号/草稿/规则/联系人完好。
- [ ] `heal` 的隔离文件可恢复，且 heal 代码里没有对 messages/drafts/outbox/contacts/rules 的 delete（门禁）。
- [ ] 诊断包脱敏 8 类断言通过，导出前可预览，体积 ≤ 5 MB。
- [ ] 测试矩阵七层全绿（含 UI 冒烟与性能冒烟）；`tools/library_probe.rs` 通过。
- [ ] `check.ps1` 六项新增检查通过。

> **章节名对照（模板 13 节）**：接口→§3 数据结构与接口；状态机→§4 主流程；测试→§7 测试矩阵。
> 内容齐全，只是节名与模板不完全同名（见 design/README.md 的说明）。
