# 17 交付与分发（安装、签名、更新、上架）

> **目标**：让用户"敢下载、敢安装、装了能一直更新"，并且卸载干净、可换机器。
> 这一步做不好，前面所有功能都没人看见。
>
> 覆盖 ID：DIST-01…09、SEC-15（更新包签名校验）、REL-10（通道与灰度）。
> 事实依据：[`A1-research-findings.md`](A1-research-findings.md)（本节所有版本号与政策均来自 2026-10-03 的核对）。

---

## 1. 目标与非目标

**目标**
1. 双击安装、**不弹 UAC**、装完给开始菜单/桌面快捷方式、能卸载干净。
2. **自动更新**默认开、安静、签名校验、失败不影响使用。
3. **不出 SmartScreen 恐吓**（需要代码签名 + 声誉积累，见 §6）。
4. **注册为系统邮件程序**（`.eml` + `mailto:`），并在首次运行时引导用户去设置里选我们。
5. 免费渠道铺开：winget 上架 + 便携版 zip + （后期）微软商店。
6. 中国大陆可下载、可更新（诚实说明 ICP 与 CDN 的现实，见 §7）。

**非目标**
- 不做 MSIX/Store 打包（Tauri 不支持生成 MSIX；Store 走"EXE/MSI 产品"路线，见 §8）。
- 不做企业 MSI/GPO 部署（M3 评估；一旦发 MSI 就必须同时发两个 target key，见 §4.2）。
- 不做静默自动重启（更新后由用户选，见 §5）。

## 2. 用户可感知行为

| 场景 | 用户看到 |
|---|---|
| 下载安装包 | `EasyEmail_1.0.0_x64-setup.exe`（约 12-20 MB，NSIS），双击 → 无 UAC → 装到 `%LOCALAPPDATA%\Easy Email` → 自动启动 |
| 机器没有 WebView2 | 安装时静默装引导器（需要联网，约 +1.8 MB 包体）；**离线安装包另发**（+127 MB，标 `-offline`） |
| 首次运行 | 向导："设为默认邮件程序"按钮 → 跳 Windows 设置页并高亮我们（系统不允许程序自己抢默认） |
| 有新版本 | 托盘气泡/应用内横幅："1.0.1 已下载，重启后生效"（可"稍后"、"查看更新说明"） |
| 点更新安装 | 应用自行退出 → NSIS 静默安装（有进度窗，无需交互）→ 自动重启 |
| 更新失败 | "更新失败，已回退到当前版本"+ 一行原因；当前版本仍可用；日志进诊断包 |
| 卸载 | 卸载器里有一个**默认不勾**的"同时删除本地数据（草稿、规则、联系人、缓存）" |
| 便携版 | zip 解压即用，数据在程序目录旁的 `data/`；**更新需手动替换**（UI 里明说）。**评审补**：还要把 WebView2 的 `EBWebView` 数据目录也指到 `<exe>\data\`（用 `WEBVIEW2_USER_DATA_FOLDER`），否则"便携"仍然会往 `%LOCALAPPDATA%` 写东西 |
| 离线/墙内 | 更新检查失败**不打扰**；设置里可换更新镜像地址 |

## 3. 配置与数据结构

### 3.1 `tauri.conf.json` 关键段（最终形态）

```jsonc
{
  "bundle": {
    "active": true,
    "targets": ["nsis"],                       // M1 只发 NSIS；发 MSI 时必须同时发两个 target key（§4.2）
    "createUpdaterArtifacts": true,            // 产出 *.sig
    "windows": {
      "webviewInstallMode": { "type": "embedBootstrapper", "silent": true },  // +1.8MB，需联网
      // **评审修正**：这里必须落到实际方案，不能留 null + 只在正文里写"用 artifact-signing-cli"。
      // 二选一：①OV 证书 → 填 certificateThumbprint；②Azure Artifact Signing → 用自定义 signCommand。
      "certificateThumbprint": "<OV 证书指纹，或删掉本行改用 signCommand>",
      "digestAlgorithm": "sha256",
      "timestampUrl": "http://timestamp.digicert.com",
      "nsis": {
        "installMode": "currentUser",          // 关键：不弹 UAC，且"提权应用无法发通知"（§9）
        "displayLanguageSelector": false,
        "languages": ["SimpChinese", "English"],
        "installerIcon": "icons/icon.ico",
        // **评审修正**：不要 fork 整个 NSIS 模板（每次 Tauri 升级都要重新合并）；
        // 用 Tauri v2 的 `installerHooks` 指向一个 .nsh（里面有 NSIS_HOOK_POSTINSTALL / PREUNINSTALL）
        "installerHooks": "installer/hooks.nsh"
      }
    },
    "fileAssociations": [
      { "ext": ["eml"], "mimeType": "message/rfc822", "role": "Editor",
        "name": "Easy Email 邮件", "description": "电子邮件文件" }
    ],
    "resources": [],
    "icon": ["icons/icon.ico"]
  },
  "plugins": {
    "updater": {
      // **评审修正**：`{{target}}/{{arch}}/{{current_version}}` 是 Tauri 的**动态服务端**形状
      //（服务端按路径返回 204/200）；我们选的是 GitHub Releases 静态托管，它**没法**为每个历史版本
      // 生成一条路径。静态形式就是"最新版清单一处、两个 key 都写在里面"：
      "endpoints": ["https://github.com/<owner>/<repo>/releases/latest/download/latest.json"],
      "pubkey": "<minisign 公钥，直接写在这里，不能是文件路径>",
      "windows": { "installMode": "passive" },   // 默认值；有进度窗、无需交互、可自提权
      "dialog": false                            // 自定义 UI，不用插件自带弹窗
    },
    "deep-link": { "desktop": { "schemes": ["easyemail"] } }
  }
}
```

### 3.2 更新清单（静态 JSON，v1 用；`latest.json`）

```jsonc
{
  "version": "1.0.1",
  "notes": "修复了…；新增…",
  "pub_date": "2026-10-03T12:00:00Z",
  "platforms": {
    // 关键：两个 key 都要发。只发 windows-x86_64 会让 MSI 安装的机器拿到 NSIS 包（反之亦然），
    // 造成"更新循环"。`bundle_type` 由插件按当前安装方式自动推断。
    "windows-x86_64-nsis": { "signature": "<minisign 签名>", "url": "https://<host>/…/EasyEmail_1.0.1_x64-setup.exe" },
    "windows-x86_64-msi":  { "signature": "<…>",             "url": "https://<host>/…/EasyEmail_1.0.1_x64_en-US.msi" }
  }
}
```
**通道**：`stable` / `preview` 两个目录下各放一份清单，端点里用 `{{target}}/{{arch}}/{{current_version}}`。
灰度/按比例放量需要**动态端点**（服务器根据客户端上报的匿名版本做 204/200），M2 再做（REL-10）。

### 3.3 NSIS 自定义 hook（**必须自己写，Tauri 不做这部分**）

Tauri 的 NSIS 只写 `SHCTX\Software\Classes\mailto`，**不写 `RegisteredApplications` / `Capabilities`**，
所以系统"默认应用"列表里**看不到我们**。安装器必须补上（`installer/installer.nsi` 里 `NSIS_HOOK_POSTINSTALL`）：

```nsis
; 文件名：installer/hooks.nsh（由 tauri.conf.json 的 installerHooks 引入）
!macro NSIS_HOOK_POSTINSTALL
  ; 让 Windows"默认应用"页认识我们（否则用户想设默认也找不到）
  WriteRegStr SHCTX "Software\RegisteredApplications" "Easy Email" "Software\Easy Email\Capabilities"
  WriteRegStr SHCTX "Software\Easy Email\Capabilities" "ApplicationName" "Easy Email"
  WriteRegStr SHCTX "Software\Easy Email\Capabilities" "ApplicationDescription" "轻量、本地优先的邮件客户端"
  WriteRegStr SHCTX "Software\Easy Email\Capabilities\URLAssociations" "mailto" "EasyEmail.mailto"
  WriteRegStr SHCTX "Software\Easy Email\Capabilities\FileAssociations" ".eml" "EasyEmail.eml"
  WriteRegStr SHCTX "Software\Classes\EasyEmail.mailto\shell\open\command" "" '"$INSTDIR\easy-email.exe" "%1"'
  WriteRegStr SHCTX "Software\Classes\EasyEmail.eml\shell\open\command" "" '"$INSTDIR\easy-email.exe" "%1"'
!macroend

!macro NSIS_HOOK_PREUNINSTALL
  ; **评审修正**：`RegisteredApplications` 下我们写的是**值**（name → capabilities 路径），
  ; 所以卸载要用 DeleteRegValue；原来写的 DeleteRegKey 是空操作 ⇒ 应用会一直留在"默认应用"列表里
  DeleteRegValue SHCTX "Software\RegisteredApplications" "Easy Email"
  DeleteRegKey SHCTX "Software\Easy Email"
  DeleteRegKey SHCTX "Software\Classes\EasyEmail.mailto"
  DeleteRegKey SHCTX "Software\Classes\EasyEmail.eml"
!macroend
```
> ⚠️ `RegisteredApplications` 是**值**（name→capabilities 路径），不是子键；实现时用 `DeleteRegValue`。
> 若 `installMode` 改成 `perMachine`，`SHCTX` 会是 HKLM —— 但**提权安装的应用发不出通知**（§9），
> 所以本设计坚持 `currentUser`。

### 3.4 数据布局（与 03 的双库设计对齐）

```
%LOCALAPPDATA%\Easy Email\            （installMode=currentUser 的默认安装目录）
  easy-email.exe
  uninstall.exe
%APPDATA%\com.easyemail.desktop\      （Tauri 的 app_config_dir，与既有 v0.2 一致）
  settings.json     用户设置
  accounts.json     账号（非敏感）
  cache.db          可删缓存（03）
  user.db           不可删：草稿/队列/规则/联系人/标签
  logs\             ≤ 7 天轮转
  updates\          下载的更新包（安装后清理）
  startup-error.log
```
**卸载时**：WebView2 的 `EBWebView` profile 目录在 `%LOCALAPPDATA%\com.easyemail.desktop\EBWebView`，
Tauri 卸载器勾选"删除应用数据"时会一并 `RmDir`（A1 研究已确认行为）。**"删除应用数据"必须重命名成
"同时删除本地邮件缓存与草稿（不可恢复）"并默认不勾**，因为里面有 `user.db`（红线 3）。

## 4. 接口（函数级）

### 4.1 更新模块（`src-tauri/src/update.rs`）

```rust
#[derive(Clone, Debug, serde::Serialize)]
pub enum Channel { Stable, Preview }

#[derive(Clone, Debug)]
pub struct UpdateConfig {
    pub channel: Channel,
    pub auto_check: bool,          // 默认 true
    pub auto_download: bool,       // 默认 true（下载不打扰；安装前问）
    pub endpoint_override: Option<String>,   // 设置里的"更新镜像"
    pub check_interval: Duration,  // 默认 6 h
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct UpdateInfo { pub version: String, pub notes: String, pub pub_date: Option<String>, pub url: String, pub signature: String }

pub async fn check(app: &AppHandle, cfg: &UpdateConfig) -> Result<Option<UpdateInfo>>;
pub async fn download(app: &AppHandle, info: &UpdateInfo, sink: ProgressSink) -> Result<PathBuf>;
/// 安装：**会退出应用**（Windows 安装器限制）。返回前不会继续执行。
pub async fn install_and_restart(app: &AppHandle, bytes: Vec<u8>) -> Result<()>;
/// 用户选"稍后"：记录待安装，下次启动或用户点"立即重启"时执行。
pub async fn defer_install(app: &AppHandle, path: PathBuf) -> Result<()>;
pub async fn take_deferred_install(app: &AppHandle) -> Result<Option<PathBuf>>;

/// 启动时的"更新后首启"钩子：显示新版本说明、清理旧更新包、修正快捷方式 AUMID。
pub async fn post_update_startup(app: &AppHandle) -> Result<PostUpdateReport>;
```

### 4.2 默认程序引导（`src-tauri/src/win_integration.rs`）

```rust
/// 打开系统"默认应用"页并定位到我们（Windows 11 21H2/22H2 + 2023-04 CU 起支持）
/// 注意：**没有任何 API 能程序化把自己设成默认**（微软自 Win8 起废弃），只能引导。
pub fn open_default_apps_settings(app: &AppHandle) -> Result<()>;   // ms-settings:defaultapps?registeredAppUser=Easy%20Email
/// 当前是否已是 .eml / mailto 的默认处理程序（读 UserChoice，只读判断，不修改）
pub fn is_default_mail_handler() -> DefaultStatus;   // Mailto(bool) + Eml(bool) + RegisteredCapabilities(bool)
pub fn register_capabilities() -> Result<()>;        // 便携版没有安装器，运行时补写 HKCU（需用户同意）
pub fn create_jump_list(recent: &[JumpItem], unread: u32) -> Result<()>;  // ICustomDestinationList，见 §9
pub fn set_taskbar_overlay(unread: u32) -> Result<()>;                    // Tauri setOverlayIcon 可用；数字角标不可用
```

### 4.3 便携版与首次运行检查

```rust
/// 便携模式：数据放 <exe 同目录>\data，并在设置里标记 "portable"（不写 HKCU 关联、不注册自启）
pub fn portable_mode() -> bool;
/// 启动自检：WebView2 版本、磁盘空间、是否是便携版、是否被 Smart App Control 拦过（尽力）
pub fn startup_checks() -> Vec<StartupCheck>;
```

## 5. 状态机：安装 → 更新

```
安装（NSIS currentUser）
  ├ 检查/安装 WebView2（embedBootstrapper；失败→提示"请先安装 WebView2"或改用 -offline 包）
  ├ 复制文件 → 写 HKCU 关联与 Capabilities → 建快捷方式（设 AUMID）→ 启动
  └ 失败任一步 → 回滚安装目录 + 明确错误码

更新检查（每 6 h + 启动后 30 s）
  ├ 200 有新版本 → 下载（可暂停）→ 校验 minisign 签名（**不可关闭**）
  │     ├ 校验失败 → 丢弃 + 日志 + 不提示用户（防中间人/损坏；只在诊断包里可见）
  │     └ 成功 → 通知"已下载，重启生效"（横幅，可稍后）
  ├ 204 / 无新版本 → 静默
  └ 网络/解析失败 → 静默（离线不打扰），连续失败 3 次才在设置页显示"更新检查失败"
安装
  ├ 用户点"立即重启更新" → 退出应用 → 运行 NSIS（passive）→ 安装器结束 → 应用重启
  │     └ 安装失败 → 安装器退出码非 0 → post_update_startup 报"更新失败，仍是旧版本"
  └ 用户点"稍后" → 记 deferred → 下次启动执行（用户可在设置里永久忽略该版本）
```

## 6. 代码签名（这一步决定"敢不敢运行"）

**事实（A1 核对，2026-08 微软官方对比页）**
| 方案 | 价格 | 资格 | 说明 |
|---|---|---|---|
| **Azure Artifact Signing**（原 Trusted Signing） | **≈ $9.99/月** | 公共信任组织需在美/加/欧/英/澳/新/日/韩/新/瑞士/挪/以；**个人仅限美国与加拿大** | 无硬件令牌、CI 原生、身份验证几个工作日；**没有中国大陆区域**（最近日韩），签名流量出境 |
| **OV 证书** | **$150-300/年** | 全球，个人可买 | 2023-06 起私钥必须放 FIPS 140-2 L2 硬件令牌或云 HSM |
| **EV 证书** | $400+/年 | 全球 | **2024 年起 EV 不再跳过 SmartScreen**，与 OV 一样靠声誉积累 ⇒ **不要为 SmartScreen 买 EV** |

**决策（**P3 已拍板：不花钱**）**：
> 用户的约束是"**不花钱而且有效**"。所以下面三条是结论，不是选项：
> 1. **v1.0 不买证书、也不自签** —— 自签对 SmartScreen **无效**（照样"未知发布者"），
>    而"自签"这个特征本身更像恶意软件 ⇒ **自签比不签更糟**。
> 2. **开源的免费正路 = SignPath Foundation**（§6.2，条款已逐条核对）：证书签发给基金会、
>    要求"已发布 + 可验证声誉"，所以顺序必然是**先发布、攒声誉、再申请**；且**证书上的"发布者"
>    是 SignPath Foundation，不是个人名字**（诚实写在下载页）。
> 3. **不签名时的最佳实践（免费且有效）**：①下载页/README 给"首次运行看到 SmartScreen 怎么办"的图文步骤；
>    ②发布 SHA256 + 一条校验命令；③优先引导用户从 **winget** 安装（会校验安装包哈希，路径更正规）；
>    ④安装包不捆绑任何东西、不装驱动、不装后台服务；⑤**更新链的完整性不依赖 Authenticode** ——
>    Tauri updater 的 minisign 签名独立生效，所以不签名**不削弱自动更新的安全**，只影响首次下载体验。
>
> **如果将来开源（P4）**：M2 期间按 §6.2 申请 SignPath；拿到后所有发布（exe/安装器/卸载器）统一签名，
> `bundle.windows.certificateThumbprint` 换成自定义 `signCommand`（SignPath 走 CI 签名服务）。
> **仍然不买 OV/EV**（EV 自 2024 起也不再免 SmartScreen）。

**无论签不签，下面几条都成立**：
- **Authenticode 与 updater 的 minisign 是两件事**：前者给系统信任，后者给更新完整性；后者**必须有**。
- **头几个版本即使签名也会被 SmartScreen 警告**（声誉从零开始）；未签名则**一直**警告 —— 这是现实，
  UI/下载页里给一句说明，而不是让用户自己撞上。
- **Windows 11 Smart App Control 会拦未签名程序**：这条要写进"已知限制"（有一部分用户就是装不上，
  除非他们关掉 SAC —— 我们不建议用户关）。


### 6.2 免费路线：SignPath Foundation（**P3 已拍板走这条，前提是 P4 开源**）

**条款逐条核对**（[signpath.org/terms.html](https://signpath.org/terms.html)，2026-10-03 读）：

| 条件 | 我们能否满足 | 备注 |
|---|---|---|
| OSI 认可的开源许可，且**无商业双许可** | **取决于 P4**（开源才满足） | 闭源 ⇒ 这条路直接没有 |
| 不含任何专有组件（自己的或关联方的） | 能满足（依赖全是 MIT/Apache/ISC 一类） | CI 生成一份依赖许可清单备用 |
| 活跃维护 + **已经发布** | 需要先发 M1a | ⇒ 顺序是"先发布 → 后申请" |
| 有**可验证的声誉** | **最大不确定性**：官方原话是"对可执行程序，我们要求一定程度的可验证声誉" | 所以要在发布页积累下载与反馈再申请 |
| 团队用 MFA（SignPath 与代码仓库） | 能满足（单人也要开） | 现在就开 |
| 指定 authors / reviewers / approvers | 单人项目要**自组织出这三个角色**（可以是同一人，但必须写清） | 写进 Code signing policy |
| 首页要有 "Code signing policy" 段 | 能满足（README 加一节，条款给了固定措辞） | 需注明"Free code signing provided by SignPath.io, certificate by SignPath Foundation" |
| 产物元数据（product name/version 一致） | 能满足（Tauri 产物天然一致） | 用 SignPath 的 metadata restrictions |
| **每次发布需人工批准** | 接受（正好符合"发布要人工过一遍"的纪律） | — |
| 证书签发给 **SignPath Foundation** | 接受：**"发布者"显示为 SignPath Foundation**，不是个人名字 | 诚实写在下载页 |

**结论**：**M1a 先以"未签名"发布**（§6 的三条免费最佳实践），**M2 期间按上表申请 SignPath**。
**不买 OV、不自签**（理由见 `DECISIONS` D15）。

## 7. 分发渠道与大陆可达性

| 渠道 | 成本 | 门槛 | 备注 |
|---|---|---|---|
| **官网/GitHub Releases 直接下载** | 0 | 无 | 大陆可达性一般（未核实，不写死）；建议同时给国内镜像 |
| **国内 CDN/OSS** | 流量费 | **域名需 ICP 备案**（解析到大陆服务器就必须） | 备案是流程成本，不是技术问题；这也是"更新清单托管"要提前决定的原因 |
| **winget** | 0 | 多文件 YAML 清单 + PR 到 `microsoft/winget-pkgs` | 用 `wingetcreate new` 生成、`winget validate` 校验；安装器必须支持静默（NSIS `/S`）；合并约几小时 |
| **微软商店（"EXE 或 MSI 应用"路线）** | 0 | 开发者账号**现免费**（个人需身份证件+自拍验证） | 商店不生成 MSIX，只登记链接：要求**离线 WebView2** + 自更新 + 代码签名 + **静默安装**；否则被拒（10.2.9.2） |
| **便携版 zip** | 0 | `tauri build --no-bundle` + 打包 | **无法自更新**（更新器只会跑安装器）⇒ UI 明说"手动替换" |

**决策**：v1 = GitHub Releases + winget + 便携版；国内镜像作为"可配置的更新端点"（设置项），
不默认启用（避免备案与运维）。商店放 M3。

## 8. 失败模式与降级

| 触发 | 用户看到 | 影响 | 处理 |
|---|---|---|---|
| 无 WebView2 且无网络 | 安装器提示"需要联网安装 WebView2，或下载 -offline 包" | 装不上 | 官网显眼处提供 offline 包（+127 MB） |
| 更新包签名校验失败 | **无提示**（只在设置页与诊断包显示"上次更新校验失败"） | 不更新 | 丢弃 + 拉黑该版本 + 下次检查 |
| 更新安装失败（安装器非 0） | "更新失败，仍是 1.0.0"+ 原因 | 不影响使用 | 记录 + 允许重试；连续失败则关闭自动更新并提示手动下载 |
| MSI/NSIS 交叉更新 | 可能出现"更新完还是旧版本"循环 | 严重 | **只发 NSIS**（M1）；发 MSI 时两个 target key 都发（§3.2） |
| 私钥丢失 | 无法再给已安装用户推送更新（只能让他们手动重装） | **致命** | 私钥离线备份两份（密码管理器 + 纸质/离线盘）；公钥写进 conf 后**永不变更**（变更=换 key，需用户手动装一次） |
| 大陆更新检查慢/失败 | 无感（静默） | 延迟更新 | 提供镜像端点设置；失败不打扰 |
| 用户把默认程序改回去 | 下次启动时提示一次"当前不是默认邮件程序"（可永久忽略） | 无 | 不反复骚扰 |
| 卸载勾选"删除数据" | 二次确认 + 明确列出会删什么 | 数据丢失 | 默认不勾；`user.db` 存在时二次确认文案加强 |
| 提权安装 | —— | **通知全部失效**（提权应用不能发通知） | 坚持 `currentUser`；若用户手动提权运行，UI 提示"以管理员运行时无法显示新邮件通知" |

## 9. Windows 集成的现实（研究给出的硬限制，设计必须绕开）

| 想要 | 现实（A1 已核实） | 我们的做法 |
|---|---|---|
| 通知里带"标已读/删除"按钮 | **tauri-plugin-notification 在桌面端不支持 Actions**（只有移动端），也没有点击路由 | 用 Windows App SDK `Microsoft.Windows.AppNotifications`（`AppNotificationBuilder.AddButton` + `NotificationInvoked`）。未打包应用调 `Register()` 即可，无需手工 AUMID |
| 点通知打开某封邮件 | 需要激活路由 + 单实例 | `single-instance` + `deep-link`：通知带参数 `easyemail://open?account=..&folder=..&uid=..`，进程已在则转发给主窗口 |
| 任务栏数字角标 | **Windows 不支持数字角标**（`setBadgeCount` 明确 Unsupported） | 用**叠加图标**（`setOverlayIcon`，Windows 专属）显示"有未读"；数字放 tooltip 与托盘菜单 |
| 跳转列表（最近文件夹/写新邮件） | **Tauri 没有支持**（开放请求 #14699） | 用 `windows` crate 调 `ICustomDestinationList`（必须在应用内调用，安装器不能代劳） |
| 把自己设成 mailto 默认 | **微软自 Win8 起取消程序化设置默认** | 注册 Capabilities + 引导到 `ms-settings:defaultapps?registeredAppUser=Easy%20Email` |
| 托盘图标常驻可见 | Win11 默认把新图标放进**溢出区**，且**只有用户能把它拖出来** | 首次运行时提示一次"如果没看到托盘图标，请到'显示隐藏的图标'里固定"；不反复提示 |
| 关掉窗口后继续收信 | 需要显式阻止退出 | `CloseRequested` → `prevent_close()` + 隐藏；退出只能走托盘菜单（首次关闭时提示一次，可改设置） |

## 10. 性能预算

| 指标 | 预算 |
|---|---|
| 安装包体积（NSIS + embedBootstrapper） | ≤ 20 MB（offline 包 ≤ 150 MB） |
| 安装耗时（SSD） | ≤ 15 s（不含 WebView2 下载） |
| 卸载耗时 | ≤ 5 s |
| 更新包体积 | ≤ 25 MB（增量更新不做，M3 评估） |
| 更新下载+安装 | 100 Mbps 下 ≤ 30 s |
| 启动时更新检查 | 不阻塞首屏（30 s 后异步） |

## 11. 测试计划

| 场景 | 方法 |
|---|---|
| 全新安装（有/无 WebView2，有/无网络） | 干净 Windows 10 22H2 与 Windows 11 23H2 各一台（或快照） |
| 静默安装 `/S` | 脚本验证 + 装完能启动（winget/商店的前置条件） |
| 升级安装（同版本、跨版本） | 覆盖安装不丢 `user.db`/`settings.json`；快捷方式 AUMID 保持 |
| 更新全链路 | 本地静态清单 + 自签测试 key：检查 → 下载 → 校验失败（篡改包）→ 校验成功 → 安装 → 重启 → 版本变化 |
| 回滚 | 安装一个"坏"更新（安装器返回非 0）→ 断言旧版本仍可启动 |
| 关联与默认程序 | 安装后 `.eml` 双击、`mailto:` 点击、"默认应用"页能否看到我们、设置为默认后行为正确 |
| 通知 | 未打包应用发带按钮的通知、点击路由到具体邮件、专注助手开启时不弹 |
| 卸载 | 勾选/不勾选"删除数据"两条路径；注册表无残留（Capabilities/Classes/卸载项） |
| 大陆网络 | 更新检查在超时/丢包下的静默行为；镜像端点可用性 |
| 便携版 | zip 解压即用、数据落在 `data/`、不写 HKCU、UI 明示手动更新 |

## 12. 门禁新增项

1. **`tauri.conf.json` 不变量**：`installMode == "currentUser"`、`targets` 只含 nsis（除非同时提供两个 target key）、
   `pubkey` 非空、`createUpdaterArtifacts == true`（源码/JSON 检查）。
2. **签名私钥不入库**：仓库与 CI 配置里不得出现 `TAURI_SIGNING_PRIVATE_KEY` 的字面值（正则扫描）；
   只允许来自环境变量/密钥库。
2b. **产物必须真的签了**：CI 里对 exe 与安装器跑 `signtool verify /pa`，未签名/签名失败即失败
   （评审：原门禁只 grep 私钥字面量 ⇒ 一个"忘了签名"的发布会绿着通过，而 Win11 Smart App Control 会拦用户）。
3. **更新清单双 key**：CI 脚本生成清单后校验 —— 若产物里有 msi，则 `windows-x86_64-msi` 必须存在。
4. **NSIS hook 存在且含 Capabilities 写入**：检查 `installer/installer.nsi` 含 `RegisteredApplications`。
5. **版本一致性**：`Cargo.toml` / `tauri.conf.json` / 更新清单版本号三者一致（CI 校验）。

## 13. 逆向反驳（红队）

**反驳 1：`currentUser` 安装装到 `%LOCALAPPDATA%`，感觉"不正经"，企业也不喜欢。**
- 证据：Program Files 更符合传统预期。
- 但：①提权应用**发不出通知**（研究已核实）——一个不能弹新邮件提醒的邮件客户端是残废；
  ②`currentUser` 无需 UAC，安装成功率显著更高；③msi/perMachine 的部署需求可以用后期的 MSI 单独满足。
- 让步：M3 提供 `perMachine` 变体（同一个安装器 `installMode: "both"` 让用户选），但**默认保持 currentUser**，
  并在文档里写清代价。

**反驳 2：自动更新是最大的攻击面，为了省事引入它值得吗？**
- 缓解：minisign **强制签名校验**（不可关闭）+ 私钥离线备份 + 清单走 HTTPS + 只信配置里的端点 +
  校验失败静默丢弃。这套与浏览器更新同级；**不用自动更新的客户端反而会长期停留在有漏洞的版本**。
- 让步：提供"关闭自动更新（仅提示）"开关，给保守用户。

**反驳 3：把"设为默认邮件程序"做得这么曲折（还要跳设置页），用户体验很差。**
- 证据：这是 Windows 的设计（微软废除了程序化设置默认）。
- 缓解：首次运行向导里给出**一步到位**的按钮 + 截图说明；之后只在"不是默认"时提示一次；
  同时 `.eml` 双击与 `mailto:` 在**我们是默认**时工作正常（这是能被感知的核心体验）。

**反驳 4：微软商店路线很麻烦（要离线 WebView2、自更新、签名），值得吗？**
- 权衡：商店带来的**发现量**与"SmartScreen 免费放行"对小产品价值大。
- 让步：M3 再做，且只做"EXE 产品"登记（不碰 MSIX）；先靠 winget + 官网。

**反驳 5：便携版不能自更新，用户会长期跑旧版本。**
- 缓解：便携版启动时检查更新并**显式提示**"有新版本，请下载 zip 替换"；设置页显示"便携模式：手动更新"。
- 让步：如果实测便携版用户比例高，M3 做"便携版自解压更新"（下载 zip → 自替换 → 重启），
  但那要处理"程序正在运行不能覆盖自己"，成本不小，先不做。

**反驳 6：ICP 备案与国内 CDN 是运营负担，为什么写进设计？**
- 因为**更新**这一条链路在大陆的可用性会直接影响"能不能持续交付"。
- 设计上不强依赖：默认 GitHub Releases + 可配置镜像；真要上国内 CDN 时，备案是**人**的事（写进 §7 表格）。

## 14. 未决问题（需你拍板）

| # | 问题 | 影响 | 建议 |
|---|---|---|---|
| **P3** | 代码签名方案 | 安装体验与预算 | **Azure Artifact Signing（若资格符合，≈$10/月）**，否则 **OV（¥1000-2200/年 + 硬件令牌）**；**不买 EV** |
| **P15** | 发布渠道优先级 | 触达量 | v1：官网 zip + GitHub Releases + winget；M3：微软商店 |
| **P16** | 更新清单托管地 | 大陆可用性 | 默认 GitHub Releases（免备案）；预留镜像设置项 |
| **P17** | 便携版是否进 v1 | 装机受限场景 | 进（成本 S：`--no-bundle` + 打包脚本 + 便携模式数据目录） |
| **P18** | 是否发 MSI | 企业部署 | M1 不发（避免交叉更新循环）；M3 与 perMachine 一起做 |
| **P19** | 官网/域名 | 商店与 Google 审核都需要（OAuth 验证要隐私政策页与主页） | **尽早准备**，与 P2（OAuth 注册）绑定 |

## 15. 验收清单

- [ ] 干净 Win10/Win11 上：双击安装无 UAC、装完自动启动、开始菜单与桌面快捷方式带 AUMID。
- [ ] 无 WebView2 的机器：联网时自动装引导器成功；离线包能装成功。
- [ ] 安装后 `.eml` 与 `mailto:` 能选到我们，设为默认后行为正确；"默认应用"页能看到我们。
- [ ] 更新全链路（含篡改包被拒、安装失败回滚）通过；版本号三处一致。
- [ ] winget 清单校验通过并能静默安装。
- [ ] 卸载（勾选/不勾选数据）后注册表与文件无残留，`user.db` 在未勾选时保留。
- [ ] 通知带按钮且能路由到具体邮件；提权运行时给出明确提示。
- [ ] 便携版数据落在 `data/`，并明确提示手动更新。
- [ ] `check.ps1` 五项新增检查通过。

> **章节名对照（模板 13 节）**：数据结构→§3 配置与数据结构。
> 内容齐全，只是节名与模板不完全同名（见 design/README.md 的说明）。
