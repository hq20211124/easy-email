<#
.SYNOPSIS
    这台机器有没有加密邮件客户端写缓存的那块盘？（部署前置条件的取证脚本）

.DESCRIPTION
    `design/A3-local-store-protection.md` 选项 2 把本地存储的安全边界放在**卷**上，而不是放在
    应用自己的数据库加密上——这是所有头部桌面邮件客户端的共同做法。既然是前置条件，就必须有人
    能证明它成立，所以这个脚本把证明打印成一页可以存档的文本。

    应用内也有一份启动自检，但它**无法**在普通权限下回答这个问题：实测（Windows 10.0.26300）
    `manage-bde -status` 与下面的 CIM 类在非管理员进程里都返回 Access Denied。因此自检的诚实结论
    是「无法判定（需要管理员权限）」，而留档必须以**这次提权运行**的输出为准。

    只读。它只做一件事：查询 `Win32_EncryptableVolume` 的状态并打印。它不会开启加密、不会暂停保护、
    不会备份恢复密钥、不会改动任何 protector——这些是 IT 的动作。一个脚本同时具备「检查」和「修改」
    两种能力，正是审查时会追问的地方。

    退出码可以直接用于部署脚本：
      0 = 系统盘已加密且保护已开启（前置条件成立）
      1 = 不成立（未加密、保护已关闭、或仍在转换中）
      2 = 无法判定（没有管理员权限，或查询本身失败）

.EXAMPLE
    powershell -NoProfile -ExecutionPolicy Bypass -File tools\disk-protection-check.ps1
    （在部署机上以管理员身份运行，把输出连同日期、机器名一起归档）
#>

$ErrorActionPreference = 'Stop'

function Get-PrincipalNote {
    $identity = [Security.Principal.WindowsIdentity]::GetCurrent()
    $principal = New-Object Security.Principal.WindowsPrincipal($identity)
    $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
}

# 与 `src-tauri/src/diskprotect.rs` 里 `state_of` 同一张表：数字来自微软文档，
# 不是 `manage-bde` 的本地化文案——中文系统上按文案解析一定会错。
$ConversionNames = @{
    0 = '完全解密'
    1 = '完全加密'
    2 = '加密待处理'
    3 = '加密进行中'
    4 = '解密进行中'
}
$ProtectionNames = @{
    0 = '保护已关闭'
    1 = '保护已开启'
    2 = '保护状态未知'
}

function Get-VolumeState($conversion, $protection) {
    # 返回名称加一个布尔量，而不是让调用方去比较字符串：判定写在两处就会漂移。
    if ($conversion -eq 1 -and $protection -eq 1) {
        return @{ Name = '已加密且保护已开启'; Protected = $true }
    }
    if ($conversion -eq 1 -and $protection -eq 0) {
        return @{ Name = '已加密但保护已关闭'; Protected = $false }
    }
    if ($conversion -eq 0) { return @{ Name = '未加密'; Protected = $false } }
    if ($conversion -in 2, 3, 4) { return @{ Name = '加密/解密进行中'; Protected = $false } }
    # 包括 conversion=1 而 protection=2（未知）：「查不到」和「确认没开」是两条不同的结论。
    return @{ Name = '无法判定'; Protected = $false }
}

Write-Output ('机器：' + [Environment]::MachineName + '　时间：' + (Get-Date -Format 'yyyy-MM-dd HH:mm:ss'))

if (-not (Get-PrincipalNote)) {
    Write-Output '结论：无法判定——本脚本没有以管理员身份运行。'
    Write-Output '        应用内的启动自检遇到的是同一道权限墙，所以这条只能由提权运行来补。'
    exit 2
}

$namespace = 'root\CIMV2\Security\MicrosoftVolumeEncryption'
try {
    $volumes = @(Get-CimInstance -Namespace $namespace -ClassName Win32_EncryptableVolume)
} catch {
    Write-Output ('结论：无法判定——查询失败：' + $_.Exception.Message)
    Write-Output '        这台机器可能没有 BitLocker（例如 Windows 家庭版），此时前置条件需要用'
    Write-Output '        等保认可的其它全盘加密方案来满足，并把替代方案写进部署记录。'
    exit 2
}

if ($volumes.Count -eq 0) {
    Write-Output '结论：无法判定——查询没有返回任何卷。'
    exit 2
}

# VolumeType：0 = 固定系统卷，1 = 固定数据卷，2 = 可移除，3 = 未知。
# 判定只看固定卷：U 盘没加密不构成「这台机器不合规」，但数据盘没加密构成。
$typeNames = @{ 0 = '系统卷'; 1 = '数据卷'; 2 = '可移除卷'; 3 = '未知类型' }
$verdict = $true
$fixedCount = 0
foreach ($v in $volumes) {
    $state = Get-VolumeState $v.ConversionStatus $v.ProtectionStatus
    $type = $typeNames[[int]$v.VolumeType]
    if ($type -eq $null) { $type = '类型 ' + $v.VolumeType }
    Write-Output ('  {0}（{1}）：{2}　[ConversionStatus={3} ProtectionStatus={4}]' -f `
        $v.DriveLetter, $type, $state.Name, $v.ConversionStatus, $v.ProtectionStatus)
    if ($v.VolumeType -in 0, 1) {
        $fixedCount += 1
        if (-not $state.Protected) { $verdict = $false }
    }
}

if ($fixedCount -eq 0) {
    Write-Output '结论：无法判定——查询里没有任何固定卷。'
    exit 2
}
if ($verdict) {
    Write-Output '结论：前置条件成立——所有固定卷都已加密且保护已开启。'
    Write-Output '        请把本页输出（含机器名与时间）归档为送审材料。'
    exit 0
}
Write-Output '结论：前置条件不成立——至少一块固定卷不是「已加密且保护已开启」。'
Write-Output '        在这种机器上，本地缓存文件对任何拿到机器的人都是明文的；邮件客户端的'
Write-Output '        「正文不落盘」模式只是减轻暴露面，不能替代磁盘加密。'
exit 1
