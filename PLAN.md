# PLAN —— 第 88 轮：保存搜索（09 目标 7 的第一片：存储 + 命令）

> 依据 `design/09-search.md` §1 目标 7 与 §4.1 的 `saved_list/saved_save/saved_delete`。
> 切两片：本轮**存储 + 命令 + 测试**，下一片**界面**（侧栏入口 + 保存当前查询）。

## 现状

搜索语法三片已完整（77-79），但**没有一个地方能把一个查询存下来**：用户每次要重新敲
`from:shop has:attachment after:2026-01-01`。设计 §4.1 的函数签名就在那儿，`user.db` 里**没有表**。

## 目标与验收

1. `user.db` 新表 `saved_searches`（id / account_id / name / query / created_at），
   **随 `create table if not exists` 进 schema**（老库打开即得，不升版本——本仓既有模式）；
2. `UserStore` API：`save_search` / `list_saved_searches` / `delete_saved_search` / `rename_search`，
   形状照 drafts 那套；
3. **同名即替换**（用户改一个保存的查询，不该留下两份同名的）；
4. **查询为空/纯空白拒绝**（一个没有查询的"保存搜索"点了只会显示空）；
5. **名字去空白后不得为空**，且长度有上限（60 字符，侧栏放得下）；
6. 测试钉住：存/列/删/改名、同名替换、空查询拒绝、空名字拒绝、**删除不影响别人的**、
   **缓存清空不丢保存的搜索**（它在 user.db 里）；
7. 门禁 +1 步；文档。

## 非目标

界面（下一片）、搜索历史（`history_push`，单独一块——它要有"最近 N 条"的淘汰策略）、
虚拟文件夹（保存搜索 + 固定条件，M2 后半）。

## 关键决定

- **放 `user.db` 而不是 `cache.db`**：保存的搜索是**用户自己的数据**，删缓存不该让它消失
  （与草稿同一条理由）。
- **`account_id` 可为 NULL**：一个搜索可以属于某个账号，也可以是全局的。
  本轮的列表命令按账号过滤，**全局的（NULL）对所有账号可见**——这个语义要写进注释，
  否则下一个人会写成 `where account_id = ?` 把全局的漏掉。
- **id 用自增整数**，命令层转成字符串给前端（Tauri IPC 友好）；删除按 id + account 双重条件
  **不得跨账号删**。
- **不做排序字段**：按 `created_at desc`——后保存的在上（侧栏的直觉）。

## 改动点

### C1 `crates/mail-core/src/userdb.rs`
- `SavedSearch { id: i64, account_id: Option<String>, name: String, query: String, created_at: i64 }`（Serialize）；
- `pub fn save_search(&self, account_id: Option<&str>, name: &str, query: &str, now: i64) -> Result<i64>`
  （同名 ⇒ 更新 query；返回 id）；
- `pub fn list_saved_searches(&self, account_id: &str) -> Result<Vec<SavedSearch>>`
  （本账号的 **+ 全局的**）；
- `pub fn delete_search(&self, id: i64, account_id: &str) -> Result<bool>`；
- `pub fn rename_search(&self, id: i64, account_id: &str, name: &str) -> Result<bool>`；
- schema：`create table if not exists saved_searches (...)` + `name` 唯一约束
  （但愿 `account_id` 可空——SQLite 的唯一索引对 NULL 不冲突，**正好是我们要的**：
  每个账号可以各有一个"未读"，而全局的也只能有一个"未读"）。

### C2 测试（8 条，userdb 内）

### C3 `tools/check.ps1` +1 步

### C4 文档：README、`design/09` §4.1 注记、PROGRESS、HANDOFF

## 验收

```
cargo test -p mail-core --lib saved_search
cargo test --workspace
cargo fmt --check；cargo clippy -D warnings
```

## 风险与退路

- **唯一索引 + 空 account_id**：SQLite 里 `NULL != NULL`，所以全局行不会被唯一索引挡住
  ⇒ **同名全局搜索会插入两行**。这是要找出来的行为：测试先写一条，再写一条同名的，
  看它到底几行；如果不是 1 行，就改成"先查后写"并把理由写进注释（单写线程，安全）。
- 整块可停：表空着就是今天的样子，**不碰任何现有表**。
