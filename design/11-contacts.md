# 11 通讯录与自动补全

> **目标**：让"写信时打两个字就出人"成为默认体验；通讯录**自动积累**（不用先建），
> 同时**不偷偷上传**（红线：不收集通讯录到云端）。
>
> 覆盖 ID：CONTACT-01…08、MAIL-10（收件人补全）。
> 事实依据：A1-F12（vCard 2.1 的 QP/CHARSET、75 字节折行、CSV 的 GBK 与**中文表头**）、A1-F19（CardDAV 用 libdav）。
> 上游：03（contacts/contact_emails 表）、04（从同步的邮件里收集）、06（导入导出）。

---

## 1. 目标与非目标

**目标**
1. **自动收集**：从收发邮件的地址里积累联系人（发件人、收件人、抄送），带使用频次与最近时间。
2. **快速补全**：输入 1-2 个字符就出候选；按"最近 + 频繁 + 名字前缀"混合排序；键盘可选。
3. **手动管理**：新增/编辑/删除/分组/备注/头像（本地图片，不上传）；合并重复联系人。
4. **导入导出**：vCard 2.1/3.0/4.0 + CSV（含**列映射**与编码探测）；导出 UTF-8 BOM CSV 与 vCard 3.0。
5. **本地优先**：通讯录在 `user.db`（不可删），**没有任何网络同步**（M3 的 CardDAV 是可选且只读优先）。

**非目标**
- 不做社交/组织架构（那是 LDAP/AD 的事，M3 评估）。
- 不做头像上传到任何服务（Gravatar 之类一律不做，隐私红线）。
- 不做群发营销（规则 10 与写信的"收件人组"只解决小范围）。

## 2. 用户可感知行为

| 场景 | 用户看到 |
|---|---|
| 写信输入 `wang` | 下拉出现"王敏 <wangmin@example.com>（最近 3 天前 · 12 封）"，`↑↓` 选、`Tab`/`Enter` 确认 |
| 输入完整地址（新联系人） | 自动成为联系人（标记"自动收集"），下次就能搜到 |
| 打开联系人 | 详情：姓名/多个邮箱/电话/公司/备注/分组 + "往来邮件 23 封"（点进去是搜索该地址） |
| 头像 | 首次出现首字母色块；用户可上传本地图片（存 `user.db` 外？见 §3 决定） |
| 合并重复 | 通讯录里显示"发现 3 组可能重复" → 一键合并（保留所有邮箱与频次） |
| 导入 vCard | 预览前 5 条 + "导入 412 条（跳过重复 37 条）" |
| 导入 CSV（中文表头） | 出现**列映射**界面（中文表头自动猜 + 可改）+ 编码提示（GBK） |
| 导出 | vCard 3.0 / CSV（UTF-8 BOM）；导出的 CSV 能被 Outlook/Excel 正确打开（中文不乱码） |

## 3. 数据结构

03 已有 `contacts` / `contact_emails`；本模块补充：

```sql
-- 联系人分组
create table if not exists contact_groups (id text primary key, name text not null, color text, sort_order integer not null default 0);
-- 合并记录（撤销用）
-- **第二轮评审修正**：原表只记 id 列表，`merge_undo` 根本重建不出姓名/公司/备注/头像/分组，
-- 也回不去 contact_emails 的地址、标签与计数 ⇒ §9 的"合并→撤销→数据回到合并前"是空承诺。
-- 改成**软删除 + 完整快照**：
create table if not exists contact_merges (
    id integer primary key autoincrement,
    from_ids_json text not null, into_id text not null,
    before_json text not null,     -- 源联系人全部字段 + 受影响的 contact_emails 行 + 目标侧的增量
    merged_at integer not null, undone_at integer
);
-- `contacts.merged_into` / `merged_at`（软删除列）同样**已在 03 §3 定义**，这里不再重复 ALTER。
-- 头像：**存文件而不是 BLOB**（可选）
--   路径：%APPDATA%\com.easyemail.desktop\avatars\<contact_id>.<ext>；contacts.avatar_path 记录
```

```rust
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Contact {
    pub id: String,
    pub display_name: String,
    pub emails: Vec<ContactEmail>,            // 多邮箱
    pub phones: Vec<String>, pub org: Option<String>, pub title: Option<String>, pub note: Option<String>,
    pub groups: Vec<String>,                  // group id
    pub avatar_path: Option<String>,
    pub source: ContactSource,                // Manual | Collected | Imported | CardDav
    pub created_at: i64, pub updated_at: i64,
}
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ContactEmail { pub addr: String, pub label: Option<String>, pub sent_count: u32, pub recv_count: u32, pub last_used_at: Option<i64> }
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum ContactSource { Manual, Collected, Imported, CardDav }

#[derive(Serialize, Clone, Debug)]
pub struct ContactHit { pub contact_id: String, pub name: Option<String>, pub addr: String,
                        pub score: i32, pub reason: HitReason, pub group: Option<String> }
#[derive(Serialize, Clone, Copy, Debug)] pub enum HitReason { NamePrefix, AddrPrefix, Recent, Frequent, InGroup }
```

## 4. 接口（函数级）

```rust
// ── 收集（从同步落库的邮件里）────────────────────────────────
/// 从一封邮件里收集地址（发件人 + 收件人 + 抄送，排除自己账号的地址）
pub async fn collect_from_message(db: &Db, account_id: &str, m: &MessageRow, opts: &CollectOptions) -> Result<u32>;
#[derive(Clone, Debug)] pub struct CollectOptions { pub include_cc: bool, pub ignore_roles: bool /* noreply@ 等 */, pub only_known_domains: bool }
/// 批量（04 的 commit_page 之后调用，一次事务；有节流：同一地址 5 分钟内不重复计数）
pub async fn collect_batch(db: &Db, account_id: &str, rows: &[MessageRow]) -> Result<u32>;

// ── 查询与补全 ───────────────────────────────────────────────
/// 模糊搜索（名字/地址/公司/备注），分页
pub async fn search(db: &Db, q: &str, limit: u32, offset: u32) -> Result<Vec<ContactHit>>;
/// 自动补全（**延迟敏感**：目标 ≤ 20 ms @ 10 万地址）
pub async fn autocomplete(db: &Db, prefix: &str, limit: u32, ctx: &AcContext) -> Result<Vec<ContactHit>>;
pub struct AcContext { pub exclude: Vec<String> /* 已在收件人里的 */, pub prefer_group: Option<String> }
/// 排序分数（纯函数，可单测）：名字前缀 > 地址前缀 > 最近使用 > 使用频次
pub fn score_hit(prefix: &str, c: &ContactEmail, name: Option<&str>, now: i64) -> i32;

// ── 管理 ─────────────────────────────────────────────────────
pub async fn upsert(db: &Db, c: &Contact) -> Result<String>;
pub async fn delete(db: &Db, id: &str) -> Result<()>;
pub async fn merge(db: &Db, from: &[String], into: &str) -> Result<MergeReport>;
pub async fn merge_undo(db: &Db, merge_id: i64) -> Result<()>;
pub async fn duplicates(db: &Db) -> Result<Vec<DuplicateGroup>>;      // 同名/相似地址/同域相似名
pub async fn set_avatar(db: &Db, id: &str, src: &Path) -> Result<()>; // 复制进 avatars/，限制尺寸（≤ 256×256）

// ── 导入导出 ─────────────────────────────────────────────────
pub fn parse_vcards(bytes: &[u8]) -> Result<Vec<Contact>>;            // calcard（A1-F12）
pub fn export_vcards(db: &Db, ids: Option<&[String]>) -> Result<Vec<u8>>;
pub struct CsvPreview { pub headers: Vec<String>, pub rows: Vec<Vec<String>>, pub encoding: String, pub guessed_mapping: ColumnMapping, pub row_count: u32 }
pub fn preview_csv(bytes: &[u8], encoding: Option<&str>) -> Result<CsvPreview>;
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct ColumnMapping { pub name: Option<usize>, pub email: Option<usize>, pub phone: Option<usize>,
                           pub org: Option<usize>, pub title: Option<usize>, pub note: Option<usize>, pub groups: Option<usize> }
pub async fn import_csv(db: &Db, bytes: &[u8], m: &ColumnMapping, opts: &ImportOpts) -> Result<ContactImportReport>;
pub async fn export_csv(db: &Db, opts: &CsvExportOpts) -> Result<Vec<u8>>;   // UTF-8 BOM；表头英文（兼容）

// ── 与写信的接口（07 用）────────────────────────────────────
pub async fn recent_recipients(db: &Db, limit: u32) -> Result<Vec<ContactHit>>;
pub fn validate_address(addr: &str) -> AddressVerdict;
pub enum AddressVerdict { Valid, MissingAt, BadDomain, DomainTypo(String), NonAscii, Empty }
```

## 5. 主流程

### 5.1 自动收集（不打扰、不膨胀）
```
04 落库一批头部 → collect_batch
  地址来源：from_addr（权重 2）+ to/cc（权重 1）
  过滤：自己的账号地址、noreply/no-reply/mailer-daemon/postmaster/bounce、纯 IP 域、明显机器地址
  计数：contact_emails.sent_count / recv_count 累加；last_used_at 更新（**5 分钟节流**，避免刷榜）
  容量：地址总量上限（默认 50,000）；超限时**只允许淘汰"自动收集（source=Collected）且 sent+recv ≤ 1、180 天未再出现"的地址**
  —— **第二轮评审**：原来的规则只看出现次数，会把用户导入或手填、但只往来过一次的老联系人删掉（地址簿里的数据丢失）。
  淘汰必须**报数并给样本**，且永不淘汰手填/导入/被收藏的联系人
```

### 5.2 自动补全（延迟是硬指标）
```
输入 → 防抖 80 ms → autocomplete(prefix)
  查询：contact_emails 按 lower(email) 前缀 + contacts.display_name 前缀（两条索引查询）
       + 最近使用（last_used_at desc, limit 10）+ 频繁（count desc, limit 10）→ 合并去重 → 打分排序
  返回 ≤ 8 条；UI 显示"名字 · 地址 · 依据（最近/12 封）"
键盘：↑↓ 选择、Enter/Tab 确认、逗号/分号 分隔多收件人、Esc 关闭
```

### 5.3 导入（vCard/CSV）
```
vCard：calcard 解析（处理 2.1 的 QP+CHARSET、75 字节折行、3.0↔4.0 的 TYPE 差异）
CSV：①猜编码（UTF-8 → GBK/GB18030 回退；BOM 检测）②解析（RFC 4180）③**列映射 UI**（默认猜，可改）
    ④预览前 5 条 ⑤导入（按 email 去重；已存在的合并计数，不覆盖用户手填的字段）
结果报告：新增/合并/跳过（重复）/失败（含原因）
```

## 6. 失败模式与降级

| 触发 | 用户看到 | 处理 |
|---|---|---|
| 空间不足（50,000 地址上限） | 设置里显示"通讯录已达上限（可清理最久未用的）" | 按 §5.1 的策略自动收缩 + 提示 |
| 导入的 vCard 解析失败 | "412 条中 8 条无法解析（示例…）" | 跳过并报告（不中止） |
| CSV 编码猜错 | 预览里可见乱码 → 手动切换编码 | 提供编码下拉（UTF-8/GBK/GB18030/BIG5） |
| 列映射漏了邮箱列 | 导入前报错并指出必须映射邮箱 | 阻止导入（不是导入一堆空联系人） |
| 自动收集把机器地址当人 | 通讯录里出现 `noreply@` | 过滤规则 + 用户可"永不再收集此地址" |
| 头像文件被删 | 回退到首字母色块 | 无感 |
| 合并出错 | 合并记录可撤销（`contact_merges`） | 一键撤销 |
| 补全很慢（远处地址过多） | 无感（有 limit 与索引） | 性能基准与门禁保证 |

## 7. 迁移与兼容

- 03 的 v3 迁移建 `contacts`/`contact_emails`（v0.2 无通讯录，无迁移负担）。
- 头像存文件而非 BLOB（避免 `user.db` 膨胀）；路径相对 `app_data/avatars`。
- 导出格式固定（vCard 3.0 / CSV UTF-8 BOM + 英文字头），**只增不改**（06 §7 的纪律同样适用）。
- 与 06 的接口：导入/导出/备份都通过本模块的函数，不重复实现解析。

## 8. 性能预算

| 指标 | 预算 |
|---|---|
| 补全（10 万地址） | ≤ 20 ms（P95 ≤ 35 ms） |
| 搜索联系人（10 万） | ≤ 40 ms |
| 从一批 500 封邮件收集 | ≤ 200 ms（一个事务） |
| vCard 导入 5,000 条 | ≤ 3 s |
| CSV 预览（1 万行） | ≤ 1 s（只解析前 50 行 + 计数） |
| 头像处理（≤ 5 MB 图片） | ≤ 300 ms（缩放到 256×256，存 WebP/PNG） |

## 9. 测试计划

| 层 | 用例 |
|---|---|
| 单元 | `score_hit` 排序（名字前缀/地址前缀/最近/频繁四类组合）；`validate_address`（含 `gmial.com` 的域名疑似）；`preview_csv` 的编码与引号边界；收集过滤规则（noreply 等 12 种） |
| 夹具 | vCard 2.1（QP + CHARSET）/3.0/4.0；CSV（UTF-8 BOM、GBK、**中文表头**、引号内逗号、换行字段） |
| 集成 | 导入 → 补全 → 写信用它发一封 → 计数增加（sent_count）；合并 → 撤销 → 数据回到合并前 |
| 性能 | §8 六项纳入 `tools/bench.ps1`（10 万地址夹具） |
| 真机 | 用真实通讯录导出（脱敏）→ 导入 → 补全体验；导出的 CSV 在 Excel/Outlook 打开中文不乱码 |
| 隐私 | 断言：通讯录相关代码**零网络调用**（假连接池计数）；头像不上传 |

## 10. 门禁新增项

1. **通讯录零网络**：集成测试断言（假连接池计数为 0）。
2. **CSV 必须列映射**：`import_csv` 的类型上要求 `ColumnMapping`（结构体字段强制）且邮箱列必填。
3. **不收集自己的地址**：单测断言账号自身地址不进入 `contact_emails`。
4. **上限保护**：单测覆盖"超过上限时的收缩策略"（不无限增长）。
5. **头像不入库**：`contacts.avatar_path` 只能是文件路径（不得存 base64/BLOB）。

## 11. 逆向反驳（红队）

**反驳 1：自动收集通讯录 = 未经同意建立"联系人数据库"，隐私上有争议（且会让用户觉得被监视）。**
- 缓解：①**只在本机**（零网络，门禁保证）；②设置里可关闭自动收集（默认开，但在隐私说明里写明）；
  ③**只收集地址与显示名**，不收集正文/主题；④提供"清空自动收集的联系人"一键；
  ⑤UI 上明确区分"自动收集"与"我手动添加的"（`source` 字段可视化）。
- 让步：首次运行时用一句话征求同意（"要不要自动记住往来过的地址，方便下次写信？"），默认勾选但可取消。

**反驳 2：补全要求 20 ms，但"最近 + 频繁 + 前缀"三路合并可能超预算。**
- 缓解：①三条查询都走索引且 limit 10；②合并与打分是纯内存（≤ 30 条）；③若超标，
  退化为"只做前缀查询（名字+地址），最近/频繁只用于排序不加候选"。
- 让步：把 `limit` 从 8 降到 6（视觉上也更清爽）。

**反驳 3：卡片式头像（本地图片）会不会成为隐私与体积负担？**
- 缓解：①默认**只有首字母色块**，上传头像是可选项；②缩放到 256×256 并存文件（不入库）；
  ③备份时可选是否包含头像（`06` 的备份选项）。
- 让步：若实测上传率极低，M3 直接移除头像上传（保留色块）。

**反驳 4：联系人合并（去重）很容易错（把两个人合成一个）。**
- 缓解：①只对"同地址"与"完全同名 + 同域"给建议，**不自动合并**；②合并前展示将要合并的字段对照；
  ③合并可撤销（`contact_merges`）；④**永远不删除**被合并方的地址（都保留）。
- 让步：不提供"自动合并全部"的一键（危险）。

**反驳 5：不做 CardDAV 同步，用户会觉得"通讯录只在这台电脑上"，与手机不一致。**
- 证据：A1-F19 显示 CardDAV 的 principal 发现各家不同、iCloud 还只能用应用专用密码。
- 缓解：①M3 做 **CardDAV 只读拉取**（手机加的人能出现在补全里）；②导出 vCard 让用户手动同步到手机；
  ③文档明确说明现状（不装作已经同步）。
- 让步：若用户量大，M3 末做双向（带 ETag 冲突提示）。

## 12. 未决问题

| # | 问题 | 影响 | 建议 |
|---|---|---|---|
| G1 | 自动收集默认开还是关 | 隐私 vs 便利 | 默认**开**并在首次运行征得同意（可关） |
| G2 | 地址总量上限（50,000？） | 性能/磁盘 | 先 50,000，实测后调 |
| G3 | 头像上传是否保留 | 复杂度/隐私 | 保留但默认不用 |
| G4 | CardDAV 只读何时做 | 多端一致 | M3（只读优先） |
| G5 | 是否支持"收件人组"群发 | 便利 | M2 做（组 = 保存的地址集合，不做邮件合并） |
| G6 | 导出 CSV 表头用中文还是英文 | 兼容性 | 英文（Excel/Outlook 兼容更好），备注列给中文说明 |

## 13. 验收清单

- [ ] 10 万地址夹具：补全 P95 ≤ 35 ms、搜索 ≤ 40 ms、收集 500 封 ≤ 200 ms。
- [ ] 自动收集过滤掉 12 类机器地址；自己的账号地址不进通讯录（单测）。
- [ ] vCard 2.1（QP+CHARSET）与 3.0/4.0 都能导入；CSV 的中文表头 + GBK 走列映射成功导入。
- [ ] 补全在写信界面可用（键盘全流程），且有"最近/频繁"依据提示。
- [ ] 合并可撤销；导入导出往返一致；导出的 CSV 在 Excel 中文正常。
- [ ] 通讯录相关代码零网络调用（门禁）；头像不入库。
- [ ] `check.ps1` 五项新增检查通过。

> **章节名对照（模板 13 节）**：状态机→§5 主流程。
> 内容齐全，只是节名与模板不完全同名（见 design/README.md 的说明）。
