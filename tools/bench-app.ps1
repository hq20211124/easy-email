<#
    App-level benchmarks for Easy Email (design/15 §4): P1 cold start, P3 scrolling, P5 resident
    memory, P6 idle CPU.

    These four need the **running desktop shell**, which is why they are not in `tools/bench.ps1`:
    the storage benchmark measures the database, and reporting its numbers as "the app is fast" is
    the mistake this split exists to prevent. They also could not be measured in the environment
    where the rest of the report was produced — a GUI process started there exited immediately with
    no window and no log line — so **the numbers in reports/performance.md for P1/P3/P5/P6 are
    missing, not estimated.**

    Usage:
        powershell -ExecutionPolicy Bypass -File tools\bench-app.ps1
        powershell -ExecutionPolicy Bypass -File tools\bench-app.ps1 -Seconds 300

    What it does:
      P1  starts the exe, polls for the WebView2 debugging port, and reports the time from process
          start to the moment the renderer answers — an upper bound on "cold start to a usable list",
          and honest about being one.
      P5  samples working set, private bytes, handles and threads at the points design/15 §7 lists.
      P6  samples CPU over a quiet minute after startup has settled.

    Requirements: the app built with `--remote-debugging-port=9222` in `additionalBrowserArgs`
    (see tools/inspect-dom.mjs for the recipe — and put the flag back afterwards; tools/check.ps1
    fails the build if it is left in). A real profile makes P1 and P5 meaningful; an empty one is
    still worth running for P1.

    The numbers are appended to target\bench-app-history.jsonl so a regression shows up as a trend
    rather than as a memory of what last month's number was.
#>
[CmdletBinding()]
param(
    [int]$Seconds = 120,
    [int]$Port = 9222,
    [string]$Exe = 'target\debug\easy-email.exe'
)

$ErrorActionPreference = 'Continue'
Push-Location (Join-Path $PSScriptRoot '..')

function Sample-Process([System.Diagnostics.Process]$Process) {
    # Working set is what the user's machine actually pays; private bytes is what the process would
    # keep if everything else were paged out. Handles and threads are the leak indicators.
    $Process.Refresh()
    [PSCustomObject]@{
        WorkingSetMB = [math]::Round($Process.WorkingSet64 / 1MB, 1)
        PrivateMB    = [math]::Round($Process.PrivateMemorySize64 / 1MB, 1)
        Handles      = $Process.HandleCount
        Threads      = $Process.Threads.Count
        CpuSeconds   = [math]::Round($Process.TotalProcessorTime.TotalSeconds, 2)
    }
}

$results = [ordered]@{}

# ---------------------------------------------------------------- P1: cold start
Write-Host "P1 冷启动：启动 $Exe 并等待渲染进程应答…" -ForegroundColor Cyan
$started = Get-Date
$process = Start-Process -FilePath $Exe -PassThru
$ready = $null
while (((Get-Date) - $started).TotalSeconds -lt 60) {
    try {
        $null = Invoke-RestMethod "http://127.0.0.1:$Port/json/version" -TimeoutSec 2
        $ready = ((Get-Date) - $started).TotalSeconds
        break
    } catch {
        Start-Sleep -Milliseconds 250
    }
}

if (-not $ready) {
    Write-Host "  渲染进程 60 秒内没有应答 —— 是不是没带 --remote-debugging-port=9222 构建？" -ForegroundColor Red
    if ($process -and -not $process.HasExited) { $process.Kill() }
    Pop-Location
    exit 1
}
$results['P1_cold_start_to_renderer_s'] = [math]::Round($ready, 2)
Write-Host ("  P1 冷启动到渲染进程应答：{0:N2} s" -f $ready)

# ---------------------------------------------------------------- P5: memory samples
Write-Host "`nP5 常驻内存采样（design/15 §7 的采样点）…" -ForegroundColor Cyan
$samples = [ordered]@{}
Start-Sleep -Seconds 10
$samples['10s_after_start'] = Sample-Process $process

for ($i = 0; $i -lt $Seconds; $i += 10) {
    Start-Sleep -Seconds 10
    $now = Sample-Process $process
    Write-Host ("  t+{0,4}s  WS {1,7} MB   Private {2,7} MB   句柄 {3,5}   线程 {4,3}" -f `
        ($i + 20), $now.WorkingSetMB, $now.PrivateMB, $now.Handles, $now.Threads)
}
$samples['after_idle'] = Sample-Process $process

$results['P5_working_set_mb'] = $samples['after_idle'].WorkingSetMB
$results['P5_private_mb'] = $samples['after_idle'].PrivateMB
$results['P5_handles'] = $samples['after_idle'].Handles

# ---------------------------------------------------------------- P6: idle CPU
$idleStart = $process.TotalProcessorTime.TotalSeconds
Start-Sleep -Seconds 60
$process.Refresh()
$idleCpu = $process.TotalProcessorTime.TotalSeconds - $idleStart
$idlePercent = [math]::Round($idleCpu / 60.0 * 100.0, 2)
$results['P6_idle_cpu_percent'] = $idlePercent
Write-Host ("`nP6 空闲 CPU（60 秒）：{0:N2} %   （预算 ≤ 0.5%）" -f $idlePercent) -ForegroundColor Cyan

# ---------------------------------------------------------------- P3: scrolling
Write-Host "`nP3 列表滚动：需要 CDP 驱动滚动脚本，本脚本不假装能测。" -ForegroundColor Yellow
Write-Host "    用 node tools/inspect-dom.mjs 的通道手动跑（见 design/15 §6：60fps、每帧 DOM 变更 ≤ 40 行）。"
$results['P3_scroll_fps'] = $null

# ---------------------------------------------------------------- history
$historyPath = 'target\bench-app-history.jsonl'
$record = [PSCustomObject]@{
    at = (Get-Date).ToString('o')
    machine = "$env:COMPUTERNAME"
    seconds_sampled = $Seconds
    results = $results
}
($record | ConvertTo-Json -Compress) | Add-Content -Path $historyPath -Encoding utf8

Write-Host "`n结果（同时追加到 $historyPath）：" -ForegroundColor Green
$results.GetEnumerator() | ForEach-Object { Write-Host ("  {0,-34} {1}" -f $_.Key, $_.Value) }

if ($process -and -not $process.HasExited) {
    Write-Host "`n关闭应用（脚本自己启动的进程）…"
    $process.CloseMainWindow() | Out-Null
    Start-Sleep -Seconds 3
    if (-not $process.HasExited) { $process.Kill() }
}

Pop-Location
