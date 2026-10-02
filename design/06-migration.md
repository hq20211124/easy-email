# 06 迁移与导入（换过来的人不能丢邮件）

> **目标**：让"从别的客户端换过来"这件事**可信、可预期、可回退**：账号能自动配好、
> 本地存档能进来、联系人能带过来、导出的数据能再带走。
>
> 覆盖 ID：MIG-01…06、UI-14（首次运行向导的迁移入口）、SYN-13（备份/导出）。
> 事实依据：A1 §一（Thunderbird 配置文件、PST 现实、Gmail 配额、网易 30 天窗口、APPEND 上限、CSV 编码坑）。
> 上游：03（表结构）、04（同步）、05（账号）。下游：11（联系人）、19（里程碑）。

---

## 1. 目标与非目标

**目标**
1. **三条迁移路径都要能用，且用户知道该走哪条**：
   ① **从服务器重新同步**（首选，最完整）；② **导入本地存档**（EML/MBOX/Maildir）；③ **导入账号与联系人**（联系人只走"用户自己导出的 vCard/CSV"；**Thunderbird 的 `abook.sqlite`/`.mab` 不在承诺内** —— 评审指出 A1-F5 提到过它，但设计里没有任何函数读它，所以从目标里划掉，避免"承诺了没人做"）。
2. **默认无害**：导入默认只读源文件、不删源、不自动上传到服务器、先干跑给报告。
3. **可中断可续跑**：50 GB 存档导入中断后可继续，进度可见。
4. **可带走**：导出 mbox/EML/设置/联系人，且导出结果能被 Thunderbird 直接导入（人工验收）。
5. **不碰红线**：不导入别人的密码库（P14），不捆绑 GPL 二进制（P13）。

**非目标**
- **不做 PST/OST 解析**（P13：无纯 Rust 实现、唯一路线是许可证自相矛盾的 C sidecar、且 New Outlook 已无 MAPI）。
  替代方案见 §5.5（用户自行用第三方工具导出成 mbox/eml，我们再导入）。
- 不做"从网页邮箱抓取"（违反服务条款且脆弱）。
- 不做双向同步的"本地账号 ↔ 服务器账号"迁移器（上传是显式动作，见 §5.4）。

## 2. 用户可感知行为

| 场景 | 用户看到 |
|---|---|
| 首次运行 | 向导第一步："你有其它邮件客户端吗？"→ 列出探测到的 Thunderbird/Outlook（仅提示，不自动改） |
| 选了"从服务器重新同步" | 按 04 的阶段 A 立刻可用；同时给**诚实的体量预估**："全部约 8.4 GB：Gmail 每天限约 2.3 GB，约 4 天；首屏现在就能用。" |
| 选了"导入本地存档" | 选文件夹 → **干跑报告**："发现 12,431 封，其中 8,902 封与现有邮件重复（将跳过），预计 3 分钟，占用约 1.2 GB" → 确认后开始，进度可暂停 |
| 导入完成 | "导入 3,529 封到本地文件夹『旧邮箱存档』（跳过重复 8,902 封）" |
| 想把导入的存档也放到服务器 | 右键文件夹 →"上传到服务器…"→ 提示："Gmail 每天只允许上传约 500 MB，全部上传约需 21 天，确定吗？" |
| 导入联系人 | 选 vCard/CSV → CSV 出现**列映射界面**（自动猜 + 手工改）→ 预览前 5 条 → 导入 |
| 想换机器 | 设置 → 备份 → 生成 `easy-email-backup-*.db`（含草稿/规则/联系人/设置，**不含密码**）→ 新机器恢复后提示重登 |
| 想离开我们 | 设置 → 导出 → 文件夹导出 mbox / 单封 EML / 设置 JSON；导出后可直接拖进 Thunderbird |
| 导入中断（断电/关应用） | 重启后提示"上次导入还有 1,204 封未完成，继续吗？" |

## 3. 数据结构（对 03 的增补）

03 的表基本够用，本模块需要以下**增补**（实现时并入 03 的 v3 迁移）：

```sql
-- 账号多了"类型"这一轴：imap / pop3 / local（本地存档账号，不连服务器）
alter table accounts add column kind text not null default 'imap';
-- 本地账号的文件夹也存 folders 表，用 account_id 归属；selectable=1，role 自定义

-- **归属（与 03 的双库拆分一致）**：import_jobs → user.db（导入计划与用户选择不能因删缓存而丢）；
-- import_dedupe → cache.db（只是加速器，真正的去重依据是 messages.message_id，同库同生共死）

-- 导入任务（可续跑、可报告）
create table if not exists import_jobs (
    id            text primary key,
    kind          text not null,              -- mbox | maildir | eml_dir | thunderbird | contacts | settings
    source_path   text not null,
    account_id    text,                       -- 目标账号（本地账号或普通账号）
    folder_map_json text,                     -- 源文件夹 → 目标文件夹
    options_json  text not null,              -- 去重策略/是否上传/编码提示…
    state         text not null default 'scanning', -- scanning|awaiting_confirm|running|paused|done|failed|cancelled
    scanned       integer not null default 0,
    imported      integer not null default 0,
    skipped_dup   integer not null default 0,
    failed        integer not null default 0,
    cursor_json   text,                       -- 断点（文件偏移/目录项索引）
    cache_epoch   text,                       -- **评审补**：导入当时 cache.db 的世代标记（建库写进 kv）。
                                              --   删过缓存后 epoch 对不上 ⇒ 报告必须说"需重新扫描"，
                                              --   而不是照旧 cursor"继续"（否则计数不符、本机存档会少一截）
    started_at    integer, finished_at integer,
    last_error    text
);

-- 导入去重指纹（message_id 缺失时兜底）
create table if not exists import_dedupe (
    account_id text not null, fingerprint text not null,
    primary key (account_id, fingerprint)
);
-- fingerprint = sha1(normalised_message_id | subject_norm | from_addr | date | size)
```

```rust
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ImportOptions {
    /// 默认 true：只读源，不动源文件
    pub read_only_source: bool,
    /// 默认 Skip：遇到重复怎么处理
    pub duplicate_policy: DuplicatePolicy,          // Skip | ImportAnyway
    /// 源编码提示（用于 .eml 文件名/CSV；邮件正文编码由 mail-parser 处理）
    pub source_encoding: Option<String>,            // "gbk" | "utf-8" | auto
    /// 导入后是否建立索引（默认 true）
    pub index_after_import: bool,
}
// **评审删除的字段**：原来的 `upload_to_server: bool` 被删掉了 —— 它与"本地导入零网络"的门禁
// 直接冲突（见 §9/§10.1），也让"导入完成"的提示变成谎话。上传是**独立动作**（§5.4），
// 有自己的确认、配额预估与进度。
#[derive(Serialize, Deserialize, Clone, Copy, Debug)] pub enum DuplicatePolicy { Skip, ImportAnyway }

/// **导入目标账号的硬约束（第二轮评审的 BLOCKER）**：
/// 导入只允许落到 `accounts.kind ∈ {local, pop3}` 的账号。
/// **禁止导入到 `kind='imap'` 的账号** —— 那些行的 uid 无法与服务器 UID 共存
/// （顺序分配会覆盖缓存里真实的邮件；保留高位又会让规则/归档/已读误发服务器命令，
/// 甚至撞上真实 UID 删掉真邮件）。要上服务器，走 §5.4 的显式上传。
pub fn validate_import_target(account: &AccountConfig) -> Result<()>;


#[derive(Serialize, Clone, Debug)]
pub struct ScanReport {
    pub kind: ImportKind, pub source: String,
    pub folders: Vec<ScanFolder>, pub total_messages: u64, pub duplicates: u64,
    pub estimated_bytes: u64, pub estimated_seconds: u64,
    pub warnings: Vec<String>,                      // 例如"检测到 GBK 编码的 CSV"、"发现 3 个 0 字节文件"
}
#[derive(Serialize, Clone, Debug)] pub struct ScanFolder { pub source_name: String, pub messages: u64, pub bytes: u64, pub suggested_target: String }
```

## 4. 接口（函数级）

```rust
// ── 探测本机其它客户端 ────────────────────────────────────────
pub struct DetectedClient { pub kind: ClientKind, pub label: String, pub profile_path: PathBuf, pub version: Option<String>, pub accounts: Vec<DetectedAccount> }
#[derive(Serialize, Clone, Debug)] pub enum ClientKind { Thunderbird, OutlookClassic, OutlookNew, WindowsMail, Foxmail, NetEaseMailmaster }
pub fn detect_clients() -> Vec<DetectedClient>;                   // 只读、无网络
pub struct DetectedAccount { pub email: String, pub display_name: Option<String>, pub imap: Option<Endpoint>, pub smtp: Option<Endpoint>, pub auth_guess: AuthMethod }
/// 从 Thunderbird 配置读账号（prefs.js；两种格式都探测）——**不读密码**（P14）
pub fn parse_thunderbird_accounts(profile: &Path) -> Result<Vec<DetectedAccount>>;
/// 读 Thunderbird 签名/身份（prefs 里的 htmlSigText 等），供导入设置用
pub fn parse_thunderbird_identities(profile: &Path) -> Result<Vec<DetectedIdentity>>;

// ── 扫描（干跑）与导入 ───────────────────────────────────────
pub async fn scan(kind: ImportKind, source: &Path, opts: &ImportOptions) -> Result<ScanReport>;
pub async fn import_start(db: &Db, plan: ImportPlan, job: &JobCtx) -> Result<ImportReport>;
pub async fn import_pause(job_id: &str) -> Result<()>;
pub async fn import_resume(db: &Db, job_id: &str, job: &JobCtx) -> Result<ImportReport>;
pub async fn import_cancel(db: &Db, job_id: &str) -> Result<()>;
pub struct ImportPlan { pub kind: ImportKind, pub source: PathBuf, pub account_id: String, pub folder_map: Vec<(String, String)>, pub options: ImportOptions }

// ── 解析器（都是流式 + 可续跑）────────────────────────────────
/// mbox：自己写（**crates.io 的 `mbox` crate 不是解析器**，A1-F10）
/// 支持 mboxo/mboxrd/mboxcl、`>From` 反转义、最后一条无空行截断、CRLF/LF 混用
pub struct MboxReader { /* file, offset, current folder */ }
impl MboxReader {
    pub fn open(path: &Path) -> Result<Self>;
    pub fn next_message(&mut self) -> Result<Option<RawMessage>>;   // RawMessage { bytes, offset_start, offset_end }
    pub fn seek(&mut self, offset: u64) -> Result<()>;              // 断点续跑
}
pub struct MaildirReader { /* dirs: cur/new/tmp */ }
impl MaildirReader { pub fn open(dir: &Path) -> Result<Self>; pub fn next_message(&mut self) -> Result<Option<RawMessage>>; }
pub fn read_eml(path: &Path) -> Result<RawMessage>;

/// 落库（**批量事务**，与 04 的 commit_page 同一形状）
pub async fn store_imported(db: &Db, account_id: &str, folder: &str, batch: &[RawMessage], opts: &ImportOptions) -> Result<StoreReport>;
pub struct StoreReport { pub imported: u32, pub duplicates: u32, pub failed: u32 }

// ── 上传到服务器（显式动作，默认关）──────────────────────────
pub async fn upload_local_folder(ctx: &AppCtx, account_id: &str, folder: &str, job: &JobCtx) -> Result<UploadReport>;
pub fn estimate_upload_days(bytes: u64, provider: &Provider) -> Option<f32>;   // 用 A1-F1 的配额算

// ── 联系人 / 设置导入导出 ────────────────────────────────────
pub fn parse_vcards(bytes: &[u8]) -> Result<Vec<ContactDraft>>;                 // calcard
pub fn parse_contacts_csv(bytes: &[u8], encoding: Option<&str>) -> Result<CsvPreview>;  // 含列名与猜到的映射
pub async fn import_contacts(db: &Db, drafts: &[ContactDraft], mapping: &ColumnMapping, opts: MergePolicy) -> Result<ContactImportReport>;
pub fn export_contacts(db: &Db, account_id: Option<&str>, format: ContactFormat) -> Result<Vec<u8>>;  // vCard 3.0 / CSV(UTF-8 BOM)
pub fn export_settings(state: &AppState) -> Result<Vec<u8>>;                    // JSON + schema_version，**不含凭据**
pub async fn import_settings(bytes: &[u8], state: &AppState, mode: MergeMode) -> Result<SettingsImportReport>;

// ── 导出（与 03 的 export 模块对齐）──────────────────────────
pub async fn export_folder_mbox(db: &Db, account_id: &str, folder: &str, dest: &Path, job: &JobCtx) -> Result<ExportReport>;
pub async fn export_messages_eml(db: &Db, keys: &[MsgKey], dest: &Path, job: &JobCtx) -> Result<ExportReport>;
```

## 5. 状态机与主流程

### 5.1 首次运行向导（迁移入口）
```
欢迎 → "你最想做什么？"
 ├ 我现在就开始用（从服务器同步）      → 05 的加账号流程 → 04 的阶段 A/B → 顺带显示体量预估
 ├ 我有本地存档要导进来                → 选类型(mbox/maildir/eml目录) → scan → 报告 → 确认 → import
 ├ 帮我把账号配好                      → detect_clients() → 列出可导入账号（**要用户重输密码/授权码**）
 └ 只要联系人                          → vCard/CSV → 列映射 → 导入
完成后：设置默认邮件程序（17 §4.2）+ 通知偏好 + 同步窗口（04 §5.7 的三档快捷选项）
```

### 5.2 导入（流式、可续跑、可暂停）
```
scan（只读、不写库）
  ├ 遍历源（文件/目录/账号），统计封数/字节/重复（用 import_dedupe 的指纹集合）
  └ 产出 ScanReport → UI 展示 → 用户确认/改映射 → 写 import_jobs(state=awaiting_confirm)
start
  ├ state=running；按 batch=200 封：解析 → 落库（一个事务，**每行写 `origin='imported'`**）→ 写指纹 → 更新 cursor_json
  （`origin='imported'` 是 03 §3 的标记位：本机存档**永不进 `pending_ops`、永不被配额清理、永不上传**，
    除非用户在导入完成后显式点"上传到服务器"；三条各有一处门禁断言）
  ├ 每批检查 cancel/pause；每批发 job:progress
  ├ 索引：把新邮件塞进 09 的 index_queue（不阻塞导入）
  └ 完成 → ImportReport（导入/跳过/失败/耗时/占用）
失败
  ├ 单封失败（解析不出来）→ 记 failed + 样例原因，继续（**不因一封坏邮件中止全部导入**）
  └ 整批失败（磁盘满/权限）→ 暂停 + 明确原因 + 可续跑
```

### 5.3 去重规则（保守优先，**定义只有一处**）
```
**唯一指纹定义（评审后统一）**：
  fingerprint = sha1( normalised_message_id | subject_norm | from_addr | date | size )   // 存前 16 字节
  其中 normalised_message_id 走 03 的统一函数（小写、去尖括号与空白）——**全库只有这一个规范形式**。

**三档判定**：
  1. 指纹完全相同（整条元组一致）→ 判重，Skip（计数）
  2. **只有 Message-ID 相同、其它字段不同** → **不判重**，进"疑似重复"桶：
     报告里列出样本（把 Gmail 的 All Mail/多标签重复、以及"同一封信被转发/重新发送"区分开），
     给一个"全部导入"的按钮由用户决定 —— **绝不静默丢弃**（A1-F4：Gmail 会合法地存在同 Message-ID 的副本）
  3. 没有 Message-ID → 用整条元组兜底；仍然只做"完全相同才判重"
已存在的去重依据始终是 `messages.message_id`（同库同生共死）；`import_dedupe` 只是加速器。
**绝不因为"看起来像"就删源文件**（源永远只读）。
```

### 5.4 本地账号 → 服务器（显式、可撤销、幂等）
```
右键"上传到服务器"（**独立动作**，不是导入时的选项）
  ├ provider 配额检查（Gmail 500 MB/天，A1-F1）→ 显示"约需 N 天"
  ├ 确认 → 入队 pending_ops(kind=append_message)（**复用 07 的发件箱重试机制**，但走 APPEND）
  ├ **每次 APPEND 之前先 `UID SEARCH HEADER Message-ID "<规范形式>"`**（第二轮评审）：
  │   命中 → 说明上次重试其实成功了 → 直接标记 done，**不重复上传**
  │   （"靠 Message-ID 去重"只在 Gmail 成立；Dovecot/Exchange/163/QQ 会老老实实收下第二份）
  ├ 上传是**复制**语义：本地保留；成功后写 `uploaded_at` + `server_uid`
  ├ APPEND 之后拿不到新 UID（`APPENDUID` 被丢弃，A1-F43）⇒ 用同一个 SEARCH 找回；
  │   找不到就记 `server_uid=unknown`，下次同步按 Message-ID 对账（**不重传**）
  └ 失败（超限/权限）→ 停止并报告；**大小/权限类失败标为永久失败**，不再无限重试
```

### 5.5 PST/Outlook 存档的替代路径（**不是我们做**）
```
设置 → 导入 → "我有 Outlook 的 PST 文件"
  └ 页面说明三步（我们只做第三步的导入）：
     ① 用你信任的第三方工具（如 libpff 的 pffexport / readpst / 商业转换器）把 PST 导出成 mbox 或 eml 目录；
     ② 导出到任意文件夹；
     ③ 回到这里选择"mbox / eml 目录"导入。
  为什么这样：把 GPL/许可争议留给用户自己选择的工具，我们不捆绑、不再分发；
  且 New Outlook 已无 MAPI，捆绑方案也会随时间失效（A1-F8/F9）。
```

## 6. 失败模式与降级

| 触发 | 用户看到 | 数据 | 处理 |
|---|---|---|---|
| mbox 格式变体/损坏 | 报告里列出"N 封无法解析（示例：第 12,431 条）" | 源只读，未损 | 跳过坏块并继续；提供"导出失败样本" |
| 超大 mbox（> 20 GB） | 扫描阶段提示耗时与磁盘占用 | 无 | 流式解析 + 断点续跑（按文件偏移） |
| 磁盘空间不足 | "还需要约 1.2 GB，当前可用 300 MB" | 已导入的保留 | 导入前用 ScanReport 预估并检查可用空间 |
| 编码混乱（GBK/BIG5 正文） | 无感（`mail-parser` 41 种字符集，含 GB18030/GBK） | 无 | 无法识别时按 Latin-1 兜底并标"疑似乱码" |
| CSV 表头是中文（中文版 Outlook 导出） | 列映射界面预选不对 → 用户手工改 | 无 | **必须给列映射 UI**（A1-F12），不自动猜死 |
| 导入的邮件与服务器邮件重复 | 报告里写明跳过数 | 无 | 指纹去重（§5.3） |
| 用户导入后以为"服务器上也有了" | 文件夹上标着"仅本地"徽标 + 工具栏给"上传到服务器" | 无 | UI 语义明确 + §5.4 |
| Thunderbird 账号导入后要求输密码 | "为了安全，我们不会读取其它客户端保存的密码。请重新输入一次。" | 无 | P14 的正面表述 |
| 导入过程中应用被关闭 | 下次启动提示续跑 | 已导入保留 | `import_jobs.state=running` 启动时改为 `paused` |
| 导出为 mbox 时缺原文 | 该封从缓存重建（可能丢失原始头顺序） | 无 | 无 `raw_zstd` 时先按需取回（03 的策略） |

## 7. 迁移与兼容

- **对 v0.2 用户**：他们已有账号与缓存；本模块只**新增**（导入/导出/备份），不改既有路径。
- **对 03 的 schema 增补**：`accounts.kind`、`import_jobs`、`import_dedupe`（并入 v3 迁移；都是 `add column` / `create table if not exists`）。
- **设置导出格式**：`{schema_version, settings, accounts(不含凭据), signatures, templates, rules, saved_searches}`；
  导入时按 `schema_version` 做向前兼容（未知字段忽略 + 提示）。
- **导出格式只增不改**：v1 之后只允许加字段。

## 8. 性能预算

| 指标 | 预算 |
|---|---|
| 扫描速率 | ≥ 2,000 封/秒（只读头 + 指纹，不解码正文） |
| 导入速率 | ≥ 300 封/秒（含解析、落库、指纹） |
| 导入峰值内存 | ≤ 64 MB（流式；单封 > 64 MB 的邮件单独处理并警告） |
| 50 GB mbox 扫描 | ≤ 60 s（只读偏移与头部） |
| 联系人导入 | 10,000 条 ≤ 5 s |
| 导出速率 | ≥ 500 封/秒（写 mbox） |
| 进度事件频率 | ≤ 每 200 ms 一次 |

## 9. 测试计划

| 层 | 用例 |
|---|---|
| 夹具 | 自建 `tests/fixtures/import/`：mboxo/mboxrd/mboxcl 三种、maildir（cur/new/tmp）、单封 eml、**截断的最后一封**、0 字节文件、GB2312 正文、中文文件名附件、无 Message-ID 的邮件、CSV（UTF-8 BOM / GBK / 中文表头）、vCard 2.1（QP + CHARSET，A1-F12） |
| 单元 | 指纹去重规则（同 Message-ID 不同正文 → 判重；无 Message-ID 的相似邮件 → **不**判重）；`parse_thunderbird_accounts`（prefs.js 两种格式）；列映射猜测 |
| 集成 | 导入 → 导出 mbox → 再导入到另一个本地账号 → **封数/主题/时间完全一致**（幂等往返） |
| 中断 | 导入 5,000 封，到 2,000 时 kill → 重启续跑 → 总数正确且无重复 |
| 性能 | §8 每项纳入 `tools/bench.ps1` |
| 真机 | 用一份真实 Thunderbird profile（脱敏）走"账号导入 + mbox 导入 + 联系人导入"；导出结果拖进 Thunderbird 能读 |
| 无网络断言 | 本地导入全程**零网络调用**（假连接池断言）——这是"本地优先"的红线验证 |

## 10. 门禁新增项

1. **导入零网络**：集成测试断言本地导入路径不触发任何网络调用（用假连接池计数）。
2. **源只读**：导入测试前后对比源文件 mtime 与 sha256（必须不变）。
3. **干跑默认**：`import_start` 必须带 `ScanReport`（类型上要求），防止"直接开干"的写法。
4. **不解析密码库**：源码中不得出现 `logins.json` / `key4.db` 的读取（正则；P14 的机械保证）。
5. **导出可往返**：`export → import` 的往返幂等测试必须在 CI 里跑（防止格式悄悄改坏）。

## 11. 逆向反驳（红队）

**反驳 1："从服务器重新同步"对很多人是假承诺：Gmail 每天 2.5 GB、网易企业邮只给 30 天
（A1-F1/F2）⇒ 一个 163 用户换过来只看到 30 天邮件，会认为我们丢了他的存档。**
- 证据：这是**产品级**的失败，不是技术问题。
- 改设计：①加账号向导**必须**在开始同步前给出体量与时间预估，并给"只要最近 90 天 / 最近 2 年 / 全部"三档；
  ②检测到服务端下界（04 的 `server_floor`）时弹出**可操作**的提示（去网页端改"收取范围"）；
  ③本地存档导入作为"我有备份"的补充路径，并在向导里并排呈现；
  ④文档与 UI 都**不承诺"全部同步完成"**，只说"已缓存 X 封（共 Y 封，后台继续）"。

**反驳 2：本地账号（不连服务器）会让用户困惑："我导入的邮件在手机上怎么看不到？"**
- 缓解：①本地账号在侧栏有明确分区与图标（"本机存档"），文件夹带"仅本地"徽标；
  ②导入完成时给一句明确的话（"这些邮件只在这台电脑上，可随时上传到服务器"）；
  ③提供一键"上传到服务器"并显示配额与预计天数。
- 让步：向导里默认选项仍是"从服务器同步"，本地导入是**并列**选项而不是默认，避免大多数用户误入。

**反驳 3：自己写 mbox 解析器会踩坑（换行、`>From`、编码、截断）。**
- 证据：crates.io 上根本没有可用的 mbox 解析 crate（A1-F10），所以我们**必须**自己写；
  风险靠夹具与"坏数据不中止"策略压住。
- 缓解：①三种 mbox 变体各有夹具；②坏块跳过并报告，绝不中止整批；③导入幂等往返测试；
  ④解析器只做"切分 + 交给 mail-parser"，不自己解析 MIME。
- 让步：`mboxcl`/`mboxcl2` 这类罕见变体只做"尽力"，不支持时明确报错而不是猜。

**反驳 4：不做 PST 会丢掉一批 Outlook 用户。**
- 证据：PST 的现实是"没有纯 Rust 实现 + 唯一路线要捆绑许可有争议的 C 二进制 + New Outlook 已无 MAPI"（A1-F8/F9）。
- 缓解：①**替代路径写进 UI**（§5.5：用户用第三方工具导出成 mbox/eml，我们再导入）；
  ②明确告知"如果你用的是 M365/Exchange，直接走服务器同步更完整"；
  ③文档里诚实写"本版本不支持直接读 PST"。
- 让步：若后续真有一批用户卡在这里，再评估"由用户自行放置 pffexport 可执行文件、我们只读它的输出"
  （零再分发，风险最低）——记入未决。

**反驳 5：不导入别人的密码会让人觉得"你做不到"。**
- 证据：技术上能做到（A1-F6），但"解密别的客户端的凭据库"与凭据窃取工具无法区分（A1-F7）。
- 缓解：把这件事**正面表述**（"出于安全，我们不读取其它客户端保存的密码；请重新输入一次，约 60 秒"），
  并在导入账号后立刻复用 05 的授权码/ OAuth 引导（把 60 秒变得不难）。
- 让步：若未来有明确的企业需求，可做"用户主动指定密码库并输入主密码"的**显式同意**版本（仍不建议）。

**反驳 6：导入的邮件会污染搜索与未读计数（几千封历史未读突然出现）。**
- 证据：真实问题（导入的历史邮件很多是未读状态）。
- 缓解：①导入时**默认标记为已读**（可选项：保留原状态），并在报告里说明；
  ②导入的文件夹默认不参与"统一收件箱"与通知（用户可手动打开）；
  ③导入的时间戳沿用原邮件的 Date/InternalDate（排序正确），不会挤在"今天"。
- 让步：设置里给"导入后标记为已读 / 保留原状态"的开关，默认前者。

**反驳 7：`import_dedupe` 指纹表会随导入无限增长，占用可观。**
- 证据：sha1 十六进制 40 字符 + 索引，1,000 万封约 600 MB —— 对纯本地账号不现实。
- 改设计：指纹只存**摘要前 16 字节**（hex 32 字符）+ 只对"有 Message-ID 的邮件"存
  （能覆盖绝大多数导入场景），并按需清理（导入完成后保留 30 天）；`import_dedupe` 归 `cache.db`（可删）。
- 让步：重复检测在"第二次导入同一份存档"时仍能工作（因为 Message-ID 集合仍在 `messages` 表里，
  指纹表只是加速器）——**真正的去重依据是 `messages.message_id`**，不是指纹表。

## 12. 未决问题

| # | 问题 | 影响 | 建议 |
|---|---|---|---|
| M1 | 导入的历史邮件默认是否标记已读 | 未读计数爆炸 | 默认**标记已读**（可关，见反驳 6） |
| M2 | 是否提供"由用户自行放置 pffexport、我们只读输出"的 PST 路径 | Outlook 存档 | M3 评估（零再分发风险）；v1 只给 §5.5 的说明页 |
| M3 | 是否支持 Outlook Express/WinMail 的 `.dbx` | 极小众 | 不支持（文档写明） |
| M4 | 档案上传到服务器的默认策略 | 配额/体验 | 默认不上传；每次显式确认并显示天数 |
| M5 | 设置导入的合并策略默认值 | 覆盖 vs 合并 | 默认"合并，冲突以本机为准"，并给预览 |
| M6 | 是否支持从 Foxmail/网易邮箱大师的目录导入 | 国内用户 | 它们的存档格式非公开且常带自研索引 ⇒ 只支持其"导出为 eml/mbox"后的结果 |

## 13. 验收清单

- [ ] 三条迁移路径（服务器同步 / 本地存档 / 账号+联系人）各跑通一次，且有真机或真实 profile 验证。
- [ ] 导入全程**零网络调用**、源文件 sha256 不变（门禁自动验证）。
- [ ] 5,000 封导入中断后续跑：总数正确、无重复、无丢失。
- [ ] 导入 → 导出 mbox → 再导入：封数/主题/日期完全一致（幂等往返）。
- [ ] GBK/中文表头 CSV、vCard 2.1（QP）能正确导入；列映射界面可用。
- [ ] 本地账号在 UI 上有明确标识，且"上传到服务器"给出配额与天数提示。
- [ ] 备份/恢复：新机器恢复后草稿/规则/联系人在，密码需重登（符合红线 1）。
- [ ] `check.ps1` 五项新增检查通过。
