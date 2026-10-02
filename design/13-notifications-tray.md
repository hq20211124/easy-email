# 13 通知、托盘与后台

> **目标**：让"新邮件来了"这件事**该响的时候响、不该响的时候不烦**，并且点一下就能到那封邮件；
> 关掉窗口不等于退出，托盘常驻但**不吃资源**。
>
> 覆盖 ID：NOTIF-01…07、UI-16（系统集成的一部分）、PERF-04（空闲资源）。
> 事实依据：[`A1-research-findings.md`](A1-research-findings.md)（分发研究 §6：桌面的通知能力边界）。
> 上游：04（监听与调度）、03（通知日志表）。下游：12（视觉与设置 UI）。

---

## 1. 目标与非目标

**目标**
1. **可操作通知**：通知里能"标为已读 / 删除 / 稍后提醒"，点正文能定位到那封邮件（NOTIF-01/07）。
2. **不烦**：同会话聚合、2 秒去重、每账号/文件夹/发件人可静音、专注助手与全屏时自动不弹（NOTIF-03/06）。
3. **托盘常驻**：未读可见、菜单可操作、关窗口不退出、开机自启可选（NOTIF-02/04）。
4. **省电**：空闲时几乎零 CPU/网络（NOTIF-05），笔记本用电池时自动降频。

**非目标**
- 不做"通知中心历史"（Windows 已有；我们只保留自己的去重日志，用于避免重复弹）。
- 不做移动端推送、不做企业 IM 集成。
- 不做数字角标（Windows 平台不支持，见 §3.4 的现实）。

## 2. 用户可感知行为

| 场景 | 用户看到 |
|---|---|
| 收到新邮件（窗口在后台） | Windows 通知：`张三 · 项目周报` / `这周的进度和风险…` + 三个按钮"标为已读 / 删除 / 稍后"；点整个通知 → 窗口前置并打开该邮件 |
| 同一会话连来 5 封 | **只弹 1 条**：`张三（5 封新邮件）`，展开看最新一封 |
| 正在全屏看视频/演示 | **不弹**，只在托盘与叠加图标上显示未读；退出全屏后不补弹（避免刷屏），进"未读列表" |
| 专注助手/勿扰开启 | 同上（用 `SHQueryUserNotificationState` 判断），设置里可选"勿扰期间也要弹重要发件人" |
| 点托盘图标 | 唤出主窗口（左键）/ 菜单（右键）：打开主窗口、写新邮件、立即检查、暂停通知 1 小时、暂停同步、退出 |
| 右键某个文件夹 → 静音 | 该文件夹不再弹通知（角标仍计数），设置里可撤销 |
| 关闭主窗口 | 提示一次"Easy Email 会继续在后台收信（可在设置里改成直接退出）"→ 最小化到托盘 |
| 笔记本用电池 | 后台同步频率降低、正文预取暂停（可在设置里改），通知照常 |
| 开机 | 可选自启并静默到托盘（默认关，首次运行时问一次） |

## 3. 数据结构与平台现实

### 3.1 平台现实（决定了实现方式，不要再试更简单的路）

> ⚠️ 本节经独立评审修正了三处**会直接导致返工**的误解（标注"评审修正"）：**`SHQueryUserNotificationState`
> 不等于专注助手**、**WinAppSDK 通知有未计入的部署成本**、**冷启动路由不走命令行**。

| 能力 | 现实 | 我们的做法 |
|---|---|---|
| 通知按钮/点击路由 | **`tauri-plugin-notification` 在桌面端不支持 Actions**（官方标 "Mobile Only"，源码注明桌面端只取 title/body/icon/sound），也没有激活路由回调 | 用 Windows App SDK `Microsoft.Windows.AppNotifications`（`AddButton/AddArgument` + `NotificationInvoked`）。**未打包应用** `Register()` 会自行注册 COM 激活、无需手工 AUMID（这一点是对的） |
| **WinAppSDK 的部署成本（评审修正）** | 未打包应用必须**自己经 Bootstrapper API 初始化运行时**，且官方明确"**由开发者负责部署所需的 Windows App SDK 运行时包**"；**没有官方 Rust 投影**，要经 `windows` crate 手写 WinRT 互操作 | 二选一：①**框架依赖**（分发包里带运行时安装器/要求预装）②**自包含**（打进去，体积 +几十 MB，与 ≤20 MB 安装包预算冲突）。**M0 先做 2 天 spike 量成本**（能否从 Rust 调通、运行时怎么分发、通知能否带按钮），量完再定；无论哪条都要有 `capability()` 检查 + 回退到插件 |
| 提权运行时的通知 | 提权（管理员）应用**不能收发通知** | 安装器坚持 `currentUser`（17 §3）；检测到以管理员身份运行 → 明确提示 |
| **应用未运行时的点击（评审修正）** | Windows 点击通知会**启动应用**，激活载荷经 `NotificationInvoked` / App Lifecycle 送达，**不是命令行参数** | **不要**指望单实例插件转发 argv（会是空的）。做法：**启动最早期先注册 handler 再 `Register()`**，消费 pending activation；跨进程转发用 `AppInstance.FindOrRegisterForKey` + `RedirectActivationToAsync`（或命名管道）。`easyemail://` 深链只留给 `mailto:` / `.eml` |
| 任务栏数字角标 | **Windows 不支持数字角标**（`setBadgeCount` 明确 Unsupported），`setBadgeLabel` 仅 macOS | 用**叠加图标**（`setOverlayIcon`）：无未读=无图标，有未读=小红点；数字放托盘 tooltip 与菜单标题 |
| 跳转列表 | **Tauri 无支持**，需 Win32 `ICustomDestinationList`（必须在应用内调用） | 用 `windows` crate 自己写；M2 再做 |
| 托盘图标位置 | Win11 默认放进**溢出区**，且**只有用户能拖出来** | 首次运行提示一次；tooltip 带未读数兜底 |
| **免打扰/全屏抑制（评审修正）** | `SHQueryUserNotificationState` 的真实语义：`QUNS_NOT_PRESENT`（锁屏/屏保）、`QUNS_BUSY`（**全屏应用或演示设置**）、`QUNS_RUNNING_D3D_FULL_SCREEN`、`QUNS_PRESENTATION_MODE`、`QUNS_ACCEPTS_NOTIFICATIONS`、`QUNS_QUIET_TIME`（**新用户登录后第一个小时**，**不是专注助手**） | **抑制条件 = NOT_PRESENT / BUSY / RUNNING_D3D_FULL_SCREEN / PRESENTATION_MODE**；**专注助手交给 Windows 自己**（它会静音 toast 并送进操作中心），我们不去猜。
  备选方案（**待实测**）：若实测发现"专注助手开着时我们仍被计入打扰"，再评估 `FocusSessionManager`
  主动查询；在那之前**不做**（多一个 API 就多一份版本兼容风险）。`NotifyContext` 增加 `locked: bool`。这条列为**待实测**（按 A1 的纪律） |


### 3.2 设置与状态

```rust
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct NotifySettings {
    pub enabled: bool,                       // 总开关（默认 true）
    pub show_preview: bool,                  // 通知里是否显示正文摘要（默认 true，可在隐私场景关闭）
    pub play_sound: bool,                    // 默认 true
    pub actions: NotifyActions,              // 显示哪些按钮（默认 标为已读 + 稍后）
    pub respect_quiet_hours: bool,           // 尊重专注助手/全屏（默认 true）
    pub dnd_until: Option<i64>,              // "暂停通知"到某时刻
    pub aggregate_window_secs: u32,          // 同会话聚合窗口（默认 2）
    pub per_account: BTreeMap<String, AccountNotifyPolicy>,
    pub important_senders: Vec<String>,      // 勿扰期间仍然弹（可选）
    pub only_inbox: bool,                    // 只对收件箱通知（默认 true，其他文件夹靠规则 RULE-07）
    pub battery_saver: BatteryPolicy,        // 电池时的降频策略
}
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct AccountNotifyPolicy {
    pub enabled: bool,                       // 账号级开关
    pub muted_folders: Vec<String>,          // 静音文件夹（线路名）
    pub muted_senders: Vec<String>,          // 静音发件人（地址或域名）
    pub only_vip: bool,                      // 只对重要发件人
}
#[derive(Serialize, Deserialize, Clone, Copy, Debug)]
pub struct BatteryPolicy { pub reduce_sync: bool, pub pause_prefetch: bool }   // 默认 true/true
```

**去重与历史**（03 已有 `notification_log`）：同一 `(account, thread_id)` 在窗口内只弹一次；
`dnd_until` 期间**不记录为"已展示"**，退出勿扰后也不补弹（只在托盘计数），避免"上班一看 40 条通知"。

### 3.3 核心接口

```rust
// ── 通知 ──────────────────────────────────────────────────────
pub struct NotifyRequest {
    pub account_id: String, pub folder: String, pub uid: u32, pub thread_id: String,
    pub title: String, pub body: String, pub actions: Vec<NotifyAction>,
    pub launch_arg: String,          // easyemail://open?…
}
pub enum NotifyAction { MarkRead, Delete, Snooze(Duration), OpenInNewWindow }

pub trait Notifier: Send + Sync {
    /// 平台能力探测（提权/未注册/AUMID 是否可用）
    fn capability(&self) -> NotifyCapability;
    fn show(&self, req: &NotifyRequest) -> Result<NotifyHandle>;
    fn clear_all(&self) -> Result<()>;
}
pub struct WindowsNotifier { /* AppNotificationManager */ }
impl WindowsNotifier {
    pub fn register() -> Result<Self>;               // 未打包应用：Register() 自注册 COM
    pub fn on_invoked(cb: impl Fn(NotifyInvocation) + Send + 'static);   // 按钮/正文点击
}
pub enum NotifyInvocation { Body { launch_arg: String }, Action { name: String, launch_arg: String }, Dismissed { launch_arg: String } }

/// 决定"这条该不该弹、怎么弹"（纯逻辑，可单测）
pub fn decide(req: &NotifyRequest, s: &NotifySettings, ctx: &NotifyContext) -> NotifyDecision;
pub enum NotifyDecision { Show(NotifyRequest), Aggregate { thread_id: String, count: u32, latest: NotifyRequest }, Suppress(SuppressReason) }
pub enum SuppressReason { Disabled, MutedFolder, MutedSender, NotVip, Dnd, Fullscreen, Locked, AppFocused, Duplicate, Read, InitialSync, Merged(usize) }
// 评审修正：补 InitialSync（§5 与门禁 1 都断言它）；Aggregate 带上 latest，
// 否则 §3.4 的"最新一封的主题+摘要"与 §5 的"≤3 条 + 其他 N 封"根本无法表达。

pub struct NotifyContext {
    pub focused: bool,              // 主窗口是否在前台（在前台就不弹，只在列表里动）
    /// 来自 SHQueryUserNotificationState 的四态（**不是**专注助手）
    pub screen_state: ScreenState,  // Accepts | BusyOrFullscreen | PresentationMode | NotPresent
    pub locked: bool,               // 锁屏/屏保（QUNS_NOT_PRESENT）——锁屏时不显示正文摘要
    pub battery: bool, pub on_ac: bool,
    pub now: i64,
}

// ── 托盘与窗口生命周期 ────────────────────────────────────────
pub fn build_tray(app: &AppHandle, state: &AppState) -> Result<TrayIcon>;
pub fn update_tray(tray: &TrayIcon, unread: u32, paused: bool) -> Result<()>;   // tooltip + overlay
pub fn set_overlay_icon(app: &AppHandle, unread: u32) -> Result<()>;            // 0 → 清除
pub fn on_window_close_requested(app: &AppHandle) -> WindowAction;              // MinimizeToTray | Exit
pub fn create_jump_list(recent: &[JumpItem], prompts: bool) -> Result<()>;

// ── 自启与电源 ───────────────────────────────────────────────
pub fn set_autostart(enable: bool) -> Result<()>;        // tauri-plugin-autostart
pub fn autostart_enabled() -> Result<bool>;
pub fn on_power_source_changed(on_ac: bool, state: &AppState);   // 切电池 → 降频（04 的预算）
```

### 3.4 通知文案规则（照抄，别临场发挥）

| 情形 | 标题 | 正文 |
|---|---|---|
| 单封 | `{发件人} · {主题}` | `{摘要 ≤ 100 字}`（`show_preview=false` 时改为"你有 1 封新邮件"） |
| 同会话聚合 | `{发件人}（{n} 封新邮件）` | 标题取 `Aggregate.latest` 的主题（"王敏：十月报销单… 等 3 封"）+ 摘要 |
| 多账号统一模式 | `{账号显示名}：{发件人} · {主题}` | 同上（仅在统一收件箱模式且账号 > 1 时加前缀） |
| 规则命中"重要" | `⭐ {发件人} · {主题}` | 同上，且**勿扰期间仍然弹**（若用户在重要发件人列表里） |
| 按钮 | `标为已读` / `删除` | `稍后` （三种可选，默认前两个） |

## 4. 状态机与主流程

### 4.1 新邮件 → 通知
```
04 的同步落库 → 事件 mail:new(account, folder, thread_id, uids)
   ├ 规则引擎（10）可能改写：静音/重要/直接删除
   └ notifier 任务（全局 1 个，聚合窗口 2 s）
        ├ decide()：先看 focused / fullscreen / dnd / muted / battery
        │     ├ Suppress → 只更新托盘与叠加图标（未读仍计入）
        │     └ Show/Aggregate → WindowsNotifier.show()
        ├ 写 notification_log（供去重与"最近通知"统计）
        └ 更新托盘 tooltip + 叠加图标
用户点通知
   ├ 正文 → 解析 launch_arg → 主窗口前置 → 打开该会话并定位到该封（08 的定位高亮）
   ├ 按钮"标为已读" → 走 03 的 pending_ops（离线也能点，联网后收敛）→ 通知随系统消失
   └ 应用未运行 → deep-link 冷启动 → 同一处理路径（单实例保证不重复起进程）
```

### 4.2 窗口与进程生命周期
```
CloseRequested(主窗口)
   ├ 首次 → 弹一次说明（可在设置里改默认行为）→ 按设置执行
   ├ MinimizeToTray → prevent_close() + hide()；应用继续跑（IDLE 保持）
   └ Exit → 正常退出流程：暂停任务 → 发件箱保留 → 写 last_shutdown 时间 → 退出
托盘菜单"退出" → 同上（唯一"真退出"的入口之一）
开机自启 → 启动参数 --minimized → 直接进托盘，不抢焦点
```

### 4.3 电池与节能
```
on_power_source_changed(false /* 电池 */):
   ├ 同步：窗口回溯/正文预取 降频到 25%（04 的 BandwidthPolicy）
   ├ 附件预取：暂停
   ├ IDLE 窗口：维持（这是"新邮件及时性"，不能省）
   └ 设置里可见："当前在电池上，后台同步已降频（可在设置里改）"
插电恢复 → 立刻跑一次增量 + 恢复预取
```

## 5. 失败模式与降级

| 触发 | 用户看到 | 处理 |
|---|---|---|
| 通知注册失败（AppNotificationManager.Register 出错） | 设置页显示"系统通知不可用"，托盘与角标仍工作 | 回退到 `tauri-plugin-notification`（无按钮、无路由），并在诊断包里记录原因 |
| 以管理员身份运行 | 顶部提示"以管理员运行时无法显示通知" | 不重试、不刷屏；角标仍工作 |
| 通知权限被系统关闭 | 设置页红点 + "去系统设置打开通知"（跳 `ms-settings:notifications`） | 一次性提示 |
| 托盘图标被折叠 | 首次提示一次"在'显示隐藏的图标'里可以固定" | 不再提示 |
| 专注助手开启 | 不弹（符合预期） | 记录 Suppress(Dnd)，不补弹 |
| 同一秒来 200 封（首次同步） | **一条都不弹**（首次同步不通知，这是常识） | `decide()`：`reason=InitialSync` 直接 Suppress；只更新角标 |
| 通知里点了"删除"但离线 | 邮件本地删除 + 队列；托盘未读数立刻减 | 联网后收敛（03 §5.3 的破坏性操作要 conflict 检查：若服务器上已不存在 → 视为成功） |
| 多个账号同时来邮件 | 归并到 ≤3 条通知（超出合并为"其他 N 封"） | 聚合器全局去重窗口 |
| 用户点了通知但邮件已被删 | 打开列表并提示"这封邮件已被移动或删除" | 定位失败的可解释路径 |

## 6. 迁移与兼容

- v0.2 已有：托盘（三菜单项）+ Rust 侧 Windows 通知 + 前端 `mail:new` 监听。**全部保留语义**，
  升级点：托盘菜单扩项、通知换成 AppNotifications（带按钮与路由）、叠加图标、聚合与静音。
- `notification_log` 是新表（03 v3 迁移里）。
- 设置项新增后，老 `settings.json` 缺失字段用默认值（`serde(default)`）。
- **不改变**既有行为：首次同步不通知、只监听 INBOX 的默认（多文件夹监听在 04 里放开，但通知默认仍只对收件箱）。

## 7. 性能预算

| 指标 | 预算 |
|---|---|
| 从落库到通知出现 | ≤ 300 ms |
| 通知路径 CPU（1000 封/小时） | 可忽略（事件驱动，无轮询） |
| 空闲 CPU（3 账号 IDLE + 托盘） | ≤ 0.5% |
| 空闲内存（通知+托盘模块） | ≤ 20 MB |
| 托盘 tooltip/图标更新 | ≤ 50 ms，节流 ≥ 1 s |
| 电池模式下的网络流量 | 相对插电 ≤ 25% |

## 8. 测试计划

| 层 | 用例 |
|---|---|
| 单元 | `decide()` 的真值表：focused/fullscreen/dnd/muted/sender/thread 聚合/首次同步/重要发件人（≥ 25 条用例，快照测试） |
| 集成（Windows） | 通知注册、按钮回调、点击路由（应用在运行/未运行两种）、冷启动 deep-link 解析、叠加图标在 0/1/99 三种值下的表现 |
| 托盘 | 图标创建、菜单项动作、tooltip 未读数、关窗口不退出、退出路径、托盘菜单"暂停通知 1 小时"的到期恢复 |
| 电源 | 模拟切电池（`PowerSettingRegisterNotification`）→ 断言同步降频；插电恢复 |
| 通知不打扰 | 全屏视频、专注助手开启、勿扰时段三种情况下都不弹 |
| 真机 | Win10 22H2 / Win11 23H2 各一台：通知样式、按钮、溢出区（人工） |
| 长稳 | 连续 24 h 收信（1 封/分钟）不断言内存增长（RSS 曲线平坦） |

## 9. 门禁新增项

1. **首次同步不通知**：单测断言 `InitialSync` → `Suppress`（这条很容易被后来者"顺手"改坏）。
2. **提权提示存在**：源码检查含"管理员/通知不可用"的提示路径（防止换实现时丢掉）。
3. **通知参数格式**：`launch_arg` 生成与解析在同一模块，且单测覆盖往返（防止点击打不开）。
4. **不做数字角标**：源码中不得出现 `setBadgeCount`（Windows 不支持；出现即误导）。
5. **静音默认值**：`only_inbox == true` 与 `respect_quiet_hours == true` 作为默认值被单测钉住。

## 10. 逆向反驳（红队）

**反驳 1：为了通知按钮去引入 Windows App SDK，值吗？**
- 证据：`tauri-plugin-notification` 桌面端**确实**不支持 Actions 与激活路由（官方文档与源码都写明）。
- 取舍：邮件客户端"点通知直接处理"是高频动作（每天几十次），一个只能看不能动的通知在现代客户端里算缺功能；
  而 AppNotifications 在**未打包应用**里只需 `Register()`，没有额外的清单/打包负担。
- 让步：**双实现**（AppNotifications 优先，失败回退插件）+ 设置里可关闭按钮；
  并且这条路必须**真机验收**（不是靠文档）。

**反驳 2：静音/勿扰/全屏判断越多，越可能出现"该弹的不弹"（漏报比骚扰更糟）。**
- 缓解：①默认只在"主窗口不在前台"时才考虑弹；②`focused=false` 且非全屏非勿扰 ⇒ **默认弹**；
  ③所有 Suppress 都记进诊断日志（用户说"我没收到通知"时能立刻查出原因）；
  ④设置页提供"测试通知"按钮 + "最近 10 条通知为什么没弹"的可见列表。
- 让步：`important_senders` 白名单可穿透勿扰（默认空）。

**反驳 3：聚合 2 秒窗口会让"5 封新邮件"只看到一条，用户可能漏读某一封。**
- 缓解：聚合通知标题写 `(5 封新邮件)`，点开进**会话**（不是单封）并高亮未读；托盘角标与列表未读都没丢。
- 让步：聚合窗口可配（0 = 不聚合）。

**反驳 4：电池降频会让人觉得"邮件来得慢"，与"快"的卖点冲突。**
- 证据：邮件的新鲜度主要靠 IDLE（不降频），降的只是**历史回溯与正文预取**这类后台工作。
- 让步：设置里可关（`battery_saver` 两个开关都可关），并在状态栏显示当前策略。

**反驳 5：托盘常驻 + 关窗口不退出，会被用户当成"流氓软件"（关不掉）。**
- **评审修正：原来的理由（"与 QQ/微信/Outlook 一致，是邮件客户端的行业惯例"）是错的** ——
  **Outlook 与 Thunderbird 关窗口就是退出**，而我们正是要跟它们抢用户。
- 改设计：**默认"关闭窗口 = 退出"**（与竞品一致）；"最小化到托盘/后台收信"是**显式开关**（设置里默认关，
  首次关闭时给一次勾选机会，勾了以后才常驻）。托盘依然存在（用于最小化/后台收信/未读提示），
  但不再假装"关不掉是行业惯例"。
- 保留：托盘菜单第一项"打开主窗口"、最后一项"退出"；退出前若有未发送队列，明确提示。


**反驳 6：通知内容可能泄露隐私（锁屏可见正文摘要）。**
- 缓解：`show_preview` 默认开（可用性优先），但设置里有明确开关；
  并且当系统处于锁屏/勿扰时（`SHQueryUserNotificationState`）不显示摘要、只显示"你有新邮件"。
- 让步：M2 可加"仅在解锁时显示摘要"的更细策略。

## 11. 未决问题

| # | 问题 | 影响 | 建议 |
|---|---|---|---|
| N1 | 通知按钮默认显示哪几个 | 打扰 vs 便利 | 默认"标为已读 + 稍后"，删除放自定义 |
| N2 | 默认是否只对 INBOX 通知 | 噪音 | 是（其他文件夹靠规则 RULE-07 打开） |
| N3 | 关窗口默认行为 | 用户预期 | 最小化到托盘（首次解释一次） |
| N4 | 聚合窗口 2 s 是否合适 | 体感 | 先用 2 s，实测后调 |
| N5 | 是否做"最近通知"历史视图 | 排查与回溯 | M2 做（数据已在 `notification_log`） |
| N6 | 跳转列表是否进 M1 | 成本 S | 进 M2（属于加分项，不影响门槛） |

## 12. 验收清单

- [ ] 通知带按钮，三个动作都能在**离线**状态下生效并正确收敛。
- [ ] 点击通知（应用运行中/未运行）都能打开对应会话并定位到那封。
- [ ] 全屏/专注助手/勿扰时不弹，且诊断日志能说明"为什么没弹"。
- [ ] 首次同步、静音文件夹、静音发件人都不弹（单测 + 真机各验一次）。
- [ ] 托盘 tooltip 未读数、叠加图标、菜单动作、关窗口不退出、退出干净。
- [ ] 电池降频生效且可关；插电后立刻恢复。
- [ ] 24 h 长稳：RSS 与句柄数不增长，通知无重复。
- [ ] `check.ps1` 五项新增检查通过。

> **章节名对照（模板 13 节）**：接口→§3.3 核心接口。
> 内容齐全，只是节名与模板不完全同名（见 design/README.md 的说明）。
