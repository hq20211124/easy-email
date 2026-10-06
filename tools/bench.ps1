<#
    Storage-path benchmark for Easy Email (design/15 §3/§4).

    measures the numbers that live in the database layer: ingest throughput, list and search
    latency, and the size of the database and the FTS index. It drives the ignored test in
    crates/mail-core/tests/bench_fixture.rs, which is where the fixture is generated.

    What this does NOT measure — cold start, resident memory, idle CPU, scroll smoothness, IPC
    payload time. Those need the running desktop shell and are listed as *unmeasured* in
    reports/performance.md rather than quietly implied by these numbers.

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

# `--nocapture` because the whole point is the printed table.
cargo test $profile -p mail-core --test bench_fixture bench_storage_path -- --ignored --nocapture
$code = $LASTEXITCODE

# The fixture is large and reproducible; keeping it only wastes disk.
if (Test-Path $fixture) { Remove-Item -Recurse -Force $fixture }

Pop-Location
exit $code
