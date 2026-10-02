# 12 界面与设计系统（好看、好用、无障碍）

> **目标**：让界面**第一眼不像"开源小工具"**：克制、清晰、字重层次分明、动效极少但准确；
> 键盘能走完全部流程；屏幕阅读器能读懂；中英双语；10 万封不卡。
>
> 覆盖 ID：UI-01…20、READ-11/12/14/15（列表与阅读的交互部分）。
> 依赖：**P1（前端技术栈）决策**（`01` §7）。本文按"Vite + TypeScript + Svelte 5"写具体接口，
> 若选无打包器方案，令牌与交互规格不变，只把组件实现换成原生模块（工作量 M+）。
> 视觉参考原型：[`12-ui-prototype/index.html`](12-ui-prototype/index.html)（可直接双击打开）。

---

## 1. 目标与非目标

**目标**
1. **视觉系统可复用**：所有颜色/间距/字重/圆角/阴影来自**唯一令牌源**，禁止散落硬编码。
2. **内容优先**：默认三栏；列表信息密度高但不拥挤；阅读区最大宽度受控（长行不利阅读）。
3. **键盘优先**：任何操作都有快捷键；`Ctrl+K` 命令面板；焦点可见且顺序合理。
4. **无障碍达标**：对比度 ≥ 4.5:1（正文）、焦点环、语义角色、屏幕阅读器可用、200% 缩放不破版、支持"减少动效"。
5. **中英双语**：所有用户可见文本走 i18n；日期/数字按本地格式；为 RTL 预留（不实现）。
6. **性能**：首屏 ≤ 400 ms 出骨架；列表滚动 60 fps @ 10 万封；动效 ≤ 150 ms 且可关。

**非目标**
- 不做可换肤/主题市场（只做亮/暗/跟随系统 + 强调色）。
- 不做移动端布局（窗口最小 900×600）。
- 不做"炫技"动效（悬浮粒子、复杂视差等一律不要）。

## 2. 设计原则

1. **克制**：一屏只有一个主行动按钮；颜色只用于语义（品牌色、未读、危险、成功、警告）。
2. **层次靠字重与间距，不靠线框**：分隔线只在必要处（列表行之间），避免"表格化"。
3. **状态可见**：每个操作都要有反馈（保存/发送/同步/离线），但**不刷屏**（横幅 + 状态栏，不用满屏弹窗）。
4. **可降级**：动效可关、图片可拦、字体可放大、列表可虚拟化。
5. **不打扰**：不弹广告、不弹推荐、不弹"给我们评分"。

## 3. 用户可感知行为

| 场景 | 用户看到 |
|---|---|
| 首次启动 | 空白态三步引导（加账号 → 选范围 → 设为默认邮件程序），带插画但**不花哨** |
| 切主题 | 顶栏一个按钮，立即切换（不重启、不闪白）；`color-scheme` 同步切换（下拉框不黑底黑字） |
| 键盘流 | `J/K` 移动、`Enter` 打开、`R` 回复、`E` 归档、`#` 删除、`/` 搜索、`Ctrl+K` 命令面板、`?` 快捷键总览 |
| 多选 | `Shift+点击` 选范围、`Ctrl+点击` 单加、`Ctrl+A` 全选当前页；顶部变成批量条（含"已选 23 封"） |
| 拖动 | 邮件拖到文件夹即移动；附件拖到资源管理器即另存；文件拖进写信即加附件 |
| 屏幕阅读器 | 列表是 `role="listbox"`/`option`（虚拟化后仍保持语义），未读用 `aria-label`（不只靠颜色） |
| 200% 缩放 | 三栏自动折成两栏/单栏；不出现横向滚动条 |
| 慢网络 | 列表骨架屏（不是转圈）；同步进度在状态栏；不阻塞阅读 |
| 出错 | 就地错误（红色小字 + 重试），不用全屏对话框 |

## 4. 设计令牌（唯一来源 `web/src/design/tokens.ts` → 生成 CSS 变量）

```css
:root {
  /* 品牌与语义色（亮） */
  --brand-600: #2563eb;  --brand-700: #1d4ed8;  --brand-050: #eef4ff;
  --on-brand: #ffffff;               /* 主按钮上的文字色（亮色下白字对 #2563eb = 4.9:1 ✅） */
  --danger-600: #c0392b; --danger-050: #fdecea;
  --success-600: #15803d; --warning-600: #b45309;
  --unread: var(--brand-600);      /* 未读点与左侧条 */
  /* 中性（亮） */
  --bg: #f6f7fb; --surface: #ffffff; --surface-2: #f0f2f8; --surface-3: #e8ebf4;
  --border: #e2e6f0; --border-strong: #ccd3e3;
  --text: #141b2d; --text-2: #4b5568; --text-3: #5f6a80;   /* **评审修正**：原 #7a8398 对 --surface 只有 3.80:1、对 --surface-2 只有 3.40:1（11px 小字不达标）；改成 ≥4.5:1 */
  /* 阴影与圆角 */
  --shadow-1: 0 1px 2px rgba(16,24,40,.06);
  --shadow-2: 0 6px 20px rgba(16,24,40,.10);
  --radius-sm: 6px; --radius: 10px; --radius-lg: 14px; --radius-pill: 999px;
  /* 间距（4 的倍数） */
  --sp-1: 4px; --sp-2: 8px; --sp-3: 12px; --sp-4: 16px; --sp-5: 20px; --sp-6: 24px; --sp-8: 32px;
  /* 字体 */
  --font-ui: "Segoe UI Variable Text", "Segoe UI", "Microsoft YaHei UI", system-ui, sans-serif;
  --font-read: "Segoe UI Variable Text", "Segoe UI", "Microsoft YaHei UI", system-ui, sans-serif;
  --font-mono: "Cascadia Mono", Consolas, monospace;
  --fs-xs: 11px; --fs-sm: 12px; --fs-md: 13px; --fs-lg: 15px; --fs-xl: 18px; --fs-2xl: 22px;
  --lh-tight: 1.35; --lh-normal: 1.55; --lh-read: 1.7;
  /* 动效 */
  --dur-fast: 90ms; --dur: 140ms; --ease: cubic-bezier(.2,.7,.3,1);
  color-scheme: light;
}
:root[data-theme='dark'] {
  --brand-600: #6ea8ff; --brand-700: #8bb9ff; --brand-050: #16233b;
  --on-brand: #0f131b;               /* **第二轮评审修正**：暗色下 #6ea8ff 上的白字只有 2.41:1（axe serious），
                                        深色文字对 #6ea8ff 是 7.7:1 ✅ —— 主按钮必须用这个变量，不许写 #fff */
  --danger-600: #ff8071; --danger-050: #3a2320;
  --success-600: #4ade80; --warning-600: #fbbf24;
  --bg: #0f131b; --surface: #161b25; --surface-2: #1d2431; --surface-3: #242c3b;
  --border: #2a3342; --border-strong: #3a4759;
  --text: #e8ecf4; --text-2: #a9b3c6; --text-3: #8b96a8;   /* 评审修正：原 #7b8698 对 --surface-2 只有 ~4.3:1，11px 小字不达标 */
  --shadow-1: 0 1px 2px rgba(0,0,0,.35); --shadow-2: 0 8px 24px rgba(0,0,0,.45);
  color-scheme: dark;
}
@media (prefers-reduced-motion: reduce) { :root { --dur-fast: 0ms; --dur: 0ms; } }
```
**规则**：①组件不得写十六进制颜色；②`color-scheme` 必须随主题切（既有教训）；
③密度档位（`comfortable` / `compact`）只改 `--row-h` 与间距，不改字号层级。

## 5. 组件与接口（前端模块边界）

```ts
// web/src/api/index.ts —— **invoke 的唯一出口**（组件不许直接 invoke）
export const api = {
  accounts: { list(): Promise<Account[]>; create(input: NewAccount): Promise<Account>; pause(id: string): Promise<void>; /* … */ },
  sync: { folderNow(id: string, folder: string): Promise<JobId>; status(): Promise<SyncStatus>; pause(jobId: string): Promise<void>; },
  mail: { page(q: PageQuery): Promise<Page<ThreadRow>>; open(account: string, folder: string, uid: number): Promise<MessageDetail>; batch(ops: MessageOp[]): Promise<BatchReport>; },
  search: { run(q: SearchRequest): Promise<SearchResponse>; save(name: string, query: string): Promise<void>; },
  compose: { send(m: OutgoingMessage): Promise<OutboxId>; undo(id: OutboxId): Promise<void>; saveDraft(d: Draft): Promise<void>; },
  settings: { get(): Promise<Settings>; set(patch: Partial<Settings>): Promise<void>; },
  // …按 02 §6.3 的命令清单一一对应
};

// web/src/stores/*.ts —— 每个域一个 store（细粒度响应式，避免单一大对象）
export const accounts = createStore<Account[]>([]);
export const list = createStore<{ rows: ThreadRow[]; total: number; selected: Set<string>; anchor?: string }>(…);
export const reader = createStore<{ detail?: MessageDetail; bodyMode: 'html'|'text'; allowRemote: boolean; quotesExpanded: Set<number> }>(…);
export const compose = createStore<{ drafts: DraftMeta[]; active?: Draft; undo?: { id: OutboxId; until: number } }>(…);
export const sync = createStore<{ jobs: JobSnapshot[]; net: NetState; coverage: SearchCoverage }>(…);
export const ui = createStore<{ theme: Theme; density: Density; palette: boolean; shortcuts: Record<string,string> }>(…);

// 组件契约（纯展示，无副作用；事件向上、数据向下）
export interface ListProps { rows: ThreadRow[]; selected: Set<string>; onOpen(id: string): void; onSelect(id: string, mods: Mods): void; onContextMenu(id: string, at: Point): void; }
export interface ReaderProps { detail?: MessageDetail; bodyMode: BodyMode; allowRemote: boolean; onToggleRemote(): void; onToggleQuote(i: number): void; /* … */ }
// 虚拟列表：只渲染视口 ± 8 行；键盘导航通过"锚点 + 滚动到可见"实现
export interface VirtualListOptions { rowHeight: (i: number) => number; overscan: number; getKey: (i: number) => string }
```

## 6. 布局

| 断点（**全部是 CSS/逻辑像素**，不是物理像素 —— 这台机器实测视口 993×621 CSS px，物理 2160×1440；`HANDOFF.md` §3 记过一次因此误判布局的教训） | 布局 | 说明 |
|---|---|---|
| ≥ 1,180 px | **三栏**：导航 200 | 列表 320-420（可拖） | 阅读 剩余 | 默认（用户机器实测视口约 993 CSS px，属下一档） |
| 900-1,179 px | **三栏窄**：导航 180 可折叠 | 列表 280-360 | 阅读 剩余 | 导航默认收起成图标条 |
| 600-899 px | **两栏**：列表 + 阅读（导航抽屉） | 适合小窗口 |
| < 600 px | **单栏**：列表或阅读（导航抽屉） | 窗口最小 900×600（CSS px）会阻止拖到这么窄，但**缩放 ≥150% 时会自然落到两栏/单栏** —— 这是预期行为，不是 bug |

- 分隔条可拖（记忆宽度）；双击分隔条 = 折叠/展开。
- 阅读区正文**最大宽度 720 px**（超出留白），行高 1.7；代码/纯文本模式用等宽。
- 顶部：账号/文件夹标题 + 过滤视图 + 搜索框 + 主行动按钮（写邮件）。
- 底部状态栏：同步进度、网络状态、索引进度、待同步操作数（都只在"有内容"时出现）。

## 7. 交互

### 7.1 快捷键（默认两套预设：`Gmail 风格` / `Outlook 风格`，可自定义）
| 动作 | Gmail 预设 | Outlook 预设 | 备注 |
|---|---|---|---|
| 上/下一封 | `K` / `J` | `↑` / `↓` | 列表聚焦时 |
| 打开 | `Enter` / `O` | `Enter` | |
| 返回列表 | `U` / `Esc` | `Esc` | |
| 回复 / 全部回复 / 转发 | `R` / `A` / `F` | `Ctrl+R` / `Ctrl+Shift+R` / `Ctrl+F` | |
| 归档 / 删除 / 标未读 / 旗标 | `E` / `#` / `Shift+U` / `S` | `Ctrl+E`… | 归档=移出收件箱（有归档文件夹时） |
| 搜索 / 命令面板 / 快捷键总览 | `/` / `Ctrl+K` / `?` | 同 | |
| 写邮件 / 发送 / 撤销发送 | `C` / `Ctrl+Enter` / `Ctrl+Z` | 同 | 撤销发送在窗口内有效 |
| 下一个未读 / 折叠全部 | `N` / `[` | `Ctrl+U` | |

**规则**：①所有快捷键都能在设置里改；②冲突检测（同一组合被占用时提示）；③输入框内不触发单键快捷键（除 `Esc`）。

### 7.2 命令面板（`Ctrl+K`）
```
输入 → 模糊匹配三类：跳转（文件夹/标签/账号）、动作（写邮件/同步/标记）、设置（打开某项设置）
      → 上下键选择、Enter 执行、Tab 看详情
      → 也接受搜索语义（输入"发票"直接给"搜索 发票"的候选）
```
### 7.3 右键菜单与拖放
- 列表行：打开/标已读/标旗标/稍后提醒/移动到…/复制到…/归档/删除/打印/导出/查看源码。
- 文件夹：刷新/标记全部已读/清空垃圾箱/重命名（本地文件夹）/上传到服务器（本地账号）。
- 附件 chip：打开/另存为/全部保存/复制文件名。
- 拖放：邮件 → 文件夹（移动）；附件 → 资源管理器（另存）；文件 → 写信窗口（加附件）。

### 7.4 多选
```
点击 = 单选；Ctrl+点击 = 切换；Shift+点击 = 从锚点范围选择；Ctrl+A = 当前页全选
选中 ≥1 时：列表顶部变成批量条（"已选 N 封"+ 归档/删除/移动/标已读/更多）
Esc = 清空选择
```

## 8. 状态设计（每个都必须设计，不许留空）

| 状态 | 表现 |
|---|---|
| 空（无账号） | 插画 + "添加邮箱账号"主按钮 + 一句"服务器设置我们替你填好" |
| 空（文件夹无邮件） | "这个文件夹里还没有邮件" + （收件箱时）"刷新"次要按钮 |
| 空（搜索无结果） | 覆盖度说明（`09` §5.2）+ "在服务器上继续搜索" + "保存这个搜索" |
| 加载 | **骨架屏**（列表 8 行灰条；阅读区标题+段落灰条），不用 spinner 遮罩 |
| 部分加载（索引落后） | 行内提示条（不是阻塞） |
| 错误（单封） | 阅读区错误卡：原因 + 重试 + "查看源码" |
| 错误（全局） | 状态栏红点 + 横幅（16 §2），可展开详情 |
| 离线 | 状态栏"离线"徽标 + 列表顶部一行说明（可关） |
| 无权限/需重登 | 该账号区块整体置灰 + 主按钮"重新登录" |
| 首次索引 | 状态栏进度（可点开看细节、可暂停） |

## 9. 无障碍（不达标不算完成）

- **虚拟列表的键盘/焦点模型以 `15` §11 反驳 4 为唯一来源**（容器持有焦点 + `aria-activedescendant` +
  `aria-setsize`/`aria-posinset`；行不是独立 tab stop）。本节其余条目与它配套，**不要另立一套**。
- **语义**：列表 `role="listbox"` + `option`（`aria-selected`、`aria-label` 含"未读/有附件"）；
  工具栏 `role="toolbar"`；对话框 `role="dialog" aria-modal`。
- **焦点**：焦点环始终可见（`outline: 2px solid var(--brand-600); outline-offset: 2px`）；
  打开邮件把焦点移到阅读区标题（`tabindex="-1"`）；关闭回到列表原位；模态框焦点陷阱 + `Esc` 关闭。
- **键盘全覆盖**：任何鼠标能做的都能用键盘做（含右键菜单：`Shift+F10` 或菜单键）。
- **对比度**：正文 ≥ 4.5:1，次要文字 ≥ 3:1（在 CI 里用脚本核对令牌组合）。
- **缩放**：200% 缩放不破版、不出现横向滚动；`font-size` 用相对单位。
- **减少动效**：`prefers-reduced-motion` 时禁用过渡（令牌已处理）。
- **屏幕阅读器**：新邮件到达时用 `aria-live="polite"` 播报（可关）；不依赖颜色表达状态（未读同时有文本标签）。
- **本地化布局**：按钮宽度按文案自适应，不留"英文长度假设"。

## 10. i18n

```
web/src/i18n/{zh-CN.json, en.json}   键名形如 "reader.remote_blocked": "已拦截 {count} 张远程图片"
t('reader.remote_blocked', { count: 12 })
```
- **禁止**在组件里写面向用户的中文字面量（门禁：源码扫描 `>[\u4e00-\u9fa5]` 与字符串字面量中的中文）。
- 日期/数字/文件大小用 `Intl`；时区用 `Intl.DateTimeFormat().resolvedOptions().timeZone`。
- 复数与性别：中英都简单，但保留 `Intl.PluralRules` 接口，便于未来加俄语等。
- RTL：用逻辑属性（`margin-inline-start` 等）书写 CSS，为将来留路（不实现切换）。
- 后端文案：`ApiError.code → t()`；后端 `message` 只作补充（`02` §6.1）。

## 11. 性能预算

| 指标 | 预算 |
|---|---|
| 首屏骨架出现 | ≤ 400 ms（已有缓存） |
| 列表首屏渲染 | ≤ 100 ms（50 行） |
| 滚动（10 万封） | 60 fps；每帧 DOM 变更 ≤ 40 行 |
| 主题切换 | ≤ 50 ms，不闪白 |
| 打开命令面板 | ≤ 50 ms |
| 动效时长 | ≤ 150 ms（可关） |
| 主包体积（gzip） | ≤ 300 KB（含框架与设计系统） |

## 12. 测试与门禁

| 层 | 内容 |
|---|---|
| 视觉回归 | Playwright 截图对比（亮/暗 × 三档宽度 × 6 个关键界面），阈值 0.1% |
| 无障碍 | axe-core 自动检查（0 critical/serious）+ 手工键盘走查 + 屏幕阅读器抽查（Narrator） |
| 对比度 | 脚本遍历令牌组合，断言比值（正文 ≥ 4.5、次要 ≥ 3） |
| i18n | 键覆盖检查（zh/en 键集合一致）+ 无硬编码中文扫描 |
| 快捷键 | 冲突检测 + 每个动作都有快捷键的覆盖率检查 |
| 性能 | 长列表滚动脚本（Phantom 采样）+ 首屏打点 |

**门禁新增项（`check.ps1`）**
1. 令牌单一来源：组件目录不得出现十六进制颜色（正则）。
2. i18n 键一致性（**评审修正，原写法是空转的**：两个文件可以一起丢键）：
   ①**代码里每个 `t('key')` 必须在 `zh-CN.json` 里存在**（正向，这才是真正会挡住 bug 的方向）；
   ②`zh-CN` 与 `en` 的键集合互差为 0；③扫描范围限定 `web/`，过渡期把旧 `src/app.js` 放进
   **允许清单并记一个"待消灭的硬编码中文条数"**（这个数字只能减，门禁比对它）—— 否则阶段 0-2 永远绿不了。
3. 无硬编码中文文案（组件/模板中的字符串字面量；**范围限 `web/`，旧 `src/app.js` 走允许清单 + 欠账计数**）。
4. 对比度脚本通过。
5. `invoke(` 只允许出现在 `web/src/api/`（源码检查）。

**前端门禁改造（第二轮评审的 BLOCKER B1，共 6 条，必须一次做完）**：现有 `check.ps1` 的 12 步里有 6 步是
前端相关，换栈后**要么失败、要么变成空转**，其中最危险的是"**绿着发出旧 UI**"。逐条：

| # | 现状 | 换栈后会发生什么 | 改成 |
|---|---|---|---|
| 1 | `cargo build` 只重嵌 `frontendDist` 里**现在**的东西；前端构建（`beforeBuildCommand`）只有 Tauri CLI 会跑 | **门禁全绿但界面是旧的**（正是 `HANDOFF.md` §3 记过的那类事故） | **`check.ps1` 第 0 步自己跑前端构建**（`pnpm build`），再跑其余检查 |
| 2 | "index.html 引用的资源存在"：`Join-Path 'src' '/src/main.ts'` → `src\src\main.ts` | **失败**（Vite 的入口是 `/src/main.ts`，构建后是 `/assets/index-<hash>.js`） | 改成检查**构建产物**（`web/dist`）里的引用；源侧只检查入口存在 |
| 3 | "元素 id 引用"读 `src/app.js` + `src/index.html` | 旧文件删掉后**正则匹配 0 次也算通过**（空转） | 改成**非空断言**（匹配数 < N 即失败）+ 用 **CDP 冒烟**替代静态 id 检查（断言三栏渲染出来） |
| 4 | "每个前端命令都已注册"读 `src/app.js` | 同上，空转 | 从 `web/src/api/**` 抽取命令名；**断言抽取到的命令数 ≥ 15**（防空转） |
| 5 | 两条 CSP/宽度不变量是**钉在 `src/app.js` 的字面文本**上的 | 三元表达式搬到 TS/Svelte 后**失败**，而最省事的"修法"是把正则放宽 ⇒ 防线消失 | 把 CSP 与宽度规则**抽成导出常量**（单一来源），同时断言**源码与产物**里都成立；`check.ps1` 里加"**匹配数必须 > 0**" |
| 6 | `node --check src/*.js` 看不到 TS/Svelte，也不解析构建产物 | 语法错误只在运行时暴露 | 改成 `tsc --noEmit`（或 `svelte-check`）+ 对**产物**做一次 `node --check`（能抓打包器不该产生的语法问题） |

过渡期纪律：**两套都查**（旧 `src/app.js` 还在就继续查它），直到旧 UI 删除；这条写进 M1a 的前端切换块验收（`19` §7）。


## 13. 逆向反驳（红队）

**反驳 1：换前端栈（P1=②）是最大的单点风险，可能让 M1 延期数月。**
- 缓解：①分阶段替换（02 §7.3）：先"列表 + 阅读"两屏，旧 UI 保留可用；②令牌与交互规格与框架无关，
  即便回退到无打包器方案也不白做；③把"虚拟列表 + 无障碍 + i18n"这三件放在第一阶段（它们是原生 JS 最难的部分）。
- 让步：如果第一阶段超期 2 周以上，**停下来重新评估**（宁可继续用旧 UI 补功能，也不要卡在重构上）。

**反驳 2：设计系统容易变成"为了好看牺牲信息密度"（列表行变高、留白过多）。**
- 缓解：①默认 `comfortable` **行高 40 px**（两行信息）—— 评审按实测算过：993×621 视口减去
  面板头/搜索条/页脚约 104 px 后只剩 ~517 px，44 px 行高只能放 **11 整行**（含半行 11.75），
  所以要么把行高压到 40 px（12 整行），要么把验收写成 11；**取 40 px + 12 行**；
  ②提供 `compact`（34 px，单行）；③验收里写清"**完全可见**的行数"这个口径。
- 让步：允许用户自定义字号与行高（存 settings）。

**反驳 3：要求"所有操作都有快捷键"会让实现复杂度上升，而且大多数人不用键盘。**
- 缓解：①只对**高频 20 个动作**要求快捷键（其余通过命令面板可达，命令面板是"全部动作"的兜底）；
  ②快捷键表集中在一处（数据驱动），组件只声明 `action` 名，不各自绑定。
- 让步：无障碍验收里"键盘可达"是硬指标，但"每个动作都有**单键**快捷键"不是。

**反驳 4：暗色主题 + 邮件 HTML（自带浅色背景）会很难看（正文白底黑字落在暗色界面里）。**
- 证据：真实邮件大量写死 `background:#fff; color:#000`。
- 缓解：①阅读区提供"邮件自带样式 / 强制适配暗色"开关（默认**保持邮件原样**，因为改色会破坏品牌邮件）；
  ②阅读区在暗色主题下仍给正文一个**浅色容器**（像"纸张"），而不是把邮件硬掰成暗色；
  ③容器边缘用 `--surface-2` 过渡，避免生硬。
- 让步：M2 可加"对纯文本/无样式邮件套用主题色"的智能判断。

**反驳 5：i18n 与"文案集中"会让开发变慢（每句话都要过 t()）。**
- 缓解：①先只做 zh + en；②文案模板与检查脚本让"漏翻译"在 CI 里暴露，而不是在用户那里暴露；
  ③后端文案集中在 `i18n.rs`（`02` §3），前端集中在 JSON。
- 让步：允许 **M1 只发中文**，但**代码结构必须已经是 i18n**（P11 的"地基先打"），en 可在 M2 补。

**反驳 6：命令面板、拖放、多选、独立窗口……这些"专业功能"对普通用户是负担。**
- 缓解：①默认界面只有三栏 + 工具栏，高级交互**不占视觉位置**（都在右键/命令面板/快捷键里）；
  ②首次运行不弹"教学"（只在用户按 `?` 时给总览）；③设置里可关"命令面板快捷键"。
- 让步：①若实测用户发现率很低（可以用本地计数看，不上传），就把它降级为"高级"开关。

## 14. 未决问题

| # | 问题 | 影响 | 建议 |
|---|---|---|---|
| **P1** | 前端技术栈 | 12 的全部实现 | Vite + TS + Svelte 5（见 02 §7.1） |
| U1 | 默认密度与字号 | 一屏信息量 | comfortable/13px；提供 compact |
| U2 | 暗色下正文的"纸张"策略 | 观感 | 保原样 + 浅色容器（反驳 4） |
| U3 | 快捷键默认预设 | 习惯 | 首次运行时问一次（Gmail/Outlook） |
| U4 | 是否做"跟随系统主题"第三态 | 期待 | 做（`theme.js` 已预留状态） |
| U5 | 主界面是否显示账号头像/颜色 | 辨识度 | 显示颜色条 + 首字母，不做头像图片（隐私） |
| U6 | 图标来源 | 一致性与许可 | 自绘 SVG 图标集（≤ 60 个），统一线宽 1.5 px |

## 15. 验收清单

- [ ] 令牌单一来源：组件无硬编码颜色/间距（门禁）。
- [ ] 亮/暗 + 三档宽度 × 6 个关键界面：视觉回归通过；真机（993 px 视口、comfortable 行高 40 px）**一屏 12 整行**（口径 = 完全可见）。
- [ ] 键盘可以完成"打开→回复→发送→归档→搜索"全流程；`?` 有总览；冲突检测生效。
- [ ] axe-core 0 critical/serious（主按钮用 `--on-brand`：暗色 7.7:1；`--text-3` ≥ 4.5:1）；Narrator 抽查能读完一封邮件并执行"标为已读"。
- [ ] 虚拟列表：容器持焦 + `aria-activedescendant` + `setsize`/`posinset`（Narrator 能读出"第 3 封，共 149 封，未读"）。
- [ ] 200% 缩放不破版（≥150% 允许落到两栏/单栏，这是预期）；`prefers-reduced-motion` 生效。
- [ ] 代码里每个 `t('key')` 都在 `zh-CN` 里存在；zh/en 键集合互差为 0；`web/` 无硬编码中文（旧 `app.js` 走允许清单 + **欠账计数只能减**）；日期/大小按本地格式。
- [ ] 10 万封滚动 60 fps；首屏骨架 ≤ 400 ms；主包 ≤ 300 KB。
- [ ] `check.ps1` 五项新增检查通过（且**非空转**：匹配数为 0 即失败）。
- [ ] **原型 ↔ 实现的一致性由工具判定**（不是人眼）：见 §15.1。

### 15.1 原型 ↔ 实现的机械比对（评审要求：把"允许 10% 偏差"变成可判定）

```
tools/proto-diff.mjs
  1. 用 CDP 打开两个目标：① design/12-ui-prototype/index.html?theme=<t>&density=<d>
                          ② 运行中的应用（同一主题与密度）
  2. 三个宽度各取一次几何量：993×621（本机实测视口）、1280×800、1600×900（CSS px，deviceScaleFactor=1）
  3. **只比对布局骨架，不比像素**：取关键元素的 getBoundingClientRect()
     —— 三栏宽度、行高、可见行数、工具栏按钮位置、状态栏高度
     断言：栏宽差 ≤ 4 px、行高完全相等、可见行数相等、状态栏高度相等
  4. 另存两张 PNG 供人眼复核（像素 diff 只作参考叠色图，**不作门禁**）
  5. 输出表格：每项"原型值 / 实现值 / 差 / 结论" + 失败清单
```
> 为什么不用像素 diff 当门禁：字体渲染与亚像素抗锯齿会让"同一份 CSS"也产生 3-8% 的像素差，
> 拿它卡门禁必然天天假警报（与 `16` §11 反驳 5 同类问题）。**几何量做门禁、截图做人眼复核。**


> **章节名对照（模板 13 节）**：数据结构→§4 设计令牌+§5 组件接口；失败模式→§8 状态设计；测试/门禁→§12。
> 内容齐全，只是节名与模板不完全同名（见 design/README.md 的说明）。
