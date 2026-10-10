# 09 搜索与索引

> **目标**：10 万封邮件里，**中文两个字就能搜到**、英文子串能搜到、输入即出结果（≤ 100 ms），
> 并且索引坏了能重建、能暂停、不阻塞阅读。
>
> 覆盖 ID：SRCH-01…10。
> 事实依据：A1-F43（`UID SEARCH` 复杂条件可以透传 ⇒ 服务器端回退可行）、A1-F46（mail-parser 的 `body_text()` 内置 HTML→text）。
> **待并入**：分词器方案的第三方研究仍在进行（A1 §七）。本文按下面的纪律处理：
> **设计的骨架（队列/表/语法/重建/回退）与分词器无关；分词器是一个"用基准测量来决定"的开关**（§4.2）。

**实现状态（2026-10-10，第 71 轮后）** —— 目标 1、2 的一部分已经落地，其余仍是纸面：

| 目标 | 状态 |
|---|---|
| 1. 中文 2 字词 100% 命中（逐字空格化 + `unicode61`，§4.2 已定案并钉进测试） | ✅ 已实现并真机验证（QQ 收信） |
| 2. 覆盖：主题/发件人/收件人 + **正文**（后台有界回填，v0.1.2）+ **附件名**（第 71 轮） | ✅ 两项都实现；**附件文本按 Q3 默认关**（M2 的开关） |
| 3. 输入即出 ≤ 100 ms（10 万封） | ⚠️ 10 万封未实测；20 000 封基准在 `design/15` 与 `bench_fixture`，查询形状已按实测改过（见 `cache.rs::search` 的注释） |
| 4. 查询语法（`from:`/`has:attachment`/`filename:` 等） | ❌ 只有裸词；语法属 M1b/M2 |
| 5. 可重建/可暂停/可取消（`index_queue`、重建 UI） | ⚠️ **重建 UI 第 72 轮已做**（分批、可续跑、可停止、二次确认）；`index_queue` 队列与 FTS 损坏的**自动**探测/heal 仍未做（M2） |
| 6. 服务器回退（`UID SEARCH`） | ❌ 未做 |
| 7. 保存搜索/虚拟文件夹 | ❌ M2 |

> **一处有意偏离**：`design/20-v0.2-plan.md` C1/C2 与本节 §5.4 原本设想 `index_queue` 表；
> 实现（v0.1.2）用的是**每遍 20 封、60 秒一遍的有界回填**，同步路径一行未改。
> 第 71 轮沿用同一形状，把"索引形状升级"也做成同一遍里的本地补齐（`index_version`），
> 没有引入队列——理由与 C2 相同：队列要自己回答优先级与取消，而当前的活儿形状简单到不值得。

---

## 1. 目标与非目标

**目标**
1. **中文 2 字词 100% 可命中**（这是国内用户的底线，既有实现已做到，不许退化）。
2. **覆盖范围明确**：主题/发件人/收件人/抄送 + **正文**（同步到的邮件）+ 附件名；附件正文文本可选。
3. **输入即出**：本地查询 ≤ 100 ms（10 万封）；不因索引落后而卡住（部分结果 + 明确提示）。
4. **查询语法**：`from:`/`to:`/`cc:`/`subject:`/`body:`/`has:attachment`/`filename:`/`before:`/`after:`/`is:unread`/`is:flagged`/`larger:`/`smaller:`/`in:`/`account:`。
5. **可重建**：索引是派生物，删了能重建，进度可见、可暂停、可取消（索引队列来自 04）。
6. **服务器回退**：本地没命中且该文件夹未全量同步时，能在服务器上搜一次（`UID SEARCH`，A1-F43 ✅）。
7. **保存搜索/虚拟文件夹**：把常用查询变成侧栏入口（SRCH-05）。

**非目标**
- 不做语义/模糊搜索（"找那封说发票的邮件"这种自然语言检索）—— M3+ 再评估。
- 不做服务端全文索引（服务商不提供 IMAP 全文索引；回退只是 `SEARCH` 条件查询）。
- 不索引附件二进制内容之外的东西（不做 OCR）。

## 2. 用户可感知行为

| 场景 | 用户看到 |
|---|---|
| 输入"账单" | 立刻出结果（含正文命中），命中的字在结果里**高亮**；右上角"共 37 条" |
| 输入 `from:银行 has:attachment after:2026-01-01 发票` | 结果精确；查询语法有错时给"语法提示"而不是空结果 |
| 输入什么都没搜到 | 明确说明覆盖范围："本地已索引 12,431/12,500 封（正文只覆盖已同步的）" + 按钮`在服务器上继续搜索` |
| 索引还没建完 | 结果上方一行："正在索引（62%），结果可能不全" |
| 想看某个常用查询 | `保存为"待报销"` → 侧栏出现"待报销"，点它等于执行该查询 |
| 磁盘紧张 | 设置 → 存储 → 索引："正文索引 210 MB（可清理，清理后正文搜索范围缩小）"；附件文本索引可单独关 |
| 搜索很慢（异常） | 设置 → 健康 → 显示最近的慢查询（SQL + 耗时），可一键重建索引 |

## 3. 数据结构

**索引表结构已按实测结论定案**（A1-F47）：**external-content FTS5**（索引不存文本副本）
+ `detail=column`（保留短语、列过滤与 `snippet()`；代价是没有 `NEAR`）+ `columnsize=0`。

```sql
-- 1) 索引源表：每封邮件一行"已分词文本"（由 Rust 的 to_index_text 生成）
--    为什么单开一张表：external-content FTS5 需要一个真实表作为内容源，
--    而我们的"已分词文本"与 messages 里的原始文本不同（分词在 Rust 侧做）。
create table if not exists search_text (
    id            integer primary key,       -- **稳定 rowid**：见下（不是自增主键的偶然值）
    account_id    text not null, folder text not null, uid integer not null,
    subject       text not null default '',
    from_name     text not null default '', from_addr text not null default '',
    to_addrs      text not null default '', cc_addrs text not null default '',
    body          text not null default '',
    attachment_names text not null default '',
    unique(account_id, folder, uid)
);
-- id 的取值：**sha1(account_id|folder|uid) 的低 63 位**（稳定、无需查表、删除可按 rowid 直删）
-- 理由：FTS5 的 delete 若按 unindexed 列过滤（旧写法 `where account_id=? and folder=? and uid=?`）
--       实际是全表扫描（实测过），按 rowid 删才是索引查找。

-- 2) FTS5 索引（external content）
create virtual table if not exists search using fts5(
    subject, from_name, from_addr, to_addrs, cc_addrs, body, attachment_names,
    content      = 'search_text',
    content_rowid = 'id',
    columnsize   = 0,          -- 只失去 bm25 的长度归一化；省体积
    detail       = 'column',
    tokenize     = 'unicode61' -- Rust 侧已把 token 之间插好空格；见 §4.2
);
-- pragma：automerge 提到 8..16（默认 4）；空闲时跑有界 `merge 256`；
--        **`optimize` 只在用户点"重建索引"时跑**（实测它耗时 7.26 s 且并不缩小文件）。

-- 搜索历史与保存搜索
create table if not exists search_history (q text primary key, used_at integer not null, uses integer not null default 1);
create table if not exists saved_searches (
    id text primary key, name text not null, query text not null, icon text,
    sort_order integer not null default 0, created_at integer not null
);

-- 附件文本抽取：**不单开表**，复用 03 的 index_queue(kind='attachment_text') 与
-- attachments.text_state（单一来源，避免两处状态不一致）
```

```rust
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct SearchRequest {
    pub account_id: Option<String>,      // None = 所有账号
    pub query: String,                   // 用户输入（可含语法）
    pub folder: Option<String>,
    pub offset: u32, pub limit: u32,     // 分页
    pub sort: SearchSort,                // Relevance | DateDesc | DateAsc | SizeDesc
    pub include_body: bool,              // 默认 true（用户可关以加速）
}
#[derive(Serialize, Deserialize, Clone, Copy, Debug)] pub enum SearchSort { Relevance, DateDesc, DateAsc, SizeDesc }

#[derive(Serialize, Clone, Debug)]
pub struct SearchResponse {
    pub hits: Vec<SearchHit>, pub total: u32,
    /// 结果是否可能不全（索引落后/仅部分文件夹已索引/正文未索引）
    pub coverage: SearchCoverage,
    /// 解析后的查询（用于 UI 显示"你搜的是什么"与高亮）
    pub parsed: ParsedQuery,
    /// 语法错误（不阻塞搜索：能识别的部分照常执行）
    pub issues: Vec<QueryIssue>,
    pub elapsed_ms: u32,
}
#[derive(Serialize, Clone, Debug)]
pub struct SearchCoverage { pub indexed_messages: u64, pub total_messages: u64, pub index_pending: u64,
                           pub body_indexed: u64, pub attachments_indexed: u64, pub truncated: bool }
#[derive(Serialize, Clone, Debug)]
pub struct SearchHit { pub account_id: String, pub folder: String, pub uid: u32, pub thread_id: Option<String>,
                       pub subject: String, pub from_name: Option<String>, pub from_addr: Option<String>,
                       pub date: Option<i64>, pub seen: bool, pub has_attachments: bool,
                       /// 高亮片段（已按 <em> 标注；前端只允许渲染这几个固定标签）
                       pub snippets: Vec<HighlightSpan> }
#[derive(Serialize, Clone, Debug)]
pub struct HighlightSpan { pub field: HighlightField, pub text: String, pub highlights: Vec<(u32, u32)> }
#[derive(Serialize, Clone, Copy, Debug)] pub enum HighlightField { Subject, Body, AttachmentName, From }

#[derive(Serialize, Clone, Debug, Default)]
pub struct ParsedQuery {
    pub free_text: Vec<String>,
    pub terms: Vec<QueryTerm>,           // 结构化条件
    pub raw: String,
}
#[derive(Serialize, Clone, Debug)]
pub enum QueryTerm {
    From(String), To(String), Cc(String), Subject(String), Body(String), Filename(String),
    HasAttachment(bool), IsUnread(bool), IsFlagged(bool), IsReplied(bool),
    Before(i64), After(i64), Larger(u64), Smaller(u64), InFolder(String), Account(String),
    Label(String), Thread(bool),   // thread:true 表示"按会话聚合显示"
}
#[derive(Serialize, Clone, Debug)] pub struct QueryIssue { pub span: (u32, u32), pub message: String, pub suggestion: Option<String> }
```

## 4. 接口（函数级）

```rust
// ── 查询 ─────────────────────────────────────────────────────
/// 解析用户输入 → 结构化查询（**不允许把用户输入直接拼进 SQL**）
pub fn parse_query(input: &str) -> Result<ParsedQuery>;
/// 把结构化查询翻译成"SQL 条件 + FTS 表达式"（参数化，绝不字符串拼接值）
pub fn compile(q: &ParsedQuery, scope: &SearchScope) -> CompiledQuery;
pub struct CompiledQuery { pub fts_expression: Option<String>, pub sql_where: String, pub params: Vec<SqlValue>, pub order_by: String }
/// 执行搜索（读池；只读事务）
pub async fn search(db: &Db, req: &SearchRequest) -> Result<SearchResponse>;
/// 搜索建议（历史 + 联系人 + 语法提示）
pub async fn suggest(db: &Db, prefix: &str, limit: u32) -> Result<Vec<Suggestion>>;
pub enum Suggestion { History(String), SavedSearch(String), Contact { name: String, addr: String }, Operator(&'static str) }

// ── 索引（写入侧）────────────────────────────────────────────
/// 索引侧的分词（**与查询侧必须同源**：单测钉住）
pub fn to_index_text(s: &str) -> String;
/// 查询侧把一段文本转成 FTS5 MATCH 里的短语
pub fn to_match_phrase(s: &str) -> String;
/// 一行的索引内容（去掉 HTML、截断超长正文、抽取附件名）
pub fn build_index_row(m: &MessageRow, body: Option<&BodyRow>, atts: &[AttachmentRow]) -> IndexRow;
pub async fn index_batch(db: &Db, rows: Vec<IndexRow>) -> Result<u32>;      // 删+插同一事务
pub async fn rebuild(db: &Db, scope: IndexScope, job: &JobCtx) -> Result<RebuildReport>;
pub async fn optimize(db: &Db) -> Result<()>;
pub async fn stats(db: &Db) -> Result<IndexHealth>;
pub async fn consistency(db: &Db) -> Result<IndexConsistency>;              // 与 messages 对账

// ── 附件文本（可选，默认只索引"文件名"，文本抽取是 M2 的开关）──────
pub fn extract_text(mime: &str, name: &str, bytes: &[u8]) -> TextExtraction;
pub enum TextExtraction { Text(String), Unsupported(&'static str), TooLarge { size: u64, limit: u64 }, Failed(String) }
pub async fn attachment_text_job(ctx: &AppCtx, job: &JobCtx) -> Result<AttachmentTextReport>;

// ── 服务器回退（本地无命中且文件夹未全量同步）────────────────
pub async fn search_server(ctx: &AppCtx, account_id: &str, folder: &str, q: &ParsedQuery, limit: u32) -> Result<Vec<u32>>;
/// 把我们的语法映射到 IMAP SEARCH 条件（字符串透传，A1-F43 ✅）
pub fn to_imap_criteria(q: &ParsedQuery) -> String;

// ── 保存搜索 ─────────────────────────────────────────────────
pub async fn saved_list(db: &Db) -> Result<Vec<SavedSearch>>;
pub async fn saved_save(db: &Db, s: &SavedSearch) -> Result<()>;
pub async fn saved_delete(db: &Db, id: &str) -> Result<()>;
pub async fn history_push(db: &Db, q: &str) -> Result<()>;
```

## 4.2 分词器：**已用实测定案 —— CJK 感知的 bigram**（证据见 `A1-F47`）

**背景**：FTS5 默认 `unicode61` 把连续汉字当成一个 token ⇒ 搜"账单"命中 0（实测：`开发票` 只索引出一个 token，
查 `"发票"` 返回 0 行）；官方推荐的 `trigram` 需要 ≥ 3 字符 ⇒ **两个字的中文词搜不到**
（实测 100k 中文语料上 `MATCH '"发票"'` 命中 **0 行**，同一语料 bigram 命中 200 行）。
`unicode61` 的 `tokenchars`/`separators`/`categories` **都救不了**：汉字按 Unicode 类别就是 token 字符。

**实测发现既有实现（逐字符空格化）有真实缺陷** —— 这是改写的直接原因：

| 索引方式 | 查询 | 命中 | 误报 |
|---|---|---|---|
| 逐字符空格化（现状） | `"发 票"` | `[0,1,3,4,6]` | **`[0,6]`**：文档 0 是 `已经 收到 发\n票 了`（换行夹在中间），文档 6 是 `developer 发 票 test` |
| **CJK bigram** | `"发票"` / 裸词 `发票` | `[1,3,4]` | **0** |
| 裸 `unicode61` | `"发票"` | `[1,4]` | 0，但**漏了 `3`**（`这个 发票号码 是…`） |

即：**只要两个字之间夹着分隔字节（换行、空格、HTML 标签边界），逐字符方案就误报** ——
而 HTML 正文里两个相邻汉字之间夹换行/标签是常态。**bigram 同时更准、更小**：

| 方案 | 建索引（20k 消息 / 40.8 MiB 文本） | 索引体积 | 2 字短语查询 |
|---|---|---|---|
| 逐字符 | 5.1 s（3891 msg/s） | **126.9%** 文本量（且 ASCII 被逐字母拆开，字节量翻倍） | 0.04 ms → **0 命中**（词被拆坏） |
| **bigram** | 19.3 s（1036 msg/s） | 139.6%；**`detail=column` + ASCII 不拆 ⇒ 65.1%**（比现状更小） | 0.27 ms，200 命中 |
| trigram | 10.3 s | **412.9%** | **0 命中** |

**定案（改设计）**：`to_index_text` 与 `to_match_phrase` 共用**同一个** CJK 感知切分函数：
```
遍历字符：
  连续 CJK（Han / Hiragana / Katakana / Hangul）游程：
      长度 1 → 输出该单字（unigram，保证单字可搜）
      长度 ≥2 → 输出全部重叠二元组（发票已开 → 发票 票已 已开）
  非 CJK 游程：**原样输出、不拆**（alice@example.com、invoice2024 保持完整）
```
**因为 FTS5 对 `MATCH` 右侧也走分词器，两字查询既不需要引号也不需要特判** ——
这从机制上消掉了"索引侧与查询侧必须同步"的脆弱性（原来只能靠一个单测钉住）。
**不启用 porter 词干**：它只服务英文，且与 CJK 混排会互相污染（Mozilla 当年的原话见 A1-F47）。

> **可选升级（M1 不做，代码留 TODO）**：把切分做成真正的 FTS5 tokenizer（`tokenize='cjk_bigram'`）。
> 本地 `libsqlite3-sys 0.38.2` 的 bundled SQLite 3.53.2 已经 **生成了** `fts5_tokenizer`/`fts5_api`
> 的绑定（`xCreateTokenizer`/`xFindTokenizer`/`sqlite3_bind_pointer` 都在），需要约 150-250 行
> `unsafe` 胶水（含 `catch_unwind`，绝不能让 Rust panic 穿过 C 栈）。收益是"DDL 里写死、永远不会忘"；
> 代价是 FFI 维护。**参考实现**：Mozilla 的 `mozporter`（16 年生产验证，源码注释写明"supports CJK
> indexing using bi-gram"）。

**参考事实**：Thunderbird/Gloda 走的正是这条路（Bug 472764 的附件就叫 "bigram tokenizer"；
Bug 515560 记录了 CJK 搜索返回零结果的历史）；Windows Search 的官方口径是"索引约为内容的 10%"、
40 万条以上开始退化；Exchange 的附件索引默认上限 **32 MB** 且带格式白名单 —— 我们的附件策略照它办（§5.1）。


## 5. 主流程

### 5.1 索引队列（写入侧，与 04 对接）
```
同步/导入落库 → 同一事务里 upsert index_queue(kind='body', priority=…)
  优先级：用户刚打开过的邮件 = 10；收件箱最新 = 50；其它文件夹 = 100；导入的历史 = 150
indexer 任务（**专用线程**，全局 1，可暂停，CPU 低优先级）：
  取一批（**500-2000 封/事务**；实测 100k 规模下批量建索引 ≈ 510 msg/s，
   逐封一事务会掉到 ~650 msg/s 且段数暴涨）
    → build_index_row（HTML→text 用 mail-parser 的 body_text()，A1-F46；有 text/plain 就优先用它，
       不做 readability 式正文提取 —— 邮件 HTML 是表格结构，readability 会**丢真实正文**）
    → **写 search_text（upsert，rowid = sha1(account|folder|uid) 低位）+ 同步维护 FTS**：
       external-content 表要用 `insert into search(search, rowid, …) values('delete', …)` **带旧文本**
       再插新行；否则索引会不可预测（实测：用'delete'+insert 删除才能正确清掉词条）
    → 更新 messages.indexed_at + 发 index:status 事件（节流 1 s）
  FTS 维护参数：automerge 8..16；空闲跑有界 `merge 256`（实测 ~0 s，固定开销）；
    **`optimize` 只在用户点"重建索引"时跑**（实测 7.26 s 且不缩小文件）
附件文本（**默认开，带硬上限**，照 Exchange 的行业惯例 A1-F47）：
  仅这些格式：pdf docx xlsx pptx txt csv html rtf md；
  单个 ≤ 25 MB、PDF ≤ 200 页；压缩包**不递归**；
  抽取跑在**独立 worker 线程**（`catch_unwind` 兜住 panic；若走 pdfium 路线要考虑它 segfault 会带走进程 ⇒ 首选 `pdf-extract`）
  文本编码：chardetng + encoding_rs（中文 .txt/.csv 常是 GB18030/GBK，猜错就搜不到）
  结果追加进同一 search_text 行的 body/attachment_names
```

### 5.2 查询执行
```
parse_query → 若有语法错：能识别的部分照常执行 + issues 提示
compile → 两条路径：
  ① 有 free_text/body/subject 等 → FTS5 MATCH（JOIN messages 取元数据与排序）
     · 文本类条件推给 FTS5：subject:/from:/to: 用**列过滤**（`subject : 发票`、`{from_name from_addr} : 张三`）
     · 元数据类条件留在 messages 表的普通 SQL：has:/filename:/is:/larger:/in:/account:/before:/after:
       （**不要**塞进 MATCH 表达式）
  ② 只有结构化条件（如 is:unread after:…）→ 纯 SQL（走 messages 的索引），不必碰 FTS
ORDER BY：**默认 DateDesc（date desc, uid desc）—— 这是实测出来的关键决定**：
   `ORDER BY rank` 在常见词上要 130-143 ms（OR 查询 218 ms），**单这一项就会吃掉 100 ms 预算**；
   而 `ORDER BY rowid DESC` 只要 0.02 ms。所以：
   · 默认不排序于相关性；相关**只作为可选项**，且必须用**两阶段**：先取有界 rowid 窗口
     （如最新 5,000 条命中），再在窗口内按 rank 排 —— 实测 1.2-1.6 ms（比朴素 rank 快约 100×），
     而且更符合直觉（搜"发票"的人要的是**最近的**发票，不是全局 BM25 最优）
分页：**keyset（`MATCH ? AND rowid > ? LIMIT n`，实测 0.10 ms）**；禁止深 OFFSET
高亮：**用 SQL 里的 `snippet()`/`highlight()`**（实测 20 行 2.9-4.7 ms，且偏移量由同一个分词器保证正确）；
      只有需要自定义标记（逐词着色）时才在 Rust 里重算偏移
覆盖度：coverage 用 messages.indexed_at 与 index_queue 深度算出来，随结果一起返回
```

### 5.3 服务器回退（本地没命中时）
```
若 hits.is_empty() && coverage.truncated（该文件夹没全量同步）
  → 显示按钮"在服务器上继续搜索"
  → 用户点 → search_server：对已同步文件夹逐个 SELECT + UID SEARCH <criteria>（A1-F43：复杂条件可透传）
       ├ 命中 → 取头部（区间取件）→ 落库（这些邮件会被索引）
       └ 服务器不支持该条件（BAD）→ 明确提示"这个邮箱不支持该查询条件"（**注意：BAD 不会关连接，但未知响应会**）
  限额：一次最多 3 个文件夹、每个 ≤ 500 命中，避免把服务器惹毛（04 §5.7 的配额账本同样生效）
```

### 5.4 重建与一致性

> **实现注记（第 72 轮）**：用户触发的重建是**分步**的（`Cache::rebuild_search_step`，默认 500 行/步，
> 渲染层驱动、步间让出事件循环），不是"一个事务里重建完"。两条理由：
> ① 本节自己写的"绝不在启动事务里做"同样适用于用户触发——单个写事务重建 10 万行会把
> 其它所有命令堵在写线程后面；② 分步才谈得上"可停止、可续跑"。
> **进度不靠内存变量**：`done = count(messages.search_rowid is not null)`，所以进程死掉、
> 用户关窗、机器重启之后进度仍然是真的，接着做完即可——这是"可续跑"的全部实现，没有额外状态。
> 与迁移里那个一次性的 `rebuild_search_index(conn)`（v6 迁移专用，发生在 `migrate` 内部）
> **刻意保持两份**：用途不同，不要"顺手合并"。
> 仍未做：`index_queue` 队列、FTS 损坏的自动探测与 heal（`design/03`）、重建期间的并发读快照
> （§5.4 里的 UNION 方案，M1 可停用搜索并提示进度——当前实现正是这样：进度可见、期间搜索
> 只覆盖已重建的部分）。
```
rebuild(scope) ：
  1. 分词方案/表结构变化**无法原地迁移**（词表不兼容）⇒ 必须：
     · 升 SCHEMA_VERSION
     · 新建 search_text + search（或先建 search_new）
     · 后台分批重建（500-2000 封/事务），**绝不在启动事务里做**
     · 重建期间要能继续搜：旧表保留，按 (account_id, folder, uid) UNION 直到新表追上（可选，M1 可停用搜索并提示进度）
  2. 进度 = messages 计数；可暂停/取消（取消点只在批次边界）
  3. 完成后 consistency 对账（行数 + 抽样词条），并跑一次有界 merge
夜检（16 §4.2）：discover 差异 → 只重建差异部分（不整表重建）
```

## 6. 失败模式与降级

| 触发 | 用户看到 | 处理 |
|---|---|---|
| 索引落后 | 结果上方一行"正在索引（62%），结果可能不全" | coverage 明确；不阻塞查询 |
| 正文未取回（未预取） | 正文搜不到该封（主题/发件人仍可搜到） | coverage.body_indexed 说明；提示"打开过的邮件正文才会被索引" |
| FTS 表损坏 | "本地索引需要重建"+ 一键 | 03 的 heal + rebuild |
| 查询语法错 | 有错的部分给提示，其余照常 | `issues` 列表（带 span） |
| 查询太宽（命中 5 万+） | "结果太多（50,000+），请缩小范围" | 限制 total 计算成本（用近似或直接 cap） |
| 附件文本抽取失败/超限 | 该附件不进文本索引（文件名仍可搜） | attempts/上限；不重试无限次 |
| 大附件抽取耗时 | 无感（后台、低优先级） | 单附件 ≤ 32 MB、总量可关；CPU 预算来自 15 |
| 服务器回退被拒（BAD） | "这个邮箱不支持该查询条件" | 分类并提示；**绝不发未知扩展命令**（A1-F42 的连接杀手） |
| 搜索历史泄露隐私 | 历史只在本地、可一键清空、可在设置关闭 | — |
| 多账号搜索命中同名文件夹 | 结果里带账号与文件夹徽标 | 复用既有 `.msg-folder` 徽标 |

## 7. 迁移与兼容

- v1/v2 的 `search`（standalone + 逐字符分词）**必须换掉**：分词方案与表结构都变，**词表不兼容、无法原地迁移**
  ⇒ 升 `SCHEMA_VERSION`、新建 `search_text` + `search`、后台分批重建（§5.4）。
- `search`/`search_text` 是派生数据：**允许 drop 重建**（03 的迁移里唯一允许 drop 的表）。
- 既有 `to_tokens`/`to_query` 的**行为契约**（两侧同源）保留为门禁项，实现换名后门禁跟着改名。
- `search_history`/`saved_searches` 是新增表（M2 的虚拟文件夹）。

## 8. 性能预算（**按 A1-F47 的实测数字重写**）

| 指标 | 预算 | 依据（实测/推算） |
|---|---|---|
| 查询 P50 | ≤ 40 ms | `MATCH … LIMIT 20` 实测 0.01-0.10 ms；其余是 JOIN 与元数据 |
| 查询 P95 | ≤ 100 ms | 高亮（snippet）20 行实测 4.7 ms；keyset 分页 0.10 ms |
| **相关性排序（开启时）** | ≤ 10 ms | **必须两阶段**（有界 rowid 窗口再 rank）：实测 1.2-1.6 ms；朴素 `ORDER BY rank` 是 130-218 ms ⇒ **禁止** |
| 输入到出结果（防抖 250 ms 之后） | ≤ 150 ms | 前端 + IPC |
| 索引吞吐 | ≥ 20 封/秒（预算下限）；实测参考 510 msg/s（100k 规模、external-content） | 我们的解析 + 分词更重，取保守值 |
| 索引体积（`detail=column` + `columnsize=0`） | ≤ 0.8 × 已索引文本量 | 实测 bigram+detail=column 为文本量的 65.1%（逐字符是 126.9%） |
| 库总量（10 万封 + 1 年正文） | ≤ 1.5 GB | 与 15 §4 的 P13 对齐；正文文本 ~650 MB 时索引 ~420 MB |
| 重建（10 万封、仅头部） | ≤ 3 分钟 | — |
| 重建（10 万封含正文） | ≤ 10 分钟；**含附件文本 ≤ 30 分钟** | 实测全文索引 196 s（100k/650 MiB，Python）；附件抽取会主导墙钟 |
| 高亮（50 条） | ≤ 20 ms | snippet 实测 4.7 ms/20 行 |
| 附件文本抽取 | PDF ≤ 1 MB：≤ 200 ms；单个 ≤ 25 MB、PDF ≤ 200 页 | A1-F47：Exchange 默认上限 32 MB，照此办 |
| **回归保护** | 搜索延迟 > 100 ms 必须写 WARN 日志 | 研究明确指出"rank 排序会悄悄吃掉预算而测试查询看不出来" |

## 9. 测试计划

| 层 | 用例 |
|---|---|
| 单元（分词契约） | **必须包含实测抓到的误报用例**：`发\n票`、`发<br>票`、`AAA 发 CCC 票 DDD`、`开 发票 请` 都**不得**命中"发票"；`这个 发票号码 是…`**必须**命中；再补 200 组（中文 2/3/4 字、中英混排、英文子串、数字、emoji、全角半角、标点）；**错序不命中**（`票发` 不应命中 `发票`） |
| 单元（语法） | `parse_query` 40 条（含错误语法、引号、日期、多条件）；文本类条件走列过滤、元数据类条件走 SQL；`compile` 参数化断言 |
| 单元（高亮） | `snippet()` 输出与期待区间一致；重叠合并；HTML 转义 |
| 集成（排序） | 常见词（命中 10 万）下：默认日期序 ≤ 40 ms；开启相关性必须走两阶段且 ≤ 10 ms；**断言源码里不存在朴素 `ORDER BY rank`**（门禁） |
| 集成（队列） | 同步 1,000 封 → 批处理（500-2000/事务）→ 中断续跑 → 行数一致；打开过的优先 |
| 集成（更新语义） | 同一封索引两次：第 2 次必须用 `'delete'` + 旧文本，否则词条残留（断言旧词查不到） |
| 集成（回退） | 假服务器支持/不支持某条件两种；断言"不支持时不发未知扩展命令"（连接不被关） |
| 集成（迁移） | 从 v2 的 standalone 索引升到 v3：重建后召回与体积达标，且旧表可安全 drop |
| 真机 | QQ/163/Gmail 各跑：中文 2 字词、英文子串、`from:`/`has:attachment`/`after:`、附件名与附件文本搜索 |
| 性能 | §8 每项纳入 `tools/bench.ps1`；CI 跑宽松版 |

## 10. 门禁新增项

1. **两侧同源**：索引与查询的切分函数成对出现在同一模块，单测覆盖往返（既有门禁的延续，改名后跟着改）。
2. **不用 trigram**：`tokenize` 的值不得为 `trigram`（既有门禁保留）。
3. **参数化**：搜索相关源码不得出现把用户输入拼接进 SQL 的写法（正则 + 人工复查）。
4. **覆盖度必返回**：`SearchResponse.coverage` 为必填字段（结构体层面保证）。
5. **不发未知扩展命令**：`search_server` 的条件集合必须 ⊆ 已知可解析集合（枚举 + 测试）。
6. **禁止朴素相关性排序**：源码中不得出现不带 rowid 窗口限定的 `ORDER BY rank`（正则 + 复查）。
7. **禁止深 OFFSET**：搜索分页必须 keyset（源码检查 `offset` 不出现在搜索 SQL 里）。
8. **索引体积监控**：基准脚本记录体积，超预算 20% 报警。
9. **`optimize` 只允许用户触发**：维护任务里不得出现 `'optimize'`（源码检查）。


## 11. 逆向反驳（红队）

**反驳 1（已用实测解决）：逐字符分词的体积与英文质量都差，为什么不直接上 tantivy？**
- 实测结论：逐字符不仅体积大（126.9% vs bigram 的 65.1%），**还会误报**（`发\n票` 命中）——
  所以问题不是"要不要更强"，而是"现有方案是错的"。
- 改设计：换成 **CJK 感知 bigram**（§4.2），**不加任何新依赖、不动存储架构**，同时修掉误报与体积。
- 为什么仍不上 tantivy：它买的是**相关性排序**，而（a）实测 `ORDER BY rank` 本身就会超预算，
  我们用日期序 + 可选的窗口内重排；（b）多一个索引存储意味着多一套迁移/备份/损坏恢复；
  （c）SQLite 已经是 UID/UIDVALIDITY 一致性的载体。**等"需要专业排序/词形"成为真实痛点再说。**
- 让步：若将来真的需要分词级相关性，tantivy + `cang-jie`（MIT，jieba-rs）是备选，但要按"新增一个索引子系统"来评估。


**反驳 2：正文索引会让磁盘与 CPU 成本失控（10 万封正文 ≈ 800 MB 文本）。**
- 缓解：①正文索引**跟随 04 的预取窗口**（默认窗口内 ≤ 1 年，SYN-04/Y5），不是全量；
  ②`body` 列做**截断**（默认索引前 64 KB 正文，可在设置里调到全文）；
  ③附件文本默认只索引**文件名**，正文文本是 M2 的开关（并限 32 MB/个）；
  ④`quota::plan` 把索引体积单独列出、可单独清理（清理后搜索范围缩小，UI 明确告知）。
- 让步：`body_index_truncate_bytes` 做成设置项，给"我就要全文可搜"的用户。

**反驳 3：服务器回退可能很慢或很贵（对 10 万封的文件夹做 SEARCH）。**
- 缓解：①只在"本地零命中 + 覆盖不全"时**由用户点击**触发（不自动）；②一次最多 3 个文件夹、每个 ≤ 500 命中；
  ③走 04 的配额账本与并发限制；④服务器不支持该条件时给明确提示，不重试。
- 让步：设置里可关闭"服务器继续搜索"入口（企业账号可能不喜欢额外请求）。

**反驳 4：把"命中高亮"放在 Rust 里做会增加查询延迟。**
- 证据：FTS5 的 `snippet()`/`highlight()` 依赖 token 位置，我们的"字符短语"语义下它会给出怪异结果
  （把整段字符当 token 位置）。
- 缓解：①只对**当前页**（≤ 50 条）做高亮，且在拿到结果后计算；②用简单的子串定位（我们已经知道 term 文本）；
  ③预算 20 ms 并纳入基准。若超标，退化为"只在主题上高亮"。
- 让步：若 FTS5 的 `highlight()` 在我们的分词方案下实测可用（基准里加一条对照），就用它并省掉这段代码。

**反驳 5：查询语法对普通用户是负担（他们要的是"搜到就行"）。**
- 缓解：①自由文本始终可用；语法是增强；②输入 `from:` 时给补全（联系人）；③语法错误给出**修正建议**而不是报错；
  ④保存搜索把复杂查询封装成一个名字（普通用户只需点）。
- 让步：M2 提供一个"高级搜索"对话框（下拉条件拼查询），让不会打字法的用户也能用。

**反驳 6：多账号搜索可能命中几十万封（统一收件箱场景），分页与排序会退化。**
- 缓解：①默认按**账号/文件夹**给结果分组或加徽标；②限制 `total` 的精确计算（超过 10,000 用近似并提示）；
  ③默认按日期倒序（走索引，稳定），相关性排序只在有 free_text 且有上限时用。
- 让步：若用户在"全部账号"下搜索且命中过多，UI 建议先选账号/文件夹。

## 12. 未决问题

| # | 问题 | 影响 | 建议 |
|---|---|---|---|
| Q1 | 分词器最终选 A/B/C/D（§4.2） | 索引体积/质量 | 用基准测量决定；**默认保 A**（能跑就是资产） |
| Q2 | 正文索引的截断阈值（64 KB？） | 体积 vs 召回 | 默认 64 KB，可调 |
| Q3 | 附件文本索引默认开关 | 隐私/成本 | 默认**关**（只索附件名），M2 给开关 |
| Q4 | 是否保留"服务器继续搜索"入口 | 额外请求 | 保留但默认折叠，可关 |
| Q5 | 相关性排序是否值得 | 复杂度 | **已定**：默认日期序；相关性作为可选开关，且必须走**两阶段有界窗口重排**（朴素 rank 会超预算，实测 130-218 ms） |
| Q6 | 搜索历史长度与清理 | 隐私 | 保留最近 100 条，可一键清空 |
| Q7 | 是否需要 `NEAR` 邻近搜索 | 索引体积 | **不做**：`NEAR` 要求 `detail=full`，索引体积翻倍（实测 172% vs 65%）；真要时用一个单独的索引变体评估 |
| Q8 | 附件名的 glob 语义（`filename:report*`） | 便利 | 可选：单开一张**只针对附件名**的 trigram 索引表（trigram 的 `LIKE` 加速在短文本上是合理用途），M2 评估 |

## 13. 验收清单

- [ ] 10 万封夹具：查询 P95 ≤ 100 ms；**开启相关性时 ≤ 10 ms（两阶段）**；索引体积 ≤ 0.8× 文本量。
- [ ] **误报回归**：`发\n票`、`发<br>票`、`AAA 发 CCC 票 DDD`、`开 发票 请` 都不命中"发票"；`这个 发票号码 是…` 必须命中。
- [ ] 中文 2 字词、英文子串、数字、全角半角、emoji 各 20 条查询符合预期（含**错序不命中**）。
- [ ] 语法：40 条用例（含错误语法给建议）；文本条件走列过滤、元数据条件走 SQL；`from:`/`has:attachment`/`after:`/`filename:` 真机可用。
- [ ] 索引更新语义正确：同一封重新索引后，**旧词条查不到**（用 `'delete'` + 旧文本）。
- [ ] 覆盖度提示准确（索引落后时不谎称"没有这封邮件"）；服务器回退在支持与不支持两种服务器上都表现正确。
- [ ] 索引可暂停/取消/重建；重建分批、可续跑；重建后与 `messages` 行数对账一致；v2 → v3 迁移路径有测试。
- [ ] 高亮用 `snippet()`/`highlight()` 且正确（区间、转义、重叠合并）。
- [ ] 搜索延迟 > 100 ms 时打出 WARN（便于发现回归）。
- [ ] `check.ps1` 九项新增检查通过。


> **章节名对照（模板 13 节）**：状态机→§5 主流程。
> 内容齐全，只是节名与模板不完全同名（见 design/README.md 的说明）。
