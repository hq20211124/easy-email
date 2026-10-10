<#
    Storage-path benchmark for Easy Email (design/15 §3/§4).

    measures the numbers that live in the database layer: ingest throughput, list and search
    latency, and the size of the database and the FTS index. It drives the ignored tests in
    crates/mail-core/tests/bench_fixture.rs, which is where the fixture is generated.

    What this does NOT measure — cold start, resident memory, idle CPU, scroll smoothness. Those
    need the running desktop shell, and the coverage table printed at the end says so per item
    instead of leaving them out of the output entirely (design/15 §8 asks for all sixteen).

    Usage:
        powershell -ExecutionPolicy Bypass -File tools\bench.ps1
        powershell -ExecutionPolicy Bypass -File tools\bench.ps1 -Quick
        powershell -ExecutionPolicy Bypass -File tools\bench.ps1 -Messages 20000 -BodyBytes 4096

    The signature/execution-policy note is not incidental: the documented `.\tools\check.ps1`
    does not run on a default Windows PowerShell (RemoteSigned, unsigned script, no certificate),
    which is a papercut every contributor would hit on their first command.
#>
[CmdletBinding()]
param(
    [int]$Messages = 100000,
    [int]$BodyBytes = 8192,
    [switch]$Quick,
    [switch]$DebugProfile
)

$ErrorActionPreference = 'Continue'
Push-Location (Join-Path $PSScriptRoot '..')

if ($Quick) {
    $Messages = 5000
    $BodyBytes = 2048
}

$fixture = Join-Path (Get-Location) 'target\bench'
$env:EE_BENCH_MESSAGES = "$Messages"
$env:EE_BENCH_BODY_BYTES = "$BodyBytes"
$env:EE_BENCH_DIR = $fixture

$profile = if ($DebugProfile) { '' } else { '--release' }
Write-Host "fixture : messages=$Messages body_bytes=$BodyBytes dir=$fixture" -ForegroundColor Cyan
if ($DebugProfile) {
    Write-Host "profile : debug (numbers are 10-30x slower than release; only compare like with like)" -ForegroundColor Yellow
}

# ─────────────────────────────────────────────────────────────────────── the sixteen budgets
#
# `design/15` §4 is the single authoritative table and §8 asks the script to print all sixteen
# items. The review behind `design/19` §4 named the failure mode precisely: the script claimed a
# count the table did not have, and the difference was silently untested budgets. So the table below
# is the script's own, and `Assert-BudgetCoverage` fails the run if it stops being sixteen.
#
# Measured items are filled in from the numbers this run actually printed — parsed out of the test
# output, not remembered — so a measurement that stops happening shows up as "未测" instead of as a
# stale number from a previous run.
#
# Two rows need a comment rather than a number:
#   * P15's budget text says "v2→v3", which is from when this table was written. The fixture measures
#     the migration the *current* version actually performs (v6→v7, the FTS rebuild) on a real
#     populated file; the row is honest about which step it is rather than pretending the old label.
#   * P14 **is over its budget and the reason is structural, not a regression**: the FTS row stores
#     the *tokenised* text, and the CJK tokenisation this project is pinned to (`cache.rs::to_tokens`,
#     one token per character so that 2-character words are searchable) roughly doubles CJK text.
#     Measured 170% of indexed text at the -Quick shape on both platforms. `design/09` §8's 65.1% came
#     from a bigram shape we do not use. Either the budget row moves or the index shape does; until
#     someone decides, the honest thing is to print the number and say this.
#   * P1/P5/P6 have a harness (`tools\bench-app.ps1`) that needs the running desktop shell built with
#     a CDP port; it was not run, so they stay "未测". P3 has no harness at all — the script's header
#     mentions scrolling, its body does not do it.
$Budgets = @(
    @{ Id = 'P1';  Name = '冷启动到列表可见';        Budget = '1.5 s';        Where = '需要跑起来的桌面壳（tools\bench-app.ps1 是那台测量仪；本轮未跑）' }
    @{ Id = 'P2';  Name = '切文件夹';                Budget = '200 ms';       Where = '' }
    @{ Id = 'P3';  Name = '列表滚动 60 fps';         Budget = '60 fps';       Where = '没有测量手段：bench-app.ps1 头部提到它，脚本里没有实现' }
    @{ Id = 'P4';  Name = '本地搜索 P95';            Budget = '100 ms';       Where = '' }
    @{ Id = 'P5';  Name = '常驻内存';                Budget = '300 MB';       Where = '需要跑起来的桌面壳（tools\bench-app.ps1；本轮未跑）' }
    @{ Id = 'P6';  Name = '空闲 CPU';                Budget = '0.5%';         Where = '需要跑起来的桌面壳（tools\bench-app.ps1；本轮未跑）' }
    @{ Id = 'P7';  Name = '首同步吞吐（头部）';        Budget = '300 封/秒';     Where = '' }
    @{ Id = 'P8';  Name = '正文预取吞吐';            Budget = '20 封/秒';     Where = '需要真实 IMAP 取件（P9 只测入库，不测取件）' }
    @{ Id = 'P9';  Name = '索引吞吐';                Budget = '20 封/秒';     Where = '' }
    @{ Id = 'P10'; Name = '单封 MIME 生成（24 MB）';  Budget = '3 s';         Where = '' }
    @{ Id = 'P11'; Name = '附件另存 100 MB 峰值';     Budget = '8 MB 增量';    Where = '需要真实 IMAP 取件 + 采样峰值内存' }
    @{ Id = 'P12'; Name = '单次 IPC 载荷';           Budget = '200 KB';      Where = '' }
    @{ Id = 'P13'; Name = '数据库体积';              Budget = '1.5 GB';      Where = '' }
    @{ Id = 'P14'; Name = '索引体积';                Budget = '0.8× 文本量';  Where = '' }
    @{ Id = 'P15'; Name = '迁移（10 万封）';          Budget = '20 s';        Where = '' }
    @{ Id = 'P16'; Name = '备份（两个库）';           Budget = '30 s';        Where = '需要库里有第二个库（user.db）与 manifest' }
)

function Write-BudgetCoverage {
    param([string[]]$Log)

    Write-Host ""
    Write-Host "=== 性能预算覆盖（design/15 §4 的 16 项）===" -ForegroundColor Cyan
    Write-Host ("  {0,-4} {1,-22} {2,-12} {3}" -f 'ID', '指标', '预算', '本次')
    foreach ($b in $Budgets) {
        # Measured rows come from what this run printed. Several items print more than once (P4 has
        # four shapes), so every printed line is kept, because a budget measured four ways should
        # show four numbers rather than whichever happened to be last.
        $lines = @($Log | Where-Object { $_ -match ('^' + $b.Id + '\s') })
        $value = if ($b.Where) {
            "未测 —— $($b.Where)"
        } elseif ($lines.Count -gt 0) {
            ($lines -join '  |  ')
        } else {
            "未测 —— 本次输出里没有这一项的行（说明测量被删了或改名了）"
        }
        Write-Host ("  {0,-4} {1,-22} {2,-12} {3}" -f $b.Id, $b.Name, $b.Budget, $value)
    }
}

function Assert-BudgetCoverage {
    param($Budgets)

    # The roadmap's hard requirement, made mechanical: this table must BE the sixteen. Adding a
    # budget to design/15 without adding it here (or dropping one here) fails the run instead of
    # quietly measuring fifteen.
    $ids = @($Budgets | ForEach-Object { $_.Id })
    $expected = 1..16 | ForEach-Object { "P$_" }
    $missing = @($expected | Where-Object { $ids -notcontains $_ })
    $extra = @($ids | Where-Object { $expected -notcontains $_ })
    $dupes = @($ids | Group-Object | Where-Object { $_.Count -gt 1 } | ForEach-Object { $_.Name })

    if ($ids.Count -ne 16 -or $missing.Count -gt 0 -or $extra.Count -gt 0 -or $dupes.Count -gt 0) {
        throw ("性能预算覆盖表不是 16 项：现有 $($ids.Count) 项；缺 $($missing -join ',')；多 $($extra -join ',')；重复 $($dupes -join ',')。design/15 §4 有 16 项，脚本必须一一对应（design/19 §4 的硬要求）。")
    }
    Write-Host "budget coverage: all 16 items listed (measured ones show this run's numbers)" -ForegroundColor Green
}

# The fixture is large and reproducible; keeping it only wastes disk.
$log = New-TemporaryFile
try {
    # `--nocapture` because the whole point is the printed table. Both ignored tests are run:
    # `bench_storage_path` is the storage path, and `bench_message_build` is P10 — which used to
    # exist in the harness and never be invoked by the script that claims to measure sixteen items.
    cargo test $profile -p mail-core --test bench_fixture bench_storage_path -- --ignored --nocapture 2>&1 |
        Tee-Object -FilePath $log
    $code = $LASTEXITCODE
    cargo test $profile -p mail-core --test bench_fixture bench_message_build -- --ignored --nocapture 2>&1 |
        Tee-Object -FilePath $log -Append

    # No `-Encoding` here on purpose: `Tee-Object` writes UTF-16 under Windows PowerShell 5.1 and
    # UTF-8 under 7+, and `Get-Content` with no encoding detects both. Forcing UTF-8 would mojibake
    # the very numbers the table is supposed to show (round 70's lesson, pointed the other way).
    $measured = @(Get-Content $log)
    Write-BudgetCoverage -Log $measured
    Assert-BudgetCoverage -Budgets $Budgets

    if (Test-Path $fixture) { Remove-Item -Recurse -Force $fixture }
} finally {
    Remove-Item -Force $log -ErrorAction SilentlyContinue
    Pop-Location
}

exit $code
