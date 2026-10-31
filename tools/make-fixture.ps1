<#
.SYNOPSIS
    Build a 100k-message profile so the desktop budgets can be measured at the size they name.

.DESCRIPTION
    `design/15` §3 promised `tools/make-fixture.ps1` and `tools/bench.ps1`. The measurement side
    arrived (crates/mail-core/tests/bench_fixture.rs) but the profile side never did, so P1 and P5
    were only ever read on an 18-message profile while their rows say "100 000 messages". A budget
    that has only been measured at 1/5000 of its stated size is not a pass, it is an untested claim.

    This script closes that gap without touching the user's real profile:

      1. run the Rust fixture generator (real `Cache` API, real FTS, real migrations);
      2. lay the result out as a *separate* Tauri profile — `%APPDATA%\com.easyemail.bench` — so the
         app that opens it is the bench build (`--config '{"identifier":"com.easyemail.bench"}'`),
         never the installed one;
      3. write an `accounts.json` whose IMAP/SMTP point at 127.0.0.1:1 (connection refused), which
         is what makes a cold start a *cache* measurement rather than a network one.

    The real profile (`%APPDATA%\com.easyemail.desktop`) is never read or written. That is the whole
    point of the separate identifier: a fixture that overwrote someone's mailbox to get a number
    would be the most expensive kind of measurement.

.PARAMETER Messages
    Rows to generate. 100000 is the number the budget table names.

.PARAMETER Profile
    Where the throwaway profile goes. Defaults to `%APPDATA%\com.easyemail.bench`.

.PARAMETER SkipGenerate
    Reuse `target\bench-fixture\cache.db` if it is already there (the generator deletes and rebuilds).

.EXAMPLE
    .\tools\make-fixture.ps1

    Then, once:
      cargo tauri build --no-bundle --config '{"identifier":"com.easyemail.bench"}'
      Copy-Item target\release\easy-email.exe target\release\easy-email-bench.exe

    And measure:
      $env:WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS = '--remote-debugging-port=9222'
      .\tools\bench-app.ps1 -Exe target\release\easy-email-bench.exe -Seconds 20 -SkipSwitch
#>
[CmdletBinding()]
param(
    [int]$Messages = 100000,
    [string]$Profile = (Join-Path $env:APPDATA 'com.easyemail.bench'),
    [switch]$SkipGenerate
)

$ErrorActionPreference = 'Stop'
Push-Location (Join-Path $PSScriptRoot '..')

try {
    $fixtureDir = Join-Path (Get-Location) 'target\bench-fixture'
    $db = Join-Path $fixtureDir 'cache.db'

    if (-not $SkipGenerate) {
        Write-Host "Generating $Messages messages (release build of the fixture test)..." -ForegroundColor Cyan
        $env:EE_BENCH_MESSAGES = "$Messages"
        $env:EE_BENCH_KEEP = '1'
        $env:EE_BENCH_DIR = $fixtureDir
        # The generator prints the whole P2/P4/P7/P9/P10/P13/P14/P15 table on the way out; that output
        # is the *storage* half of the budget and is worth reading on its own.
        & cargo test --release -p mail-core --test bench_fixture bench_storage_path -- --ignored --nocapture
        if ($LASTEXITCODE -ne 0) { throw "fixture generation failed (exit $LASTEXITCODE)" }
    }

    if (-not (Test-Path $db)) { throw "no fixture database at $db (run without -SkipGenerate)" }

    New-Item -ItemType Directory -Force -Path $Profile | Out-Null
    $target = Join-Path $Profile 'cache.db'
    Write-Host "Copying fixture into $Profile ..." -ForegroundColor Cyan
    Copy-Item $db $target -Force
    # Stale sidecars from an earlier run would be applied on top of the new database.
    foreach ($sidecar in @("$target-wal", "$target-shm")) {
        if (Test-Path $sidecar) { Remove-Item $sidecar -Force }
    }

    # A dead endpoint is deliberate. A live one would make the cold start wait for a handshake and
    # the number would describe the network; P1 is about the cache leg.
    $accounts = @{
        version  = 1
        accounts = @(
            @{
                id               = 'bench@example.com'
                email            = 'bench@example.com'
                display_name     = 'Bench'
                username         = 'bench@example.com'
                provider_id      = 'qq'
                imap             = @{ host = '127.0.0.1'; port = 1; security = 'ssl' }
                smtp             = @{ host = '127.0.0.1'; port = 1; security = 'ssl' }
                needs_id_command = $false
                max_connections  = 3
                auth             = 'password'
                smtp_none        = $false
            }
        )
    }
    # No BOM: the app parses this file with serde_json, which rejects a leading U+FEFF. `Set-Content
    # -Encoding UTF8` on PowerShell 5.1 writes one, so the bytes are written explicitly instead.
    $json = $accounts | ConvertTo-Json -Depth 8
    [System.IO.File]::WriteAllText(
        (Join-Path $Profile 'accounts.json'),
        $json,
        (New-Object System.Text.UTF8Encoding($false))
    )

    $size = [math]::Round((Get-Item $target).Length / 1MB, 1)
    Write-Host ""
    Write-Host "Fixture profile ready: $Profile (cache.db $size MB, $Messages messages)" -ForegroundColor Green
    Write-Host "Your real profile (%APPDATA%\com.easyemail.desktop) was not touched." -ForegroundColor Green
    Write-Host ""
    Write-Host "Next:" -ForegroundColor Cyan
    Write-Host '  cargo tauri build --no-bundle --config ''{"identifier":"com.easyemail.bench"}'''
    Write-Host '  Copy-Item target\release\easy-email.exe target\release\easy-email-bench.exe'
    Write-Host '  $env:WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS = ''--remote-debugging-port=9222'''
    Write-Host '  .\tools\bench-app.ps1 -Exe target\release\easy-email-bench.exe -Seconds 20 -SkipSwitch'
} finally {
    Remove-Item Env:\EE_BENCH_MESSAGES, Env:\EE_BENCH_KEEP, Env:\EE_BENCH_DIR -ErrorAction SilentlyContinue
    Pop-Location
}
