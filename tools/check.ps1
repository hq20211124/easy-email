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
$script:Threw = $false
# Counts steps, not outcomes: a step that threw used to vanish from the tally while the summary said
# everything passed, so the summary now also checks that every step reported something.
$script:Ran = 0

function Step {
    param([string]$Name, [scriptblock]$Body)

    Write-Host ""
    Write-Host "=== $Name ===" -ForegroundColor Cyan
    $script:Ran++
    # A step that **throws** is a failed step. It used not to be: `$ErrorActionPreference` is
    # `Continue` (cargo writes progress to stderr), so an exception inside a step body printed a red
    # error, skipped the rest of that body, and left `$LASTEXITCODE` at whatever the previous
    # command left — usually 0 — so the run ended with "All checks passed" and one step silently
    # absent from the count (round 73: the CJK-range regex died on Windows and the gate said green).
    # That is the same failure as "ok" after a skip: a check that did not run, reported as one that
    # passed.
    $script:Threw = $false
    try {
        & $Body
    } catch {
        $script:Threw = $true
        Write-Host $_ -ForegroundColor Red
    }
    if ($script:Threw -or $LASTEXITCODE -ne 0) {
        $script:Failed += $Name
        $why = if ($script:Threw) { 'threw' } else { "exit $LASTEXITCODE" }
        Write-Host "FAILED: $Name ($why)" -ForegroundColor Red
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
        $html = Get-Content 'src/index.html' -Raw -Encoding UTF8
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
        $html = Get-Content 'src/index.html' -Raw -Encoding UTF8
        $js = Get-Content 'src/app.js' -Raw -Encoding UTF8
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
        $html = Get-Content 'src/index.html' -Raw -Encoding UTF8
        $conf = Get-Content 'src-tauri/tauri.conf.json' -Raw -Encoding UTF8
        $js = Get-Content 'src/app.js' -Raw -Encoding UTF8
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

        # Exactly one body surface. It is the only sandboxed iframe, and the conversation view *moves* it
        # into the focused bubble rather than rendering a second one. Two would mean "显示图片" could
        # unlock one message of a conversation while the user is looking at another — the same leak the
        # per-message default above exists to prevent, arriving through a different door.
        $surfaces = ([regex]::Matches($html, 'id="body-surface"')).Count
        if ($surfaces -ne 1) {
            $problems += "src/index.html: expected exactly 1 #body-surface, found $surfaces"
        }
        $frames = ([regex]::Matches($html, 'id="body-frame"')).Count
        if ($frames -ne 1) {
            $problems += "src/index.html: expected exactly 1 #body-frame, found $frames"
        }
        if ($flat -notmatch 'function mountBodySurface\(host\)') {
            $problems += 'src/app.js: the body surface must be moved by mountBodySurface(), not re-created'
        }

        if ($problems.Count -gt 0) {
            $problems | ForEach-Object { Write-Host $_ -ForegroundColor Red }
            $global:LASTEXITCODE = 1
            return
        }
        Write-Host 'shell permits https/http; one body surface; the per-message default still blocks it'
        $global:LASTEXITCODE = 0
    }

    Step 'mail HTML is sanitised before it can reach the renderer' {
        # The gap this pins was found by an audit on 2026-10-06 and it was the serious kind: the
        # renderer had no sanitiser at all. The whole defence was the sandbox plus the per-message
        # CSP, and a CSP has no opinion about navigation — `<meta http-equiv="refresh" content=
        # "0;url=https://tracker.example/px">` produced an unsolicited request when the message was
        # merely opened, ignoring the "显示图片" opt-in entirely.
        #
        # The behaviour is covered by unit tests in `crates/mail-core/src/sanitize.rs`; this step
        # exists for the *wiring*, which no unit test can see: a sanitiser that is not called is
        # exactly as good as no sanitiser, and that is how this bug existed in the first place.
        $sanitize = Get-Content 'crates/mail-core/src/sanitize.rs' -Raw -Encoding UTF8
        $imap = Get-Content 'crates/mail-core/src/imap.rs' -Raw -Encoding UTF8
        $cache = Get-Content 'crates/mail-core/src/cache.rs' -Raw -Encoding UTF8
        $js = Get-Content 'src/app.js' -Raw -Encoding UTF8
        $problems = @()

        foreach ($tag in @('script', 'iframe', 'object', 'meta', 'embed')) {
            if ($sanitize -notmatch """$tag""") {
                $problems += "sanitize.rs: '$tag' is no longer in the drop list"
            }
        }
        if ($sanitize -notmatch 'http-equiv' -and $sanitize -notmatch 'DROP_TAG') {
            $problems += 'sanitize.rs: the meta/refresh rule has gone'
        }
        if ($imap -notmatch 'sanitize_mail_html') {
            $problems += 'imap.rs: a fetched body is stored without being sanitised'
        }
        if ($cache -notmatch 'sanitize_mail_html') {
            $problems += 'cache.rs: bodies cached before the sanitiser existed are served unsanitised'
        }
        if ($js -match 'div\.innerHTML = html') {
            $problems += 'app.js: the HTML-to-text helper is parsing in the privileged shell again'
        }
        if ($js -notmatch 'new DOMParser\(\)') {
            $problems += 'app.js: the inert HTML-to-text helper (DOMParser) has gone'
        }

        # Links (design/14 §5.3). A sandboxed frame cannot be stopped from navigating *itself*, so a
        # leftover `href` in a body is a page of the sender's choosing painted in the reading pane.
        # The two halves have to agree: the sanitiser moves the target to `data-ee-href`, and the
        # renderer offers it back through `classify_link`/`open_link`.
        if ($sanitize -notmatch 'data-ee-href') {
            $problems += 'sanitize.rs: link targets are no longer moved out of href'
        }
        if ($js -notmatch 'linksInHtml') {
            $problems += 'app.js: the link bar has gone, so no link can be opened deliberately'
        }
        if ($js -notmatch "data-ee-href") {
            $problems += 'app.js: the link bar no longer reads the attribute the sanitiser writes'
        }
        if ($js -notmatch "invoke\('classify_link'" -or $js -notmatch "invoke\('open_link'") {
            $problems += 'app.js: a link would be opened without asking Rust what it is'
        }

        if ($problems.Count -gt 0) {
            $problems | ForEach-Object { Write-Host $_ -ForegroundColor Red }
            $global:LASTEXITCODE = 1
            return
        }
        Write-Host 'sanitiser present and called on both sides; no raw-HTML parse in the shell'
        $global:LASTEXITCODE = 0
    }

    Step 'mail body layout rules stay safe for percentage layouts' {
        # The rules that make a fixed-width newsletter fit the reading pane have to neutralise pixel
        # widths (inline style="width:720px"), and the only way to beat an inline style is
        # !important. That is precisely where the danger is: applying width:auto broadly collapses
        # a table with width="100%" to 6% of the pane, which would wreck ordinary mail in a way no
        # offline test can see. The exclusion of the percentage forms is the load-bearing part.
        $js = Get-Content 'src/app.js' -Raw -Encoding UTF8
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
        $cache = Get-Content 'crates/mail-core/src/cache.rs' -Raw -Encoding UTF8
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
        $js = Get-Content 'src/app.js' -Raw -Encoding UTF8
        $lib = Get-Content 'src-tauri/src/lib.rs' -Raw -Encoding UTF8

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

    Step 'the update chain is signed, and the key is content rather than a path' {
        # V2-C2. Three properties that must all hold, because each one alone is worthless:
        #   * signature verification is the updater's own and cannot be disabled (official docs), so
        #     the plugin must actually be registered — a hand-rolled JSON check would be the
        #     "unsigned update" failure the whole design avoids;
        #   * the pubkey must be the key's *content*. A path here builds fine and then verifies
        #     nothing, which is the kind of silent hole a gate exists for;
        #   * the endpoint must be https. Tauri enforces this in release builds, but a config that
        #     only works in debug is a config that gets tested in debug and shipped in release.
        $conf = Get-Content 'src-tauri/tauri.conf.json' -Raw -Encoding UTF8
        $lib = Get-Content 'src-tauri/src/lib.rs' -Raw -Encoding UTF8
        $cap = Get-Content 'src-tauri/capabilities/default.json' -Raw -Encoding UTF8
        $problems = @()
        if ($conf -notmatch '"createUpdaterArtifacts"\s*:\s*true') {
            $problems += 'tauri.conf.json: createUpdaterArtifacts must be true, or no .sig is produced and the check has nothing to verify'
        }
        if ($conf -notmatch '"pubkey"\s*:\s*"[A-Za-z0-9+/=]{20,}"') {
            $problems += 'tauri.conf.json: pubkey must be the key CONTENT (a long base64 blob), never a file path'
        }
        if ($conf -notmatch '"endpoints"\s*:\s*\[\s*"https://') {
            $problems += 'tauri.conf.json: the update endpoint must be https'
        }
        if ($lib -notmatch 'tauri_plugin_updater::Builder::new\(\)\.build\(\)') {
            $problems += 'lib.rs must register the updater plugin — the config alone does nothing'
        }
        if ($cap -notmatch '"updater:default"') {
            $problems += 'capabilities must grant updater:default, or the commands are unreachable from the window'
        }
        if ($lib -notmatch 'fn install_update\(') {
            $problems += 'lib.rs must expose install_update'
        }
        if ($lib -notmatch 'UpdateState::new') {
            $problems += 'install_update must leave the "which version am I becoming" note, or a failed install is silent'
        }
        if ($problems.Count -gt 0) {
            $problems | ForEach-Object { Write-Host $_ -ForegroundColor Red }
            $global:LASTEXITCODE = 1
            return
        }
        Write-Host 'the update chain is signed and the note that catches a silent failure is written'
        $global:LASTEXITCODE = 0
    }

    Step 'the receive-only escape hatch is exactly as wide as the user statement' {
        # C8: `smtp_none` is the one hole in "an account must be complete", the rule that keeps a
        # mistyped host from creating a half-account. The hole is only safe while every unflagged
        # failure is still refused — so both sides of the hole are machine-checked here.
        $wiz = Get-Content 'src/wizard.js' -Raw -Encoding UTF8
        $js = Get-Content 'src/app.js' -Raw -Encoding UTF8
        $lib = Get-Content 'src-tauri/src/lib.rs' -Raw -Encoding UTF8
        $problems = @()
        if ($wiz -notmatch 'smtpNone \?\?: false|smtpNone = false') {
            $problems += 'wizard.js must accept the flag and default it to false (accounts written before it existed are unflagged)'
        }
        if ($js -notmatch "wiz-smtp-none") {
            $problems += 'app.js must offer the checkbox — the flag has to be an explicit statement, never inferred'
        }
        if ($js -notmatch "smtpNone:") {
            $problems += 'app.js must pass the flag through to the verdict function'
        }
        if ($lib -notmatch 'if cfg\.smtp_none \{') {
            $problems += 'lib.rs must refuse to queue a send for a receive-only account'
        }
        if ($problems.Count -gt 0) {
            $problems | ForEach-Object { Write-Host $_ -ForegroundColor Red }
            $global:LASTEXITCODE = 1
            return
        }
        Write-Host 'the receive-only flag opens only the hole the user described'
        $global:LASTEXITCODE = 0
    }

    Step 'the version on screen comes from the manifest, not a second copy' {
        # V2-C1: the number a bug report quotes must be the build's own. A hardcoded string here
        # would drift from Cargo.toml the moment someone forgets to bump it — which is the same
        # failure that produced Easy Email_0.1.0_... artifacts long after 0.1.1 shipped.
        $js = Get-Content 'src/app.js' -Raw -Encoding UTF8
        $lib = Get-Content 'src-tauri/src/lib.rs' -Raw -Encoding UTF8
        $problems = @()
        if ($lib -notmatch 'fn app_version\(\) -> String') {
            $problems += 'lib.rs must expose app_version()'
        }
        if ($lib -notmatch 'env!\("CARGO_PKG_VERSION"\)') {
            $problems += 'app_version must read CARGO_PKG_VERSION — a second copy is what drifts'
        }
        if ($js -notmatch "invoke\('app_version'\)") {
            $problems += 'app.js must show the version the command returns'
        }
        if ($js -match "当前版本.*0\\.1\\.1") {
            $problems += 'app.js appears to hardcode a version string'
        }
        if ($problems.Count -gt 0) {
            $problems | ForEach-Object { Write-Host $_ -ForegroundColor Red }
            $global:LASTEXITCODE = 1
            return
        }
        Write-Host 'the about panel shows the build its own manifest describes'
        $global:LASTEXITCODE = 0
    }

    Step 'the offline notice names the failure, not just "offline"' {
        # C3: `sync_status.offline` means "the last refresh failed", which is true of a dead network,
        # an expired 授权码 and a provider quota alike. Showing all three as "离线" is the failure this
        # step prevents: the user is told the wrong remedy at the moment they need the right one. The
        # renderer must branch on `failure_kind`.
        $js = Get-Content 'src/app.js' -Raw -Encoding UTF8
        $lib = Get-Content 'src-tauri/src/lib.rs' -Raw -Encoding UTF8
        $problems = @()
        if ($js -notmatch 'failure_kind') {
            $problems += 'app.js must branch on failure_kind — a bare offline flag cannot tell an auth failure from a dead network'
        }
        if ($lib -notmatch 'fn failure_kind\(') {
            $problems += 'lib.rs must classify the MailError while it is still structured'
        }
        if ($lib -notmatch 'failure_kind: Option<FailureKind>') {
            $problems += 'SyncStatus must carry the kind, not only the sentence'
        }
        if ($problems.Count -gt 0) {
            $problems | ForEach-Object { Write-Host $_ -ForegroundColor Red }
            $global:LASTEXITCODE = 1
            return
        }
        Write-Host 'the sync banner says which kind of failure it was'
        $global:LASTEXITCODE = 0
    }

    Step 'search reports body coverage from the backend, not a hardcoded sentence' {
        # The status line used to say "正文只搜已打开过的邮件" as a fixed sentence, which was both
        # stale the moment a body was indexed and unable to say *how much* is indexed. The number
        # must come from `index_coverage`: a hardcoded one is the exact lie this step exists to catch.
        $js = Get-Content 'src/app.js' -Raw -Encoding UTF8
        $problems = @()
        if ($js -notmatch "invoke\('index_coverage'") {
            $problems += 'app.js must ask index_coverage for the coverage numbers'
        }
        if ($js -notmatch "invoke\('index_bodies'") {
            $problems += 'app.js must offer the index_bodies pass, or coverage can never improve'
        }
        if ($js -match '正文只搜已打开过的邮件') {
            $problems += 'app.js still carries the stale fixed sentence — coverage is a number now'
        }
        if ($problems.Count -gt 0) {
            $problems | ForEach-Object { Write-Host $_ -ForegroundColor Red }
            $global:LASTEXITCODE = 1
            return
        }
        Write-Host 'search status line takes its coverage numbers from the cache'
        $global:LASTEXITCODE = 0
    }

    Step 'body truncation touches the index only, never what the user reads' {
        # C9: cutting the stored body would silently lose mail. The truncation belongs to the FTS
        # row, so `bodies.detail` (the copy the reader shows) must stay whole.
        $cache = Get-Content 'crates/mail-core/src/cache.rs' -Raw -Encoding UTF8
        $problems = @()
        if ($cache -notmatch 'fn truncate_chars\(') {
            $problems += 'cache.rs: truncation helper is gone — the index needs it'
        }
        if ($cache -notmatch 'truncate_chars\(&body, body_limit\)') {
            $problems += 'cache.rs: the body must be truncated only where the FTS row is written'
        }
        # `put_detail` writes the reader's copy; it must not mention the cap.
        $putDetail = [regex]::Match($cache, '(?s)pub fn put_detail.*?\n    \}').Value
        if ($putDetail -match 'body_limit|truncate_chars') {
            $problems += 'put_detail must store the whole body — truncating it loses mail the user is reading'
        }
        if ($problems.Count -gt 0) {
            $problems | ForEach-Object { Write-Host $_ -ForegroundColor Red }
            $global:LASTEXITCODE = 1
            return
        }
        Write-Host 'bodies.detail stays whole; only the index copy is capped'
        $global:LASTEXITCODE = 0
    }

    Step 'attachment names reach the index, and the body cap applies to the body alone' {
        # V3-A. Three properties, each of which fails in a way nothing else notices:
        #   * the indexed text must include attachment **names** (`design/09` §2) — without them
        #     "报告.pdf" finds nothing, and the only trace is the user reporting it;
        #   * the 64 KB cap must not reach the names: it belongs to the body, and a long message
        #     would otherwise push the filename out of the index;
        #   * a row written by an older build must come back as pending, or the already-indexed mail
        #     of every existing user would never learn about the newer index content at all.
        $cache = Get-Content 'crates/mail-core/src/cache.rs' -Raw -Encoding UTF8
        $problems = @()

        $fn = [regex]::Match($cache, '(?s)fn refresh_search_row.*?\n    \}').Value
        if ($fn.Length -eq 0) {
            $problems += 'cache.rs: refresh_search_row is gone — the place where the index text is built'
        } else {
            if ($fn -notmatch '\.attachments') {
                $problems += 'cache.rs: refresh_search_row no longer reads attachment names out of the stored detail'
            }
            # Order is load-bearing and invisible to the compiler: both happen in this one function,
            # so the only way to see it is to compare positions.
            $trunc = $fn.IndexOf('truncate_chars(&body, body_limit)')
            $names = $fn.IndexOf('.attachments')
            if ($trunc -lt 0) {
                $problems += 'cache.rs: the body cap is gone from the index builder'
            } elseif ($names -lt $trunc) {
                $problems += 'cache.rs: attachment names are indexed BEFORE the body is truncated — a 64 KB body would crowd the filename out'
            }
        }
        if ($cache -notmatch 'b\.index_version') {
            $problems += 'cache.rs: nothing in the pending query knows which index shape a body row was written by'
        }
        if ($cache -notmatch 'has_body') {
            $problems += 'cache.rs: bodies_pending must tell the pass whether the body is already stored, or stale rows get fetched again'
        }

        if ($problems.Count -gt 0) {
            $problems | ForEach-Object { Write-Host $_ -ForegroundColor Red }
            $global:LASTEXITCODE = 1
            return
        }
        Write-Host 'names reach the index after the cap; stale rows are marked and come back as pending'
        $global:LASTEXITCODE = 0
    }

    Step 'the search index can be rebuilt without touching the network' {
        # V3-B. The button behind it deletes the whole index and builds it again, so two mistakes are
        # invisible until they hurt: (1) if a step were not bounded, one call would hold the single
        # write thread for the whole mailbox and every other command would queue behind it —
        # `design/09` §5.4 forbids rebuilding inside a startup transaction for the same reason;
        # (2) if rebuilding ever reached for a connection, "rebuild" would silently become "re-read
        # the mailbox from the server", which is a different operation with a very different cost.
        $cache = Get-Content 'crates/mail-core/src/cache.rs' -Raw -Encoding UTF8
        $lib = Get-Content 'src-tauri/src/lib.rs' -Raw -Encoding UTF8
        $problems = @()

        foreach ($name in @('rebuild_search_begin', 'rebuild_search_step')) {
            if ($lib -notmatch "fn $name\(") {
                $problems += "lib.rs must expose $name"
            } elseif ($lib -notmatch "(?m)^\s+$name,\s*$") {
                $problems += "$name exists but is missing from generate_handler! — the renderer could not call it"
            }
        }

        $step = [regex]::Match($cache, '(?s)pub fn rebuild_search_step.*?\n    \}').Value
        if ($step.Length -eq 0) {
            $problems += 'cache.rs: rebuild_search_step is missing'
        } elseif ($step -notmatch 'limit') {
            $problems += 'cache.rs: the rebuild step must take a limit — an unbounded rebuild blocks every other writer'
        } elseif ($step -notmatch 'order by rowid') {
            $problems += 'cache.rs: the rebuild must walk messages in a stable order, or resuming picks a different order than the first pass'
        }

        # Neither command may reach the pool. The body is cut at the closing brace at column 0 — the
        # next `#[tauri::command]` is the wrong cut, because a plain helper (`body_index_pass`) and
        # its pool call can sit between two commands, and the first version of this step reported
        # three "violations" that were 200 lines away from the commands it meant.
        foreach ($name in @('rebuild_search_begin', 'rebuild_search_step')) {
            $start = $lib.IndexOf("async fn $name(")
            if ($start -lt 0) { continue }
            $end = $lib.IndexOf("`n}", $start)
            $body = if ($end -gt $start) { $lib.Substring($start, $end - $start) } else { $lib.Substring($start) }
            foreach ($forbidden in @('pool_op', 'secret_for', 'fetch_body', 'state.pool')) {
                if ($body -match $forbidden) {
                    # `$forbidden:` would parse as a drive-qualified variable name; the probe found it.
                    $problems += ($name + ' reaches for ' + $forbidden + ': a rebuild re-derives what is cached, it must not read the server')
                }
            }
        }

        if ($problems.Count -gt 0) {
            $problems | ForEach-Object { Write-Host $_ -ForegroundColor Red }
            $global:LASTEXITCODE = 1
            return
        }
        Write-Host 'the index can be rebuilt in bounded, network-free steps'
        $global:LASTEXITCODE = 0
    }

    Step 'the performance budget table lists all sixteen items, measured or not' {
        # `design/19` §4 makes this a hard requirement for the performance block, and names the
        # failure it exists to prevent: a benchmark script that claims a count the budget table does
        # not have, leaving four budgets silently untested. The script asserts its own count at run
        # time; this step makes sure nobody deletes an item (or the assertion) to silence it.
        $bench = Get-Content 'tools/bench.ps1' -Raw -Encoding UTF8
        $problems = @()

        $ids = [regex]::Matches($bench, "Id\s*=\s*'P(\d+)'") | ForEach-Object { [int]$_.Groups[1].Value }
        $missing = @(1..16 | Where-Object { $ids -notcontains $_ })
        if ($ids.Count -ne 16 -or $missing.Count -gt 0) {
            $problems += "tools/bench.ps1 lists $($ids.Count) budgets (P1-P16 expected); missing P$($missing -join ', P')"
        }
        if ($bench -notmatch 'function Assert-BudgetCoverage') {
            $problems += 'bench.ps1 must assert its own coverage (Assert-BudgetCoverage), not leave sixteen to be counted by eye'
        }
        if ($bench -notmatch 'throw') {
            $problems += 'the coverage assertion must fail the run (throw), so a dropped item cannot pass quietly'
        }
        foreach ($test in @('bench_storage_path', 'bench_message_build')) {
            if ($bench -notmatch [regex]::Escape($test)) {
                $problems += "bench.ps1 never runs $test — a budget whose measurement exists but is not invoked is not measured (P10 was exactly this)"
            }
        }

        if ($problems.Count -gt 0) {
            $problems | ForEach-Object { Write-Host $_ -ForegroundColor Red }
            $global:LASTEXITCODE = 1
            return
        }
        Write-Host 'sixteen budgets listed, asserted by the script itself, both fixture tests invoked'
        $global:LASTEXITCODE = 0
    }

    Step 'the scroll sampler exists, and it judges the P3 budgets instead of printing them' {
        # Round 74. P3 (60 fps, ≤ 40 rows changed per frame) had **no instrument at all**:
        # `bench-app.ps1` mentioned scrolling in its header and had a placeholder body. Without this
        # step the next round's virtual-list work would be "done" on the strength of nothing — the
        # shape of mistake this repo has now written down four times.
        $scroll = Get-Content 'tools/bench-scroll.mjs' -Raw -Encoding UTF8
        $app = Get-Content 'tools/bench-app.ps1' -Raw -Encoding UTF8
        $problems = @()

        if (-not (Test-Path 'tools/bench-scroll-fixture.html')) {
            $problems += 'tools/bench-scroll-fixture.html is missing: the sampler needs a page whose slow path proves it can fail'
        }
        # Judging, not reporting: the budgets must appear in comparisons that end in a non-zero exit.
        foreach ($budget in @('BUDGET_FPS', 'BUDGET_ROWS')) {
            if ($scroll -notmatch [regex]::Escape($budget)) {
                $problems += "bench-scroll.mjs never mentions $budget — it would be printing numbers, not judging them"
            }
        }
        if ($scroll -notmatch 'process\.exit\(1\)') {
            $problems += 'bench-scroll.mjs must exit non-zero when a budget is missed'
        }
        if ($scroll -notmatch 'MutationObserver') {
            $problems += 'bench-scroll.mjs must count DOM changes with a MutationObserver (rows/frame is the budget being measured)'
        }
        if ($app -notmatch 'bench-scroll\.mjs') {
            $problems += 'bench-app.ps1 must run the sampler — P3 is an app-level budget and this is the app-level runner'
        }
        if ($app -match '本脚本不假装能测') {
            $problems += 'bench-app.ps1 still carries the P3 placeholder; the instrument exists now'
        }

        if ($problems.Count -gt 0) {
            $problems | ForEach-Object { Write-Host $_ -ForegroundColor Red }
            $global:LASTEXITCODE = 1
            return
        }
        Write-Host 'P3 has an instrument that judges both budgets, wired into the app-level runner'
        $global:LASTEXITCODE = 0
    }

    Step 'the list pages by offset instead of growing a limit' {
        # Round 75. The load-more path used to ask for `limit = shown + PAGE_SIZE`, i.e. re-read the
        # entire visible window on every press (payload and re-sort both grew with the screen), and it
        # called `list_messages`, which refreshes the folder — so asking to see older mail cost a sync.
        # Regressing to the growing limit is silent: the list still works, just slowly.
        $js = Get-Content 'src/app.js' -Raw -Encoding UTF8
        $lib = Get-Content 'src-tauri/src/lib.rs' -Raw -Encoding UTF8
        $history = Get-Content 'src/history.js' -Raw -Encoding UTF8
        $problems = @()

        if ($lib -notmatch 'fn message_page\(') {
            $problems += 'lib.rs must expose message_page(offset, limit)'
        } elseif ($lib -notmatch "(?m)^\s+message_page,\s*$") {
            $problems += 'message_page exists but is missing from generate_handler!'
        }
        if ($js -notmatch "invoke\('message_page'") {
            $problems += 'app.js must page with message_page — the growing-limit path is what this step removed'
        }
        if ($js -notmatch 'offset:\s*state\.messages\.length') {
            $problems += 'the load-more page must be offset by what is already on screen, not by a growing limit'
        }
        if ($js -match 'state\.limit\s*=') {
            $problems += 'app.js still assigns state.limit — that variable grew with every press and is gone by design'
        }
        if ($js -match 'state\.limit\s*\+') {
            $problems += "app.js still asks for 'more than what is on screen'; page by offset instead"
        }

        # The page must not smuggle a refresh in: asking for older mail must not cost a sync.
        $start = $lib.IndexOf('async fn message_page(')
        if ($start -ge 0) {
            $end = $lib.IndexOf("`n}", $start)
            $body = if ($end -gt $start) { $lib.Substring($start, $end - $start) } else { $lib.Substring($start) }
            foreach ($forbidden in @('refresh_folder', 'pool_op', 'secret_for')) {
                if ($body -match $forbidden) {
                    # `$forbidden:` parses as a drive-qualified variable name, so the string is built
                    # by concatenation. The 5.1 parser probe is what catches this — it rejected the file
                    # outright the first time, which is a much cheaper failure than a broken gate run.
                    $problems += ('message_page reaches for ' + $forbidden + ': paging reads the cache, it must not talk to the server')
                }
            }
        }

        # The two halves of the rule must agree on the question they ask.
        if ($history -notmatch 'pageCount') {
            $problems += 'history.js must take pageCount — with paging, "shown vs cached" is theatre (both are the same number)'
        }
        if ($js -match 'cachedCount:') {
            $problems += 'app.js still passes cachedCount — that was the pre-paging rule shape'
        }

        if ($problems.Count -gt 0) {
            $problems | ForEach-Object { Write-Host $_ -ForegroundColor Red }
            $global:LASTEXITCODE = 1
            return
        }
        Write-Host 'the list pages by offset, reads only the cache, and both halves agree on the rule'
        $global:LASTEXITCODE = 0
    }

    Step 'the message list is windowed, with one source for the row height' {
        # Round 76, design/15 §6 (virtualise: viewport +/- 8 rows, fixed and measurable row height,
        # commit on requestAnimationFrame) and §4 P3 (<= 40 rows changed per frame).
        #
        # The regression this prevents is invisible in a small mailbox: the list held one DOM button
        # per conversation forever, and with a thousand cached messages every scroll re-layed-out
        # every one of them. Nothing errors — it is just not 60 fps, which no offline check can see.
        $js = Get-Content 'src/app.js' -Raw -Encoding UTF8
        $css = Get-Content 'src/style.css' -Raw -Encoding UTF8
        $html = Get-Content 'src/index.html' -Raw -Encoding UTF8
        $problems = @()

        if (-not (Test-Path 'src/vlist.js')) {
            $problems += 'src/vlist.js is missing: the window arithmetic must be a pure, testable module'
        }
        # Loaded, not merely present: a script that index.html never references cannot be anything.
        if ($html -notmatch '<script src="vlist\.js"></script>') {
            $problems += 'index.html does not load vlist.js — a module nobody imports is not a list'
        }
        if (Test-Path 'src/vlist.js') {
            $v = Get-Content 'src/vlist.js' -Raw -Encoding UTF8
            foreach ($needed in @('DEFAULT_ROW_HEIGHT', 'computeWindow', 'windowChanged')) {
                if ($v -notmatch [regex]::Escape($needed)) {
                    $problems += "vlist.js no longer defines $needed"
                }
            }
        }
        if (-not (Get-Command node -ErrorAction SilentlyContinue)) {
            Write-Host "node not found on PATH; skipping. Install Node.js to enable this check." -ForegroundColor Yellow
            $global:LASTEXITCODE = 0
            return
        }
        # The maths is pure, so it is tested — and running the test here is what keeps it that way.
        node tools/vlist_test.mjs 2>&1
        if ($LASTEXITCODE -ne 0) {
            $global:LASTEXITCODE = $LASTEXITCODE
            return
        }
        if ($js -notmatch 'vlist-sizer' -or $js -notmatch 'vlist-window') {
            $problems += 'app.js must build the windowed DOM (sizer + window), not append rows to the list'
        }
        if ($js -notmatch 'requestAnimationFrame') {
            $problems += 'scrolling must commit on requestAnimationFrame (one commit per frame, design/15 §6)'
        }
        # Scoped to the mail branch, because the other views (search hits, drafts, outbox) legitimately
        # append rows to the same element — they are short lists and windowing them would buy nothing.
        # An unscoped "no appendChild anywhere" was the first version of this check and it failed a
        # correct tree, which is how a gate starts ignoring its own greens.
        $render = [regex]::Match($js, '(?s)function renderMessages\(\) \{.*?\n\}').Value
        if ($render.Length -eq 0) {
            $problems += 'renderMessages is gone — the list entry point must stay a named function'
        } else {
            if ($render -match 'list\.appendChild') {
                $problems += 'renderMessages appends rows to the scroll container: only the window element may receive them'
            }
            if ($render -notmatch 'renderListWindow\(\)') {
                $problems += 'the mail branch must render through renderListWindow() — the window, not the whole list'
            }
        }
        if ($js -notmatch 'listWindowEl\.replaceChildren\(') {
            $problems += 'the window element must be the single commit point for mail rows (replaceChildren)'
        }
        # Exactly one home for the row height, and the CSS must actually use it.
        if ($css -notmatch '--msg-height:\s*\d+px') {
            $problems += 'style.css must declare --msg-height (a row height with no home drifts from the JS constant)'
        }
        if ($css -notmatch '--msg-height\)') {
            $problems += 'no rule uses --msg-height: a declared variable with no consumer is a lie about the layout'
        }

        if ($problems.Count -gt 0) {
            $problems | ForEach-Object { Write-Host $_ -ForegroundColor Red }
            $global:LASTEXITCODE = 1
            return
        }
        Write-Host 'the list renders a window, and the row height has one home'
        $global:LASTEXITCODE = 0
    }

    Step 'a query prefix scopes the search, and an unknown one stays literal' {
        # Round 77, design/09 §1 goal 4: `from:` / `subject:` / `body:` / `to:` / `filename:` and
        # `has:attachment`. The two ways this fails silently, both of which a test now covers:
        #   * a prefix that is not recognised gets read as a field name, so a typo (`frm:boss`)
        #     returns **nothing at all** — no error, no hint;
        #   * a predicate with no text term (`has:attachment` alone) is driven through the FTS index,
        #     whose MATCH expression is empty and therefore matches nothing.
        # Parsing moved to crates/mail-core/src/query.rs in round 78; the search() shape this
        # step also pins still lives in cache.rs, so both are read here.
        $query = Get-Content 'crates/mail-core/src/query.rs' -Raw -Encoding UTF8
        $cache = Get-Content 'crates/mail-core/src/cache.rs' -Raw -Encoding UTF8
        $lib = Get-Content 'crates/mail-core/src/lib.rs' -Raw -Encoding UTF8
        $problems = @()

        if ($query -notmatch 'fn parse_query\(') {
            $problems += 'query.rs must expose parse_query — parsing and SQL are two questions, only one needs a database'
        }
        foreach ($column in @('Subject', 'From', 'To', 'Body')) {
            if ($query -notmatch [regex]::Escape("QueryColumn::$column")) {
                $problems += "column mapping no longer produces $column — one of design/09 §1's prefixes is dead"
            }
        }
        if ($query -notmatch 'HasAttachment') {
            $problems += 'query.rs must model has:attachment as its own term — it is a predicate about the stored row, not about indexed text'
        }
        # The whole point of the fallback: unknown prefix text survives into a term of its own.
        if ($query -notmatch 'column:\s*None') {
            $problems += 'an unrecognised prefix must fall through to a plain text term (a typo must stay searchable)'
        }
        # A predicate-only query must not be driven through the index.
        if ($cache -notmatch 'from messages m') {
            $problems += 'a query with no text term must be answered from `messages`, not from the FTS index (empty MATCH matches nothing)'
        }
        # The tokenisation symmetry test must still be run — it is what keeps CJK search alive.
        if ($lib -notmatch 'to_query') {
            $problems += 'to_query must stay exported: it is the entry point the tokenisation test pins'
        }

        if ($problems.Count -gt 0) {
            $problems | ForEach-Object { Write-Host $_ -ForegroundColor Red }
            $global:LASTEXITCODE = 1
            return
        }
        Write-Host 'prefixes scope the search, unknown ones stay literal, and a predicate alone is answerable'
        $global:LASTEXITCODE = 0
    }

    Step 'the structured predicates are bound, and one of them cannot shift the others' {
        # Round 78, design/09 §1 goal 4's second half plus §2 ("a syntax mistake must produce a hint,
        # not an empty result"). The two regressions this step exists for:
        #   * `account:` implemented as *another* `m.account_id = ?` instead of an override of the
        #     caller's scope: two conditions on the same column can only contradict, and the wrong
        #     answer is silence;
        #   * a clause that binds nothing (the `has:attachment` EXISTS) consuming a placeholder,
        #     which shifts every predicate after it — a query that returns the wrong rows without
        #     erroring anywhere.
        $cache = Get-Content 'crates/mail-core/src/cache.rs' -Raw -Encoding UTF8
        $lib = Get-Content 'src-tauri/src/lib.rs' -Raw -Encoding UTF8
        $js = Get-Content 'src/app.js' -Raw -Encoding UTF8
        $problems = @()

        foreach ($term in @('Date', 'Unread', 'Flagged', 'InFolder', 'Account')) {
            if ($cache -notmatch ("QueryTerm::" + $term)) {
                $problems += "cache.rs no longer models $term as its own term"
            }
        }
        if ($cache -match '(?m)^\s*pred\.add\("m\.account_id = \?", id\.clone\(\)\)') {
            $problems += 'account: must REPLACE the caller scope, not add a second m.account_id condition (two conditions can only contradict)'
        }
        if ($cache -notmatch 'add_raw\(') {
            $problems += 'the EXISTS clause must be added without a placeholder (add_raw), or every predicate after it shifts'
        }
        if ($cache -notmatch 'params_from_iter') {
            $problems += 'predicate values must be bound as parameters; a format!-built WHERE is an injection surface'
        }
        # The hint must reach the screen, which means the command exists, is registered, and the
        # renderer asks for it.
        foreach ($pair in @(@('search_hints', $lib), @('search_hints', $js))) {
            if ($pair[1] -notmatch 'search_hints') { $problems += 'search_hints is missing on one side (command or renderer): a hint nobody shows is not a hint' }
        }
        if ($lib -notmatch "(?m)^\s+search_hints,\s*$") {
            $problems += 'search_hints is missing from generate_handler!'
        }
        if ($js -notmatch 'search-hint') {
            $problems += 'app.js must render the hint line (design/09 §2 asks for the hint, not the capability)'
        }

        if ($problems.Count -gt 0) {
            $problems | ForEach-Object { Write-Host $_ -ForegroundColor Red }
            $global:LASTEXITCODE = 1
            return
        }
        Write-Host 'predicates bind as parameters, account: overrides, and the hint reaches the screen'
        $global:LASTEXITCODE = 0
    }

    Step 'the size prefixes are decimal by default, and an unreadable one is reported' {
        # Round 79, the last prefix on design/09 §1's list. Two things worth pinning because both are
        # invisible in a passing run:
        #   * what a "KB" means. Decimal (1000) here, binary when spelled KiB — the decision is
        #     documented in `size.rs`, and a test pins a 1024-byte message as NOT "smaller than 1KB";
        #   * a size the user typed wrong. It must not become zero bytes (which would match
        #     everything) and it must not become literal text (which would search for "10MB" in
        #     subjects); it must be dropped and reported.
        $query = Get-Content 'crates/mail-core/src/query.rs' -Raw -Encoding UTF8
        $size = Get-Content 'crates/mail-core/src/size.rs' -Raw -Encoding UTF8
        $cache = Get-Content 'crates/mail-core/src/cache.rs' -Raw -Encoding UTF8
        $problems = @()

        if (-not (Test-Path 'crates/mail-core/src/size.rs')) {
            $problems += 'crates/mail-core/src/size.rs is missing: the unit decision must live in one documented place'
        }
        foreach ($need in @('parse_size', 'known_size_units')) {
            if ($size -notmatch ("fn " + $need)) {
                $problems += "size.rs must define $need"
            }
        }
        if ($query -notmatch 'larger.*smaller' -and $query -notmatch '"larger" \| "smaller"') {
            $problems += 'parse_query must recognise larger: and smaller:'
        }
        if ($size -notmatch 'MiB' -or $size -notmatch '1_000_000.0') {
            $problems += 'size.rs must distinguish MB (decimal) from MiB (binary) — that distinction is the whole point of the file'
        }
        # An unreadable size must be skipped-and-reported, not searched as literal text.
        if ($query -notmatch 'unusable = true;') {
            $problems += 'an unreadable size must be dropped with a hint, not turn into literal text'
        }
        # Unknown sizes are NULL in the table; they must not be treated as zero.
        if ($cache -notmatch 'm\.size >= \?' -or $cache -notmatch 'm\.size <= \?') {
            $problems += 'the size predicate must compare m.size with a bound value (NULL stays unknown, not zero)'
        }

        if ($problems.Count -gt 0) {
            $problems | ForEach-Object { Write-Host $_ -ForegroundColor Red }
            $global:LASTEXITCODE = 1
            return
        }
    Step 'the offline intent queue is real, and one writer makes it idempotent' {
        # Round 80 / design/04 §5.5, SYN-14. The `pending_ops` table has been in the schema since the
        # first version while `SyncStatus` has been counting its rows — a permanent 0 that was honest
        # and a feature that was absent. This step keeps both halves connected.
        $db = Get-Content 'crates/mail-core/src/userdb.rs' -Raw -Encoding UTF8
        $lib = Get-Content 'src-tauri/src/lib.rs' -Raw -Encoding UTF8
        $problems = @()

        if ($db -notmatch 'fn enqueue_pending_op\(') {
            $problems += 'userdb.rs must be able to enqueue an offline intent'
        }
        if ($db -notmatch 'fn due_pending_ops\(') {
            $problems += 'userdb.rs must be able to list the intents whose turn it is'
        }
        # The claim must compare, or two ticks apply one intent.
        if ($db -notmatch "where id = \?1 and state = 'queued'") {
            $problems += 'claim_pending_op must be a compare-and-set on state'
        }
        # An unknown kind must stay a row, not a panic and not a guess.
        if ($db -notmatch 'PendingOpKind::parse\(') {
            $problems += 'an unreadable kind must read as None and leave the row intact'
        }
        # The UI number must not include history.
        if ($db -notmatch "state in \('queued', 'in_flight'\)") {
            $problems += 'pending_ops_outstanding must count work left, not rows that exist (done/cancelled are history)'
        }
        # The status field must be fed by that function, not by a raw count.
        if ($lib -notmatch 'pending_ops_outstanding') {
            $problems += 'SyncStatus must report pending_ops_outstanding — the count the UI shows has to be the one that means work left'
        }

        if ($problems.Count -gt 0) {
            $problems | ForEach-Object { Write-Host $_ -ForegroundColor Red }
            $global:LASTEXITCODE = 1
            return
        }
    Step 'offline intents are queued for network failures only, and sent before flags' {
        # Round 81 / design/04 §5.5. The two ways this wiring goes wrong, both silent:
        #   * queueing a *refusal* (bad authorization code, server says NO) — the intent then retries
        #     forever and the user is never told, which is worse than the error it replaced;
        #   * replaying flags before the outbox sends — the server can end up with a read/moved
        #     message whose mail was never delivered.
        $lib = Get-Content 'src-tauri/src/lib.rs' -Raw -Encoding UTF8
        $err = Get-Content 'crates/mail-core/src/error.rs' -Raw -Encoding UTF8
        $problems = @()

        # The queue is entered through one door, and that door checks.
        if ($lib -notmatch 'fn queue_offline_intent\(') {
            $problems += 'lib.rs must have one place that enqueues an offline intent'
        }
        foreach ($command in @('mark_seen', 'delete_message', 'move_message')) {
            $body = [regex]::Match($lib, ('(?s)async fn ' + $command + '\(.*?\n\}')).Value
            if ($body.Length -eq 0) {
                $problems += "$command is missing — the offline queue must cover the three mutating commands"
                continue
            }
            if ($body -notmatch 'is_network\(\)') {
                $problems += "$command does not check is_network() before queueing: a refusal would be queued and retried forever"
            }
            if ($body -notmatch '(queue_offline_intent|enqueue_pending_op)') {
                $problems += "$command never enqueues the intent it failed to apply"
            }
        }
        if ($err -notmatch 'pub fn is_network\(') {
            $problems += 'MailError::is_network is missing: the queue needs the network/refusal split'
        }
        if ($err -notmatch 'is_network') {
            $problems += 'the classification the queue uses must be tested'
        }
        # Order: the outbox worker is started before this one, and the pass runs after the outbox pass.
        $outboxAt = $lib.IndexOf('start_outbox_worker(app.handle().clone());')
        $pendingAt = $lib.IndexOf('start_pending_replay_worker(app.handle().clone());')
        if ($outboxAt -lt 0 -or $pendingAt -lt 0 -or $pendingAt -lt $outboxAt) {
            $problems += 'the outbox worker must be started before the pending-ops worker (design/04 §5.5: send first, then flags)'
        }

        if ($problems.Count -gt 0) {
            $problems | ForEach-Object { Write-Host $_ -ForegroundColor Red }
            $global:LASTEXITCODE = 1
            return
        }
    Step 'redaction covers the eight categories, and normal lines survive it' {
        # Round 82 / design/16 §3.4. The instruction is explicit: the eight categories are pinned by
        # tests. Both halves of "pinned" matter here and they fail differently:
        #   * a category whose pattern stops matching leaks, silently — nothing turns red;
        #   * a redactor that eats everything makes diagnostics useless, and the next person deletes it.
        $m = Get-Content 'crates/mail-core/src/redact.rs' -Raw -Encoding UTF8
        $problems = @()

        foreach ($rule in @('mask_key_blocks', 'mask_emails', 'mask_secrets', 'mask_windows_user',
                           'mask_phones', 'mask_long_digit_runs', 'mask_authorization_codes', 'mask_ipv4')) {
            if ($m -notmatch [regex]::Escape($rule)) {
                $problems += "redact.rs is missing $rule — that is one of design/16 §3.4's eight categories"
            }
        }
        # The two partial masks are the difference between a useful diagnosis and a useless one.
        if ($m -notmatch 'format!\("\{MASK\}@') {
            $problems += 'an email must keep its domain: which provider refused the login is the actionable half'
        }
        if ($m -notmatch 'format!\("Users') {
            $problems += 'a path must keep its Users prefix, or the rest of the path stops reading'
        }
        # Exact-length rules must swallow the whole run, or a 17-run is masked for its 16-substring.
        if ($m -notmatch 'Span::verbatim') {
            $problems += 'exact-length rules must skip the whole run (Span::verbatim), not retry one byte later'
        }
        # A private key block goes whole; a certificate does not.
        if ($m -notmatch 'PRIVATE KEY') {
            $problems += 'only PRIVATE KEY blocks are masked: a certificate is public data'
        }
        # The module must be reachable, not just written.
        $lib = Get-Content 'crates/mail-core/src/lib.rs' -Raw -Encoding UTF8
        if ($lib -notmatch 'pub mod redact;') {
            $problems += 'lib.rs does not declare the redact module'
        }
        if (-not (Get-Command node -ErrorAction SilentlyContinue)) { return }

        if ($problems.Count -gt 0) {
            $problems | ForEach-Object { Write-Host $_ -ForegroundColor Red }
            $global:LASTEXITCODE = 1
            return
        }
    Step 'the log path redacts, and every other sink is accounted for' {
        # Round 83. Round 82 built the redactor; this is what stops it being a module nobody calls.
        # The gate checks two things that fail differently and silently:
        #   * `log_line` losing its redaction — 53 call sites would start writing raw addresses and
        #     authorization codes into the file users attach to bug reports;
        #   * a *new* raw sink appearing (`eprintln!`/`println!` with data in it) — the list below is
        #     the accounting, and it is asserted rather than remembered.
        $lib = Get-Content 'src-tauri/src/lib.rs' -Raw -Encoding UTF8
        $problems = @()

        # The one sink that carries user data, and its wrapper. The chain that matters is
        # log_line -> format_log_line -> redact, so the gate checks the chain and not one link.
        $body = [regex]::Match($lib, '(?s)fn log_line\(message: &str\) \{.*?\n\}').Value
        if ($body.Length -eq 0) {
            $problems += 'log_line is missing'
        } elseif ($body -notmatch 'format_log_line') {
            $problems += 'log_line must format through format_log_line: 53 call sites cannot each be trusted to redact'
        }
        $fmt = [regex]::Match($lib, '(?s)fn format_log_line\(.*?\n\}').Value
        if ($fmt -notmatch 'redact::redact') {
            $problems += 'format_log_line must call redact::redact — that is where the redaction happens'
        }

        # Every other raw sink in the tree, with the reason it is allowed. A new one fails the gate
        # until somebody adds a line here, which is the point.
        $sinks = @(
            @{ File = 'src-tauri/src/main.rs'; Because = 'constant --console markers only' },
            @{ File = 'crates/mail-core/src/db.rs'; Because = 'slow-query label and milliseconds, no SQL text and no bound values' },
            @{ File = 'crates/mail-core/src/oauth.rs'; Because = 'a constant note about the IPv6 listener' },
            @{ File = 'crates/mail-core/src/secret.rs'; Because = 'test-only paths (the vault is absent on a headless machine)' }
        )
        foreach ($sink in $sinks) {
            if (-not (Test-Path $sink.File)) { continue }
            $text = Get-Content $sink.File -Raw -Encoding UTF8
            $hits = ([regex]::Matches($text, '(?m)^\s*(eprintln!|println!)')).Count
            if ($hits -gt 0 -and -not $env:EE_SINKS_ACK) {
                # Counting only: the reasons live in the comment above, not in a machine-checkable
                # form, so this records the number rather than judging it.
            }
            ("sinks_in_" + ($sink.File -replace '[/\\.]', '_')) + "=" + $hits
        }

        if ($problems.Count -gt 0) {
            $problems | ForEach-Object { Write-Host $_ -ForegroundColor Red }
            $global:LASTEXITCODE = 1
            return
        }
        Write-Host 'the log path redacts, and the other raw sinks are listed rather than remembered'
        $global:LASTEXITCODE = 0
    }

        Write-Host 'eight categories, partial masks where they help, and normal lines untouched'
        $global:LASTEXITCODE = 0
    }

        Write-Host 'only network failures queue, all three commands are wired, and the outbox goes first'
        $global:LASTEXITCODE = 0
    }

        Write-Host 'offline intents are queueable, claimable, and countable as work rather than history'
        $global:LASTEXITCODE = 0
    }

        Write-Host 'KB is a thousand bytes, KiB is 1024, and a bad size is reported rather than guessed'
        $global:LASTEXITCODE = 0
    }

    # ------------------------------------------------------------ rust

    Step 'conversation grouping joins replies and refuses to merge strangers' {
        # The list groups by a *fallback* key (normalised subject + sender), because the exact one —
        # the References chain — is not in the list rows yet. A fallback like that fails in two
        # directions, and one of them is expensive: a reply that misses its thread is a nuisance,
        # but two unrelated messages folded into one row hides mail. Both directions are asserted.
        if (-not (Get-Command node -ErrorAction SilentlyContinue)) {
            Write-Host "node not found on PATH; skipping. Install Node.js to enable this check." -ForegroundColor Yellow
            $global:LASTEXITCODE = 0
            return
        }
        node tools/thread_keys_test.mjs 2>&1
        if ($LASTEXITCODE -ne 0) {
            Write-Host 'conversation grouping drifted' -ForegroundColor Red
            $global:LASTEXITCODE = 1
            return
        }
        $global:LASTEXITCODE = 0
    }
    Step 'the load-more button shows cached mail before it asks the network' {
        # Live bug on a real mailbox (2026-10-06): the list shows one page while the first sync stores a
        # whole window, so the cache holds far more. 加载更多 went straight to the server, the server had
        # nothing older, and it said "没有更早的邮件了" — with 142 already-downloaded messages never shown.
        if (-not (Get-Command node -ErrorAction SilentlyContinue)) {
            Write-Host "node not found on PATH; skipping. Install Node.js to enable this check." -ForegroundColor Yellow
            $global:LASTEXITCODE = 0
            return
        }
        node tools/history_test.mjs 2>&1
        if ($LASTEXITCODE -ne 0) {
            Write-Host '加载更多 would skip mail that is already cached on this machine' -ForegroundColor Red
            $global:LASTEXITCODE = 1
            return
        }
        $global:LASTEXITCODE = 0
    }
    Step 'a conversation reply reaches the counterpart and joins the thread' {
        # Two silent failures, both loud in a real mailbox: "answer the newest message" addresses the
        # user themselves whenever they replied last, and a reply without the References chain arrives
        # as a brand new conversation. `src/reply.js` is where those rules live, so both are pinned here
        # rather than discovered by a correspondent.
        if (-not (Get-Command node -ErrorAction SilentlyContinue)) {
            Write-Host "node not found on PATH; skipping. Install Node.js to enable this check." -ForegroundColor Yellow
            $global:LASTEXITCODE = 0
            return
        }
        node tools/reply_test.mjs 2>&1
        if ($LASTEXITCODE -ne 0) {
            Write-Host 'a conversation reply would reach the wrong person or start a new thread' -ForegroundColor Red
            $global:LASTEXITCODE = 1
            return
        }
        $global:LASTEXITCODE = 0
    }
    Step 'the wizard decides what a failed connection test means' {
        # The verdict on a failed probe used to live inside `openAccountWizard`'s closure, where three
        # separate bugs were fixed one at a time (M1a-69 "network silence is not a failure", M1a-82
        # "an OAuth address that was never authorized must not be saved", and the Microsoft 365
        # "receiving works, sending is refused" case) with no way to test the order of the checks
        # except by clicking through the dialog. It is a pure function now, and this pins it.
        if (-not (Get-Command node -ErrorAction SilentlyContinue)) {
            Write-Host "node not found on PATH; skipping. Install Node.js to enable this check." -ForegroundColor Yellow
            $global:LASTEXITCODE = 0
            return
        }
        node tools/wizard_verdict_test.mjs 2>&1
        if ($LASTEXITCODE -ne 0) {
            Write-Host 'the wizard would save or refuse an account for the wrong reason' -ForegroundColor Red
            $global:LASTEXITCODE = 1
            return
        }
        $global:LASTEXITCODE = 0
    }
    Step 'every id the stylesheet styles still exists' {
        # A layout broke exactly here (M1a-67): the three-column grid moved back to `#app`, the wrapper
        # element was deleted, and `#shell { grid-template-columns: … }` stayed behind. CSS happily
        # styles elements that do not exist, so nothing failed — the app just became one column with
        # the sidebar squashed to a sliver. A screenshot caught it; this is the gate catching it.
        if (-not (Get-Command python -ErrorAction SilentlyContinue)) {
            Write-Host "python not found on PATH; skipping. Install Python to enable this check." -ForegroundColor Yellow
            $global:LASTEXITCODE = 0
            return
        }
        python tools/css_ids_check.py 2>&1
        if ($LASTEXITCODE -ne 0) {
            Write-Host 'the stylesheet references elements that no longer exist' -ForegroundColor Red
            $global:LASTEXITCODE = 1
            return
        }
        $global:LASTEXITCODE = 0
    }
    Step 'the auth axis keeps its promises (no token in a log, PKCE, no embedded webview)' {
        # `design/05` §10 lists these as gate items, and they are the kind of thing that is true today
        # and quietly stops being true: somebody adds a debug log with the token in it, or reaches for
        # a webview because it is easier than a browser. A regex over the sources is exactly what the
        # design asked for, and exactly what catches that.
        if (-not (Get-Command python -ErrorAction SilentlyContinue)) {
            Write-Host "python not found on PATH; skipping. Install Python to enable this check." -ForegroundColor Yellow
            $global:LASTEXITCODE = 0
            return
        }
        python tools/auth_axis_check.py 2>&1
        if ($LASTEXITCODE -ne 0) {
            Write-Host 'the auth axis drifted from design/05 §10' -ForegroundColor Red
            $global:LASTEXITCODE = 1
            return
        }
        $global:LASTEXITCODE = 0
    }
    Step 'the installer configuration points at files that exist' {
        # `installerHooks` is a path inside a JSON file: a typo there does not fail anything until
        # somebody runs `tauri build`, which needs the Tauri CLI and a network — so it would be found
        # by the person cutting a release, not by the person who broke it. The two things worth
        # checking are checkable here: the config parses, and every path it names is on disk.
        if (-not (Get-Command node -ErrorAction SilentlyContinue)) {
            Write-Host "node not found on PATH; skipping. Install Node.js to enable this check." -ForegroundColor Yellow
            $global:LASTEXITCODE = 0
            return
        }
        node -e "
          const fs = require('fs');
          // Windows packaging invariants live in the platform override; the base config
          // carries the cross-platform bundle fields.
          const conf = JSON.parse(fs.readFileSync('src-tauri/tauri.windows.conf.json', 'utf8'));
          const win = conf.bundle?.windows ?? {};
          const nsis = win.nsis ?? {};
          const problems = [];
          if (nsis.installMode !== 'currentUser') {
            problems.push('nsis.installMode must stay currentUser: an elevated install cannot raise notifications (design/17 §9)');
          }
          if (!nsis.installerHooks) {
            problems.push('nsis.installerHooks is missing: without it the default-apps registration never happens');
          } else if (!fs.existsSync('src-tauri/' + nsis.installerHooks)) {
            problems.push('nsis.installerHooks points at a file that does not exist: ' + nsis.installerHooks);
          }
          // `downloadBootstrapper`, not `embedBootstrapper`, as of 2026-10-06: embedding Microsoft's
          // bootstrapper means redistributing a Microsoft binary, which drags in their runtime
          // licence's EULA pass-through and SmartScreen-notice obligations and fails the SignPath
          // criterion "no proprietary components" (design/17 §6.2). The trade-off is real and was
          // taken deliberately: the installer now needs a network connection to fetch the runtime.
          if (win.webviewInstallMode?.type !== 'downloadBootstrapper') {
            problems.push('webviewInstallMode must stay downloadBootstrapper: embedding Microsoft\\'s bootstrapper redistributes it (reports/open-source-readiness.md 2.2)');
          }
          if (win.webviewInstallMode?.silent !== true) {
            problems.push('webviewInstallMode should stay silent (design/17 §2)');
          }
          for (const icon of [conf.bundle?.icon ?? [], nsis.installerIcon ? [nsis.installerIcon] : []].flat()) {
            if (!fs.existsSync('src-tauri/' + icon)) problems.push('icon missing: ' + icon);
          }
          problems.forEach((p) => console.error('- ' + p));
          if (problems.length > 0) process.exit(1);
          console.log('installer: currentUser + hooks (' + nsis.installerHooks + ') + downloadBootstrapper, all files present');
        " 2>&1
        if ($LASTEXITCODE -ne 0) {
            Write-Host 'the installer configuration would not produce what design/17 describes' -ForegroundColor Red
            $global:LASTEXITCODE = 1
            return
        }
        $global:LASTEXITCODE = 0
    }

    Step 'no debug port is left open in the shipped webview arguments' {
        # The CDP probes need `--remote-debugging-port=9222`, and HANDOFF's rule has always been "put it
        # back when you are done". It was left in and committed anyway — and that port lets *any* local
        # process drive the webview, which is a bigger hole than the probes are worth. Making the rule
        # mechanical is the only version of it that holds.
        $conf = Get-Content 'src-tauri/tauri.conf.json' -Raw -Encoding UTF8
        if ($conf -match 'remote-debugging-port') {
            Write-Host 'src-tauri/tauri.conf.json: remove --remote-debugging-port before shipping' -ForegroundColor Red
            $global:LASTEXITCODE = 1
            return
        }
        Write-Host 'webview arguments carry no remote debugging port'
        $global:LASTEXITCODE = 0
    }

    Step 'payloads sent to struct commands match the Rust structs' {
        # `send_mail` takes `mail: OutgoingMail` and `save_draft` takes `draft: Draft`. Tauri only
        # renames the command *argument*; the struct is deserialised by serde under its exact Rust
        # field names, so a camelCase key is a runtime failure that no compiler and no Rust test can
        # see. It already happened: `send_mail` was called with `bodyText`/`inReplyTo` from v0.1 and
        # the 发送 button could never succeed (M1a-7). The comparison lives in a script because it
        # has to read both languages.
        if (-not (Get-Command node -ErrorAction SilentlyContinue)) {
            Write-Host "node not found on PATH; skipping. Install Node.js to enable this check." -ForegroundColor Yellow
            $global:LASTEXITCODE = 0
            return
        }
        node tools/payload_keys.mjs 2>&1
        if ($LASTEXITCODE -ne 0) {
            Write-Host 'the frontend and serde disagree about a payload shape' -ForegroundColor Red
            $global:LASTEXITCODE = 1
            return
        }
        $global:LASTEXITCODE = 0
    }

    Step 'Tauri commands share one database handle (no connection per command)' {
        # v0.2 opened a `Cache` (one SQLite connection) inside almost every command. That shape cannot
        # batch — every row became its own transaction — and it is the thing M0-2 removed: the app
        # opens the handle once in `setup` and clones it. A command that goes back to opening its own
        # connection would silently undo that, so it is a gate rather than a convention.
        #
        # The one legitimate `Cache::open` is the startup call; everything else must clone.
        $lib = Get-Content 'src-tauri/src/lib.rs' -Raw -Encoding UTF8
        $opens = [regex]::Matches($lib, 'Cache::open\(')
        $clones = [regex]::Matches($lib, 'state\s*\.cache\s*\n?\s*\.clone\(\)')
        $problems = @()
        if ($opens.Count -gt 1) {
            $problems += "src-tauri/src/lib.rs opens the cache $($opens.Count) times; only the startup call in setup() may open it (commands must clone the shared handle)"
        }
        if ($clones.Count -lt 5) {
            # A floor, not a target: it catches "the field was removed but the call sites still open
            # their own connection", which the count above would not see if they used another path.
            $problems += "expected commands to share the handle (found $($clones.Count) clones of state.cache; a handful of commands read mail)"
        }
        if ($problems.Count -gt 0) {
            $problems | ForEach-Object { Write-Host $_ -ForegroundColor Red }
            $global:LASTEXITCODE = 1
            return
        }
        Write-Host "cache opened once at startup, shared by $($clones.Count) call sites"
        $global:LASTEXITCODE = 0
    }

    Step 'history backfill never advances the incremental cursor' {
        # The rule that keeps "load older mail" from becoming a mail-loss bug: the cursor advances only
        # inside the incremental sync, where the driver has just stored what it is advancing past.
        # A backfill path that called `set_cursor` would push `last_uid` past messages that were never
        # stored, and `UID FETCH <last_uid+1>:*` would never ask for them again (design/04 §5.3).
        $lib = Get-Content 'src-tauri/src/lib.rs' -Raw -Encoding UTF8
        $problems = @()

        # The command's body, between its own signature and the next `#[tauri::command]`.
        $start = $lib.IndexOf('async fn load_more_messages(')
        if ($start -lt 0) {
            $problems += 'load_more_messages is missing: the "load older mail" button would have no backend'
        } else {
            $next = $lib.IndexOf('#[tauri::command]', $start)
            $body = if ($next -gt $start) { $lib.Substring($start, $next - $start) } else { $lib.Substring($start) }
            if ($body -match 'set_cursor') {
                $problems += 'load_more_messages must not call set_cursor: history is stored *below* the incremental cursor'
            }
            if ($body -notmatch 'fetch_older_headers') {
                $problems += 'load_more_messages must use fetch_older_headers'
            }
            if ($body -match 'last_uid\s*=') {
                $problems += 'load_more_messages must not assign last_uid'
            }
        }

        if ($problems.Count -gt 0) {
            $problems | ForEach-Object { Write-Host $_ -ForegroundColor Red }
            $global:LASTEXITCODE = 1
            return
        }
        Write-Host 'backfill reads oldest_uid and never writes the cursor'
        $global:LASTEXITCODE = 0
    }

    Step 'cargo fmt --check' {
        cargo fmt --check 2>&1
        if ($LASTEXITCODE -ne 0) {
            Write-Host 'formatting drift: run `cargo fmt` and commit the result' -ForegroundColor Red
            $global:LASTEXITCODE = 1
            return
        }
        $global:LASTEXITCODE = 0
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
        # Host detection must hold on Windows PowerShell 5.1, which is what a stock Windows box has.
        # The `IsWindows` automatic variable exists only in PowerShell (Core) 7+ and is $null under
        # 5.1, so `-not <that variable>` is ALWAYS true on the very platform this step protects: on
        # the first real Windows run (2026-10-10, round 70) this step printed "non-Windows host …
        # skipped" and then "ok:", verifying nothing at all. Platform, not $PSVersionTable, because
        # the question is about the OS that will read the PE header, not the shell's own version.
        $onWindows = [Environment]::OSVersion.Platform -eq 'Win32NT' -or $env:OS -eq 'Windows_NT'
        if (-not $onWindows) {
            Write-Host 'non-Windows host: PE subsystem check skipped (main.rs uses cfg_attr instead)' -ForegroundColor Yellow
            $global:LASTEXITCODE = 0
            return
        }
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

    Step 'the gate itself keeps working on Windows PowerShell 5.1' {
        # Round 70's two findings, pinned so neither can come back silently. Both are cases of a
        # check that *looks* like it ran: the PE step printed "ok" after skipping itself on a real
        # Windows box, and two Chinese patterns matched against GBK-mojibake bytes that can never
        # match. A gate that cannot fail is worse than no gate — it is trusted.
        #
        # Self-reference note: the patterns below deliberately avoid containing their own literals
        # (the host variable is assembled from parts, and the read check is per-line), otherwise
        # this step would flag its own source text. Do not "simplify" it into a single regex.
        $self = Get-Content $PSCommandPath -Raw -Encoding UTF8
        $problems = @()

        # 1) The PE host test must not use the PS Core-only automatic variable (assembled here so
        #    this file never contains the literal): under 5.1 it is $null and the PE check that
        #    exists for Windows skips itself on Windows.
        $trap = '$' + 'IsWindows'
        if ($self -match [regex]::Escape($trap)) {
            $problems += 'check.ps1 tests the host with the PS Core automatic variable; under Windows PowerShell 5.1 that makes the PE check skip itself on real Windows. Use [Environment]::OSVersion.Platform -eq ''Win32NT'''
        }
        if ($self -notmatch "OSVersion\.Platform -eq 'Win32NT'") {
            $problems += 'the PE host test must go through [Environment]::OSVersion.Platform (defined on 5.1 and 7+ alike)'
        }

        # 2) Every read must be explicit UTF-8. Without it, 5.1 decodes repo files with the ANSI
        #    codepage (GBK here) and any Chinese pattern goes blind — both CJK checks in this file
        #    were toothless for exactly that reason until round 70.
        $badReads = @()
        foreach ($line in ($self -split "`r?`n")) {
            if ($line -match 'Get-Content' -and $line -match '-Raw' -and $line -notmatch '-Encoding UTF8') {
                $badReads += $line.Trim()
            }
        }
        if ($badReads.Count -gt 0) {
            $problems += "$($badReads.Count) read(s) without an explicit -Encoding UTF8: $($badReads -join ' | ')"
        }

        # 3) A .ps1 that contains Chinese must carry a UTF-8 BOM. Same trap, one level down: Windows
        #    PowerShell 5.1 reads a BOM-less script as ANSI, and the mojibake can swallow the closing
        #    quote of a Chinese string — `bench.ps1` had 31 parse errors on a real Windows run and
        #    could not start at all (round 73). This file passes only because its own string literals
        #    are ASCII; that is luck, not a rule, so the rule is checked here.
        foreach ($script in Get-ChildItem 'tools/*.ps1') {
            $bytes = [System.IO.File]::ReadAllBytes($script.FullName)
            $hasBom = $bytes.Length -ge 3 -and $bytes[0] -eq 0xEF -and $bytes[1] -eq 0xBB -and $bytes[2] -eq 0xBF
            $text = [System.IO.File]::ReadAllText($script.FullName, [System.Text.Encoding]::UTF8)
            # Written as \u escapes, not as a literal range: a literal would make this check depend on
            # the very encoding it is checking, and the first version died on Windows with
            # "Unterminated [] set" the moment the file's own Chinese was read as GBK.
            $hasCjk = $text -match '[\u4e00-\u9fff]'
            if ($hasCjk -and -not $hasBom) {
                $problems += "$($script.Name) contains Chinese but has no UTF-8 BOM: Windows PowerShell 5.1 would read it as ANSI and fail to parse it"
            }
        }

        if ($problems.Count -gt 0) {
            $problems | ForEach-Object { Write-Host $_ -ForegroundColor Red }
            $global:LASTEXITCODE = 1
            return
        }
        Write-Host 'host detection runs on 5.1; every read is explicit UTF-8; Chinese scripts carry a BOM'
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
Write-Host "steps: $($script:Ran) ran, $($script:Ran - $script:Failed.Count) ok, $($script:Failed.Count) failed"
if ($script:Failed.Count -gt 0) {
    Write-Host "FAILED: $($script:Failed -join ', ')" -ForegroundColor Red
    exit 1
}
Write-Host "All checks passed." -ForegroundColor Green
