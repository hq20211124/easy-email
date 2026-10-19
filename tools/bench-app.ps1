<#
    App-level benchmarks for Easy Email (design/15 §4): P1 cold start, P2 folder switch, P3
    scrolling, P5 resident memory, P6 idle CPU.

    These need the **running desktop shell**, which is why they are not in `tools\bench.ps1`: the
    storage benchmark measures the database, and reporting its numbers as "the app is fast" is the
    mistake this split exists to prevent. They also could not be measured in the environment where
    the rest of the report was produced — a GUI process started there exited immediately with no
    window and no log line — so **the numbers in reports/performance.md for P1/P2/P3/P5/P6 were
    missing, not estimated.**

    P3 (round 74) is no longer a placeholder: it drives a scroll sweep inside the running window via
    `tools\bench-scroll.mjs`, which judges both of P3's budgets (60 fps, ≤ 40 rows changed per frame)
    and exits non-zero when they are missed. P1/P2 (round 98) got the same treatment from
    `tools\bench-first-paint.mjs`. §4 names `t_first_paint` and `t_folder_switch` as the instrumentation
    for those two and neither existed until now — the stamp is written in `renderMessages` (src/app.js),
    where rows actually enter the DOM, which is why the number is the moment rather than the moment the
    tool happened to look.

    **The run has happened (2026-10-19)**: release build, the user's real profile, five passes of this
    script plus a five-launch P1 repeat. The readings, the two-leg P1 decomposition and the three
    budgets that came back red are in `reports/performance.md` §9; the raw per-pass records are in
    `target\bench-app-history.jsonl`. Two things this header has to say about them, because they are the
    two ways this harness could still mislead: a **single pass is not a number** (P1 read 1 858 ms
    median over seven launches and 10 038 ms once, on the same build and the same profile), and P5/P6
    are sampled on an **18-message profile**, not the 100k shape the budget names.

    Usage:
        powershell -ExecutionPolicy Bypass -File tools\bench-app.ps1
        powershell -ExecutionPolicy Bypass -File tools\bench-app.ps1 -Seconds 300
        powershell -ExecutionPolicy Bypass -File tools\bench-app.ps1 -Exe target\release\easy-email.exe

    What it does:
      P1  starts the exe, and a CDP watcher launched *with* it reports the time from process start to
          the first `[data-uid]` row on screen (`bench-first-paint.mjs`). This script also polls for
          the debugging port; that number is recorded as `P1_cold_start_to_renderer_s` and is **not**
          P1 — it is when the renderer became reachable, which is always earlier than a row.
      P2  the same watcher clicks two sidebar folders and times each press to the new folder's first
          row, and says whether the pane still carried the 「（正在收取…）」 mark at that moment — the
          direct evidence that the first paint came from the cache rather than from the network.
      P5  samples the whole process tree — the Rust host plus the WebView2 processes it owns — at the
          points design/15 §7 lists, and reports the two halves separately. The first run of this script
          sampled only the host and read 45 MB; the renderer was not in the number, and P5's budget is
          about what the machine pays, not what one process of many reports.
      P6  samples CPU over a quiet minute after startup has settled, summed over the same tree. The
          percentage is against **one core** (CPU-seconds / 60), which is the stricter reading of the
          0.5% budget; `P6_idle_cpu_cores` is the same number in whole cores.

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
    [string]$Exe = 'target\debug\easy-email.exe',
    # The first-paint sampler clicks folders, which means real refreshes on a real profile. Skip it
    # when only memory and CPU are being sampled.
    [switch]$SkipSwitch
)

$ErrorActionPreference = 'Continue'
Push-Location (Join-Path $PSScriptRoot '..')

function Get-ProcessTreeIds([int]$RootId) {
    # The app is not one process: the Rust host plus the WebView2 child/grandchild processes that
    # actually render the page. Sampling only the host reported 45 MB for P5 while the renderer's
    # hundred-odd sat outside the number — a pass measured on the wrong process.
    $byParent = @{}
    Get-CimInstance Win32_Process -ErrorAction SilentlyContinue | ForEach-Object {
        if ($null -eq $_.ParentProcessId) { return }
        # [long] on both sides: Win32_Process gives UInt32 pids, and Int32.Equals(UInt32) is false, so a
        # mixed-key hashtable would quietly find no children.
        $parent = [long]$_.ParentProcessId
        if (-not $byParent.ContainsKey($parent)) { $byParent[$parent] = @() }
        $byParent[$parent] += [long]$_.ProcessId
    }
    $queue = New-Object System.Collections.Queue
    $queue.Enqueue([long]$RootId)
    $ids = @()
    while ($queue.Count -gt 0) {
        $id = [long]$queue.Dequeue()
        # ContainsKey, not a lookup: indexing a missing key yields @($null), which walks one level past
        # the tree into `Get-Process -Id $null`.
        if ($id -and -not ($ids -contains $id)) {
            $ids += $id
            if ($byParent.ContainsKey($id)) {
                foreach ($child in $byParent[$id]) { $queue.Enqueue([long]$child) }
            }
        }
    }
    return $ids
}

function Sample-ProcessTree([int]$RootId) {
    $procs = @(Get-ProcessTreeIds $RootId |
        ForEach-Object { Get-Process -Id $_ -ErrorAction SilentlyContinue } |
        Where-Object { $_ -and -not $_.HasExited })
    # Working set is what the user's machine actually pays; private bytes is what the process would
    # keep if everything else were paged out. Handles and threads are the leak indicators.
    $root = @($procs | Where-Object { $_.Id -eq $RootId })
    $others = @($procs | Where-Object { $_.Id -ne $RootId })
    $sum = { param($set, $prop) [math]::Round((@($set | ForEach-Object { $_.$prop }) | Measure-Object -Sum).Sum / 1MB, 1) }
    [PSCustomObject]@{
        Processes      = $procs.Count
        HostWS         = if ($root) { & $sum $root 'WorkingSet64' } else { 0 }
        HostPrivateMB  = if ($root) { & $sum $root 'PrivateMemorySize64' } else { 0 }
        WebViewWS      = if ($others) { & $sum $others 'WorkingSet64' } else { 0 }
        WebViewPrivate = if ($others) { & $sum $others 'PrivateMemorySize64' } else { 0 }
        TotalWS        = if ($procs) { & $sum $procs 'WorkingSet64' } else { 0 }
        TotalPrivate   = if ($procs) { & $sum $procs 'PrivateMemorySize64' } else { 0 }
        Handles        = (@($procs | ForEach-Object { $_.HandleCount }) | Measure-Object -Sum).Sum
        Threads        = (@($procs | ForEach-Object { $_.Threads.Count }) | Measure-Object -Sum).Sum
        CpuSeconds     = [math]::Round((@($procs | ForEach-Object { $_.TotalProcessorTime.TotalSeconds }) | Measure-Object -Sum).Sum, 2)
    }
}

$results = [ordered]@{}

function Start-PaintSampler([string[]]$Arguments, [string]$OutPath, [string]$ErrPath) {
    if (-not (Get-Command node -ErrorAction SilentlyContinue)) { return $null }
    Remove-Item $OutPath, $ErrPath -ErrorAction SilentlyContinue
    return Start-Process -FilePath 'node' -ArgumentList $Arguments -PassThru -NoNewWindow `
        -RedirectStandardOutput $OutPath -RedirectStandardError $ErrPath
}

function Get-PaintVerdict($Sampler, [string]$OutPath, [string]$ErrPath, [int]$TimeoutSeconds) {
    if (-not $Sampler) { return $null }
    if (-not $Sampler.WaitForExit($TimeoutSeconds * 1000)) {
        Write-Host "  采样器 $TimeoutSeconds 秒没结束 —— 多半在等一行都出不来的列表。" -ForegroundColor Red
        try { $Sampler.Kill() } catch {}
    }
    $line = $null
    if (Test-Path $OutPath) {
        $line = @((Get-Content $OutPath -Encoding UTF8) | Where-Object { $_ -match '^P1P2 ' }) |
            Select-Object -First 1
    }
    if (-not $line) {
        Write-Host "  采样器没有产出可解析的结果。stderr：" -ForegroundColor Red
        if (Test-Path $ErrPath) {
            Get-Content $ErrPath -Encoding UTF8 | ForEach-Object { Write-Host "  $_" }
        }
        return $null
    }
    # The verdict is the sampler's own JSON (`p1Pass`/`p2Pass`), not its exit code: the `Process` object
    # `Start-Process` hands back cannot reliably read `ExitCode` afterwards, and an empty column where a
    # pass/fail belongs is worse than no column.
    $verdict = ($line -replace '^P1P2 ', '') | ConvertFrom-Json
    return $verdict
}

# ---------------------------------------------------------------- P1: cold start
Write-Host "P1 冷启动：启动 $Exe 并等待渲染进程应答…" -ForegroundColor Cyan
$started = Get-Date
$startedEpoch = [DateTimeOffset]::new($started.ToUniversalTime()).ToUnixTimeMilliseconds()
$process = Start-Process -FilePath $Exe -PassThru

# The real P1 (`design/15` §4: 冷启动到**列表可见**) is measured inside the page, so the watcher has to
# start with the process rather than after this script finishes polling the port — and it converts the
# page clock to epoch ms against `$startedEpoch`.
#
# The two passes are deliberately separate. P2 clicks folders, which means repaints and real IMAP
# fetches; running it while P5/P6/P3 are being sampled would put that work inside their numbers, so the
# switch probe goes last and only the cold-start read runs concurrently now.
$paintOut = 'target\bench-first-paint-p1.out.txt'
$paintErr = 'target\bench-first-paint-p1.err.txt'
$paint2Out = 'target\bench-first-paint-p2.out.txt'
$paint2Err = 'target\bench-first-paint-p2.err.txt'
$painter = $null
if (Get-Command node -ErrorAction SilentlyContinue) {
    $painter = Start-PaintSampler @('tools\bench-first-paint.mjs', '--port', "$Port", '--since', "$startedEpoch", '--switch', '0') `
        $paintOut $paintErr
} else {
    Write-Host "  node 不在 PATH 上：P1 的『第一行可见』和 P2 都测不到（端口应答那条数字不是 P1）。" -ForegroundColor Yellow
}

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
Write-Host ("  渲染进程应答：{0:N2} s —— 这不是 P1，只是端口活着的时刻" -f $ready)

# ---------------------------------------------------------------- P1: the page's own number
Write-Host "`nP1 首屏：页面内采样（design/15 §4：≤ 1.5 s 到第一行 [data-uid]）…" -ForegroundColor Cyan
$fp1 = Get-PaintVerdict $painter $paintOut $paintErr 120
if ($fp1) {
    $p1 = $fp1.p1
    $firstRowMs = if ($null -ne $p1.msSinceProcessStart) { $p1.msSinceProcessStart } else { $p1.msSincePageStart }
    $results['P1_first_visible_row_ms'] = $firstRowMs
    $results['P1_measured_from'] = $p1.measuredFrom
    # `page-stamp` = the app said when it painted. `dom-poll` = this watcher only *sighted* the row, so
    # the number carries the CDP connect latency and is a ceiling, not a measurement.
    $results['P1_measured_by'] = $p1.source
    $results['P1_is_upper_bound'] = [bool]$p1.upperBound
    $results['P1_stamp_vs_dom_ms'] = $p1.stampVsDomMs
    $results['P1_rows'] = $p1.rows
    $results['P1_cache_leg_at_first_row'] = [bool]$p1.refreshingMark
    if ($p1.upperBound -and $p1.rows -gt 0) {
        Write-Host "  这个二进制里没有 __easyEmailPaint 戳记（构建早于 src/app.js 的改动）：下面是上界。" -ForegroundColor Yellow
    }
    if ($p1.rows -lt 1 -and $null -eq $firstRowMs) {
        $results['P1_what_was_on_screen'] = $p1.pane
        Write-Host ("  P1：列表里一行都没有 —— 屏幕上写的是「{0}」。这不是通过，是没测到。" -f `
            $p1.pane) -ForegroundColor Yellow
    } elseif ($fp1.p1Pass) {
        Write-Host ("  P1 第一行可见：{0} ms（{1} / {2}，≤ 1500 ms 达标{3}）" -f `
            $firstRowMs, $p1.measuredFrom, $p1.source, $(if ($p1.upperBound) { '；这是上界' } else { '' })) -ForegroundColor Green
    } else {
        Write-Host ("  P1 第一行可见：{0} ms（{1} / {2}）— 超出 1500 ms 预算。" -f `
            $firstRowMs, $p1.measuredFrom, $p1.source) -ForegroundColor Red
    }
}

# ---------------------------------------------------------------- P5: memory samples
Write-Host "`nP5 常驻内存采样（design/15 §7 的采样点，含 WebView2 子进程）…" -ForegroundColor Cyan
$now = $null
Start-Sleep -Seconds 10
$now = Sample-ProcessTree $process.Id
Write-Host ("  t+  10s  合计 WS {0,7} MB（壳 {1,6} + WebView {2,6}）   私有 {3,7} MB   进程 {4,2}   句柄 {5,5}   线程 {6,3}" -f `
    $now.TotalWS, $now.HostWS, $now.WebViewWS, $now.TotalPrivate, $now.Processes, $now.Handles, $now.Threads)

for ($i = 0; $i -lt $Seconds; $i += 10) {
    Start-Sleep -Seconds 10
    $now = Sample-ProcessTree $process.Id
    Write-Host ("  t+{0,4}s  合计 WS {1,7} MB（壳 {2,6} + WebView {3,6}）   私有 {4,7} MB   进程 {5,2}   句柄 {6,5}   线程 {7,3}" -f `
        ($i + 20), $now.TotalWS, $now.HostWS, $now.WebViewWS, $now.TotalPrivate, $now.Processes, $now.Handles, $now.Threads)
}

$results['P5_working_set_mb'] = $now.TotalWS
$results['P5_host_working_set_mb'] = $now.HostWS
$results['P5_webview_working_set_mb'] = $now.WebViewWS
$results['P5_private_mb'] = $now.TotalPrivate
$results['P5_processes'] = $now.Processes
$results['P5_handles'] = $now.Handles
$results['P5_threads'] = $now.Threads

# ---------------------------------------------------------------- P6: idle CPU
$cpuBefore = (Sample-ProcessTree $process.Id).CpuSeconds
Start-Sleep -Seconds 60
$cpuAfter = (Sample-ProcessTree $process.Id).CpuSeconds
$idleCpu = $cpuAfter - $cpuBefore
$idlePercent = [math]::Round($idleCpu / 60.0 * 100.0, 2)
$results['P6_idle_cpu_percent'] = $idlePercent
$results['P6_idle_cpu_cores'] = [math]::Round($idleCpu / 60.0, 3)
Write-Host ("`nP6 空闲 CPU（60 秒，整个进程树）：{0:N2} %   （预算 ≤ 0.5%）" -f $idlePercent) -ForegroundColor Cyan

# ---------------------------------------------------------------- P3: scrolling
# The placeholder here used to say "this script will not pretend to measure it". It can now:
# `tools\bench-scroll.mjs` drives the scroll inside the running window and judges both budgets
# itself, so this step inherits its verdict instead of re-implementing the comparison here.
Write-Host "`nP3 列表滚动：CDP 采样（design/15 §4：60 fps、每帧 DOM 变更 ≤ 40 行）…" -ForegroundColor Cyan
if (-not (Get-Command node -ErrorAction SilentlyContinue)) {
    Write-Host "  node not found on PATH；跳过（这是采样器需要的运行时）。" -ForegroundColor Yellow
    $results['P3_scroll_fps'] = $null
} else {
    $scroll = & node tools\bench-scroll.mjs --port $Port --json 2>&1
    $scrollCode = $LASTEXITCODE
    $jsonLine = @($scroll | Where-Object { $_ -match '^P3 \{' }) | Select-Object -First 1
    if ($jsonLine) {
        $p3 = ($jsonLine -replace '^P3 ', '') | ConvertFrom-Json
        $results['P3_scroll_fps'] = $p3.fps
        $results['P3_frame_ms_p95'] = $p3.frameMsP95
        $results['P3_rows_changed_per_frame_max'] = $p3.rowsChangedPerFrameMax
        # Kept separate from the judged number on purpose: the sampler itself teleports the list to the
        # top when it hits the bottom, and the frame after that replaces the whole window. Reporting only
        # the scroll-frame max would hide how much of the churn that bounce accounts for.
        $results['P3_rows_changed_after_jump_max'] = $p3.rowsChangedAfterJumpMax
        $results['P3_jump_frames'] = $p3.jumpFrames
        $results['P3_rows_in_dom'] = $p3.rows
        Write-Host ("  fps {0}   帧时 P95 {1} ms   单帧最多变更 {2} 行（滚动帧）/ {3} 行（回顶帧，采样器造成的，不计）   DOM 里 {4} 行" -f `
            $p3.fps, $p3.frameMsP95, $p3.rowsChangedPerFrameMax, $p3.rowsChangedAfterJumpMax, $p3.rows)
        # A sample of nothing is not a pass. An empty profile has no rows to scroll, and reporting
        # its 60 fps as P3 would be the exact "checks that cannot fail" mistake this repo keeps
        # writing down.
        if ($p3.rows -lt 1) {
            Write-Host "  列表里一行都没有 —— 应用可能还没进收件箱；这一项不算测到。" -ForegroundColor Yellow
            $results['P3_rows_changed_per_frame_max'] = $null
            $results['P3_scroll_fps'] = $null
        } elseif ($scrollCode -ne 0) {
            Write-Host "  P3 未达标（采样器已判失败，详见 tools\bench-scroll.mjs 的输出）。" -ForegroundColor Red
        } else {
            Write-Host "  P3 在这个形状下达标（注意：这不是 10 万封的形状）。" -ForegroundColor Green
        }
    } else {
        Write-Host "  采样器没有产出可解析的结果 —— 它多半连不上调试端口。" -ForegroundColor Red
        $scroll | ForEach-Object { Write-Host "  $_" }
    }
}

# ---------------------------------------------------------------- P2: folder switch
# Last, on purpose: clicking folders repaints the list and starts real IMAP fetches, which is the one
# thing that must not overlap the memory, idle-CPU or scroll samples.
Write-Host "`nP2 切文件夹：页面内采样（design/15 §4：≤ 200 ms 到新文件夹第一行）…" -ForegroundColor Cyan
if ($SkipSwitch) {
    Write-Host "  -SkipSwitch：跳过（这项会点击侧栏文件夹并触发真实收取）。" -ForegroundColor Yellow
} elseif (-not (Get-Command node -ErrorAction SilentlyContinue)) {
    Write-Host "  node 不在 PATH 上：P2 测不到。" -ForegroundColor Yellow
} else {
    $painter2 = Start-PaintSampler @('tools\bench-first-paint.mjs', '--port', "$Port", '--paint', '0') `
        $paint2Out $paint2Err
    $fp2 = Get-PaintVerdict $painter2 $paint2Out $paint2Err 300
    $p2 = if ($fp2) { $fp2.p2 } else { $null }
    if ($null -eq $p2) {
        $results['P2_folder_switch_max_ms'] = $null
    } elseif ($null -ne $p2.skipped) {
        Write-Host ("  P2 未测：{0}" -f $p2.skipped) -ForegroundColor Yellow
        $results['P2_folder_switch_max_ms'] = $null
        $results['P2_skipped'] = $p2.skipped
    } else {
        $times = @($p2.rounds | ForEach-Object { $_.firstPaintMs } | Where-Object { $null -ne $_ })
        $cacheLegs = @($p2.rounds | Where-Object { $_.cacheLegPainted })
        $sorted = @($times | Sort-Object)
        # Median by hand: `Measure-Object -Median` is PowerShell 7, and this script is documented
        # for `powershell -File`, which on Windows is 5.1.
        $median = if ($sorted.Count -eq 0) { $null } `
            elseif ($sorted.Count % 2 -eq 1) { $sorted[[int](($sorted.Count - 1) / 2)] } `
            else { ($sorted[$sorted.Count / 2 - 1] + $sorted[$sorted.Count / 2]) / 2 }
        $results['P2_folder_switch_max_ms'] = if ($sorted.Count -gt 0) { $sorted[-1] } else { $null }
        $results['P2_folder_switch_median_ms'] = $median
        $results['P2_measured_by'] = if ($p2.rounds.Count -gt 0) { $p2.rounds[0].measuredBy } else { $null }
        $results['P2_rounds'] = $p2.rounds.Count
        $results['P2_rounds_from_cache'] = $cacheLegs.Count
        $results['P2_timed_out_rounds'] = @($p2.rounds | Where-Object { $_.timedOut }).Count
        foreach ($r in $p2.rounds) {
            Write-Host ("    {0,-22} 第一行 {1,6} ms（{2}）  缓存先画 {3}   网络完成 {4}" -f `
                $r.folder, $r.firstPaintMs, $r.measuredBy,
                $(if ($r.cacheLegPainted) { '是' } elseif ($null -eq $r.cacheLegPainted) { '—' } else { '否' }), `
                $(if ($r.timedOut) { '60 s 内没等到' } else { "$($r.settledMs) ms" }))
        }
        if (-not $fp2.p2Pass) {
            Write-Host "  P2 有回合超出 200 ms 或一行都没出来（采样器已判失败）。" -ForegroundColor Red
        } else {
            Write-Host ("  P2 达标：{0}/{1} 回合的第一行来自本地缓存。" -f `
                $cacheLegs.Count, $p2.rounds.Count) -ForegroundColor Green
        }
    }
}

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
