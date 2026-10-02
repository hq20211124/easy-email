<#
    Pre-commit gate for Easy Email.

    Runs the checks that have actually caught bugs in this project. The frontend syntax check is
    first because it is the cheapest and it caught a real defect: a stray backtick inside a
    template literal in src/app.js made the whole renderer fail to parse, which looked exactly
    like "the UI is broken" while `cargo build` stayed perfectly green.

    Usage:  .\tools\check.ps1
            .\tools\check.ps1 -Network     # also probe the provider database against real servers
#>
[CmdletBinding()]
param(
    [switch]$Network
)

# Deliberately NOT 'Stop': cargo writes progress to stderr, and Windows PowerShell turns a native
# command's stderr into a terminating error under 'Stop', which aborts the run on success.
# Exit codes are checked explicitly instead.
$ErrorActionPreference = 'Continue'

Push-Location (Join-Path $PSScriptRoot '..')
$script:Failed = @()

function Step {
    param([string]$Name, [scriptblock]$Body)

    Write-Host ""
    Write-Host "=== $Name ===" -ForegroundColor Cyan
    & $Body
    if ($LASTEXITCODE -ne 0) {
        $script:Failed += $Name
        Write-Host "FAILED: $Name (exit $LASTEXITCODE)" -ForegroundColor Red
    } else {
        Write-Host "ok: $Name" -ForegroundColor Green
    }
}

try {
    # ------------------------------------------------------------ frontend

    Step 'frontend syntax (node --check)' {
        if (-not (Get-Command node -ErrorAction SilentlyContinue)) {
            Write-Host "node not found on PATH; skipping. Install Node.js to enable this check." -ForegroundColor Yellow
            $global:LASTEXITCODE = 0
            return
        }
        # Every script under src/, not just app.js. The stray-backtick bug that blanked the whole
        # window lived in one file; the next one can live in another, and this check is nearly free.
        # tools/*.mjs is included because a broken inspector is worse than no inspector: it would be
        # trusted to say the app is fine.
        $scripts = @(Get-ChildItem 'src/*.js', 'tools/*.mjs')
        foreach ($script in $scripts) {
            node --check $script.FullName 2>&1
            if ($LASTEXITCODE -ne 0) {
                $global:LASTEXITCODE = $LASTEXITCODE
                return
            }
        }
        Write-Host "checked $($scripts.Count) script(s)"
        $global:LASTEXITCODE = 0
    }

    Step 'frontend assets referenced by index.html exist' {
        # A renamed or mistyped <script src> / <link href> fails silently at runtime: the page just
        # boots without that file. theme.js is load-bearing — without it
        # `window.__easyEmailTheme` is undefined and the theme button throws the moment it is used.
        $html = Get-Content 'src/index.html' -Raw
        $refs = [regex]::Matches($html, '(?:src|href)="([^"]+)"') |
            ForEach-Object { $_.Groups[1].Value } |
            Where-Object { $_ -notmatch '^(https?:|data:|#)' } |
            Sort-Object -Unique

        $missing = @($refs | Where-Object { -not (Test-Path (Join-Path 'src' $_)) })
        if ($missing.Count -gt 0) {
            Write-Host "referenced by index.html but missing from src/: $($missing -join ', ')" -ForegroundColor Red
            $global:LASTEXITCODE = 1
            return
        }

        Write-Host "verified $($refs.Count) referenced asset(s): $($refs -join ', ')"
        $global:LASTEXITCODE = 0
    }

    Step 'frontend element ids resolve' {
        # Every element the renderer looks up by id must exist. Two lookups are in play:
        #   $('id')                  -> static markup in index.html
        #   querySelector('#id')     -> either static markup or a dialog built in app.js
        # The dialog fields are template strings in app.js, so both files count as definitions.
        $html = Get-Content 'src/index.html' -Raw
        $js = Get-Content 'src/app.js' -Raw
        $defined = "$html`n$js"

        $staticIds = [regex]::Matches($js, "\`$\('([a-z0-9-]+)'\)") |
            ForEach-Object { $_.Groups[1].Value } |
            Sort-Object -Unique

        $missingStatic = @($staticIds | Where-Object { $html -notmatch ('id="' + [regex]::Escape($_) + '"') })
        if ($missingStatic.Count -gt 0) {
            Write-Host "ids used in app.js but absent from index.html: $($missingStatic -join ', ')" -ForegroundColor Red
            $global:LASTEXITCODE = 1
            return
        }

        $queriedIds = [regex]::Matches($js, "querySelector\('#([a-z0-9-]+)'\)") |
            ForEach-Object { $_.Groups[1].Value } |
            Sort-Object -Unique

        $missingQueried = @($queriedIds | Where-Object { $defined -notmatch ('id="' + [regex]::Escape($_) + '"') })
        if ($missingQueried.Count -gt 0) {
            Write-Host "ids queried but never defined anywhere: $($missingQueried -join ', ')" -ForegroundColor Red
            $global:LASTEXITCODE = 1
            return
        }

        Write-Host "checked $($staticIds.Count) static ids and $($queriedIds.Count) queried ids"
        $global:LASTEXITCODE = 0
    }

    Step 'mail bodies still block remote images by default' {
        # The security invariant, pinned as source text because it lives in three files that must
        # agree. A `srcdoc` iframe INHERITS its embedder's CSP and the two policies intersect, so:
        #   * the shell must allow https/http, or the per-message opt-in cannot ever work
        #     (this was a real bug: "显示图片" changed the inner policy, and every remote image
        #     still failed with naturalWidth = 0)
        #   * the inner DEFAULT must not allow https/http, and that is what actually blocks
        #     tracking pixels without the user's consent
        $html = Get-Content 'src/index.html' -Raw
        $conf = Get-Content 'src-tauri/tauri.conf.json' -Raw
        $js = Get-Content 'src/app.js' -Raw
        $problems = @()

        foreach ($pair in @(@('src/index.html', $html), @('src-tauri/tauri.conf.json', $conf))) {
            if ($pair[1] -notmatch 'img-src[^;]*https:') {
                $problems += "$($pair[0]): shell img-src must permit https:, else the opt-in is dead"
            }
        }

        # Compare the ternary without its quotes, so this does not depend on ' vs " in app.js.
        $flat = $js -replace '["'']', ''
        if ($flat -notmatch 'allowRemoteImages \? img-src data: blob: https: http: : img-src data: blob:') {
            $problems += 'src/app.js: the allowRemoteImages branch no longer selects the permissive policy'
        }

        if ($problems.Count -gt 0) {
            $problems | ForEach-Object { Write-Host $_ -ForegroundColor Red }
            $global:LASTEXITCODE = 1
            return
        }
        Write-Host 'shell permits https/http; the per-message default still blocks it'
        $global:LASTEXITCODE = 0
    }

    Step 'mail body layout rules stay safe for percentage layouts' {
        # The rules that make a fixed-width newsletter fit the reading pane have to neutralise pixel
        # widths (inline style="width:720px"), and the only way to beat an inline style is
        # !important. That is precisely where the danger is: applying width:auto broadly collapses
        # a table with width="100%" to 6% of the pane, which would wreck ordinary mail in a way no
        # offline test can see. The exclusion of the percentage forms is the load-bearing part.
        $js = Get-Content 'src/app.js' -Raw
        $problems = @()

        if ($js -notmatch 'img \{ max-width: 100% !important') {
            $problems += 'src/app.js: body images lost their !important max-width'
        }
        if ($js -notmatch '\[width\$\="%"\]') {
            $problems += 'src/app.js: the pixel-width rules no longer exclude width="100%" attributes'
        }
        if ($js -notmatch '\[style\*="width:"\]:not\(\[style\*="%"\]\)') {
            $problems += 'src/app.js: the pixel-width rules no longer exclude inline width:100%'
        }
        if ($js -match 'max-height:\s*\d') {
            $problems += 'src/app.js: an image height cap shrinks tall banners to an unreadable sliver'
        }

        if ($problems.Count -gt 0) {
            $problems | ForEach-Object { Write-Host $_ -ForegroundColor Red }
            $global:LASTEXITCODE = 1
            return
        }
        Write-Host 'pixel widths neutralised; percentage widths and image height untouched'
        $global:LASTEXITCODE = 0
    }

    Step 'local search keeps the CJK-capable tokenizer' {
        # Behaviourally pinned by two unit tests (`two_character_chinese_words_are_searchable` and
        # `the_index_and_the_query_use_the_same_transformation`), which `cargo test` below will run.
        # This step exists for the *intent*: the FTS5 `trigram` tokenizer is the one every guide
        # recommends for substring matching, and it is wrong here — it needs three characters, and
        # most Chinese words are two. A future session reaching for it should see why not.
        $cache = Get-Content 'crates/mail-core/src/cache.rs' -Raw
        $problems = @()
        if ($cache -notmatch "tokenize = 'unicode61'") {
            $problems += 'cache.rs: FTS5 tokenizer must stay unicode61 — trigram cannot match 2-character Chinese words such as 账单'
        }
        if ($cache -notmatch 'to_tokens\(&') {
            $problems += 'cache.rs: indexed text must go through to_tokens(), or queries built by to_query() can never match it'
        }
        if ($problems.Count -gt 0) {
            $problems | ForEach-Object { Write-Host $_ -ForegroundColor Red }
            $global:LASTEXITCODE = 1
            return
        }
        Write-Host 'unicode61 + to_tokens(): 2-character Chinese search stays possible'
        $global:LASTEXITCODE = 0
    }

    Step 'every command the frontend calls is registered' {
        # The renderer and the Rust side share nothing but strings: a typo in `invoke('list_messege')`
        # fails only when that button is clicked, and the name also has to appear in
        # `generate_handler!` or Tauri rejects the call at runtime. Neither is covered by any test,
        # so the contract is checked here instead.
        $js = Get-Content 'src/app.js' -Raw
        $lib = Get-Content 'src-tauri/src/lib.rs' -Raw

        $called = [regex]::Matches($js, "invoke\('([a-z_]+)'") |
            ForEach-Object { $_.Groups[1].Value } |
            Sort-Object -Unique

        $problems = @()
        foreach ($name in $called) {
            if ($lib -notmatch "async fn $name\(" -and $lib -notmatch "fn $name\(") {
                $problems += "app.js calls invoke('$name') but no such function exists in lib.rs"
                continue
            }
            if ($lib -notmatch "(?m)^\s+$name,\s*$") {
                $problems += "'$name' exists but is missing from generate_handler!"
            }
        }

        if ($problems.Count -gt 0) {
            $problems | ForEach-Object { Write-Host $_ -ForegroundColor Red }
            $global:LASTEXITCODE = 1
            return
        }
        Write-Host "checked $($called.Count) commands: all defined and registered"
        $global:LASTEXITCODE = 0
    }

    # ------------------------------------------------------------ rust

    Step 'cargo fmt --check' {
        cargo fmt --all -- --check 2>&1
    }

    Step 'cargo clippy (-D warnings)' {
        cargo clippy --workspace --all-targets -- -D warnings 2>&1
    }

    Step 'cargo build (catches linker-level warnings)' {
        # clippy does not link, so `linker_messages` warnings only show up here. A leftover
        # `cdylib` crate type once produced one on every single build.
        $out = cargo build --workspace --message-format short 2>&1 | Out-String
        $code = $LASTEXITCODE
        $warnings = @($out -split "`n" | Where-Object { $_ -match '^warning' })
        if ($warnings.Count -gt 0) {
            Write-Host ($warnings -join "`n") -ForegroundColor Red
            $global:LASTEXITCODE = 1
        } else {
            $global:LASTEXITCODE = $code
        }
    }

    Step 'cargo test (offline)' {
        cargo test --workspace 2>&1
    }

    Step 'desktop binary is a GUI subsystem (no console window)' {
        # Regression guard. The Tauri template ships
        # `cfg_attr(not(debug_assertions), windows_subsystem = "windows")`, which left every debug
        # build as a console binary, so launching the exe from Explorer opened a cmd window behind
        # the app. The PE header is checked rather than the source, because that is what Windows
        # actually reads. Subsystem 2 = Windows GUI, 3 = console.
        $exe = 'target/debug/easy-email.exe'
        if (-not (Test-Path $exe)) {
            Write-Host "$exe missing; run the cargo build step first" -ForegroundColor Red
            $global:LASTEXITCODE = 1
            return
        }
        $bytes = [System.IO.File]::ReadAllBytes((Resolve-Path $exe))
        $peOffset = [System.BitConverter]::ToInt32($bytes, 0x3C)
        $subsystem = [System.BitConverter]::ToUInt16($bytes, $peOffset + 24 + 68)
        if ($subsystem -ne 2) {
            Write-Host "PE subsystem is $subsystem, expected 2 (GUI): a console window will appear" -ForegroundColor Red
            $global:LASTEXITCODE = 1
            return
        }
        Write-Host "PE subsystem = 2 (Windows GUI)"
        $global:LASTEXITCODE = 0
    }

    if ($Network) {
        Step 'provider database vs live servers' {
            cargo test -p mail-core --test provider_reachability -- --ignored --nocapture 2>&1
        }
    }
} finally {
    Pop-Location
}

# ---------------------------------------------------------------- summary

Write-Host ""
if ($script:Failed.Count -gt 0) {
    Write-Host "FAILED: $($script:Failed -join ', ')" -ForegroundColor Red
    exit 1
}
Write-Host "All checks passed." -ForegroundColor Green
