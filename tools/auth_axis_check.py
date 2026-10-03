"""Do the auth-axis checks the design asks the gate to make? Run from check.ps1.

`design/05` §10 lists five additions to the gate for the accounts/auth work. Three of them are static
and belong here rather than in `cargo test`, because what they must catch is a *kind of code* rather
than a behaviour:

  2. **No token leakage**: no logging call may name `refresh_token`/`access_token`, and the token store
     must not put them in a file it also writes to disk unencrypted. A regex over the sources is what
     the design asks for, and a regex is what catches the mistake that matters (somebody adding
     `log_line(&format!("token: {refresh}"))` while debugging).
  3. **PKCE and the loopback**: the oauth module must actually contain both the challenge and the
     exclusive-address option.
  4. **No embedded WebView for OAuth**: the UI must never create one for the authorization step
     (A1-F32: Google answers `disallowed_useragent`).

Exit code 0 means all three hold.
"""

import os
import re
import sys

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
problems = []


def sources(roots, suffixes):
    for root in roots:
        base = os.path.join(ROOT, root)
        for dirpath, _dirnames, filenames in os.walk(base):
            if "target" in dirpath or "node_modules" in dirpath:
                continue
            for name in filenames:
                if name.endswith(suffixes):
                    yield os.path.join(dirpath, name)


def read(path):
    with open(path, encoding="utf-8", errors="replace") as handle:
        return handle.read()


# ---------------------------------------------------------------- 2. token leakage
LOG_CALL = re.compile(r"(log_line|println!|eprintln!|dbg!)\s*\(")
TOKEN_WORDS = ("refresh_token", "access_token", "refresh-token", "access-token")

for path in sources(["crates", "src-tauri"], (".rs",)):
    text = read(path)
    for index, line in enumerate(text.splitlines(), start=1):
        if not LOG_CALL.search(line):
            continue
        if any(word in line for word in TOKEN_WORDS):
            problems.append(
                f"{os.path.relpath(path, ROOT)}:{index} logs a token: {line.strip()}"
            )

# The renderer must not print one either.
for path in sources(["src", "tools"], (".js", ".mjs")):
    text = read(path)
    for index, line in enumerate(text.splitlines(), start=1):
        if not re.search(r"console\.(log|error|warn)|toast\(", line):
            continue
        if any(word in line for word in TOKEN_WORDS):
            problems.append(
                f"{os.path.relpath(path, ROOT)}:{index} shows or logs a token: {line.strip()}"
            )

# ---------------------------------------------------------------- 3. PKCE + loopback
oauth = read(os.path.join(ROOT, "crates", "mail-core", "src", "oauth.rs"))
for needle, why in [
    ("code_challenge", "PKCE is not being sent (design/05 §10.3)"),
    ("SO_EXCLUSIVEADDRUSE", "the loopback listener is not exclusive (A1-F36)"),
    ("code_verifier", "the verifier is never sent back to the token endpoint"),
]:
    if needle not in oauth:
        problems.append(f"oauth.rs no longer contains `{needle}`: {why}")

# ---------------------------------------------------------------- 1. the UI asks the table
# `design/05` §10.1: the "sign in with X" condition must come from the provider table, not from a list of
# ids in the renderer. A hardcoded id is how a service silently gets the wrong treatment the day it
# changes its mind — and how 163 would end up with an OAuth button.
app_js = read(os.path.join(ROOT, "src", "app.js"))
for index, line in enumerate(app_js.splitlines(), start=1):
    if "oauthAvailable" in line or "oauthEligible" in line:
        if re.search(r"provider\.id\s*===", line):
            problems.append(
                f"src/app.js:{index} decides OAuth eligibility from a provider id: {line.strip()}"
            )

# ---------------------------------------------------------------- 4. no embedded WebView
for path in list(sources(["src"], (".js",))) + list(sources(["src-tauri"], (".rs",))):
    text = read(path)
    for needle in ("new WebviewWindow", "WebviewWindowBuilder", "WebviewUrl::External"):
        if needle in text:
            problems.append(
                f"{os.path.relpath(path, ROOT)} creates a webview: "
                f"{needle} — OAuth must use the system browser (A1-F32)"
            )

if problems:
    for problem in problems:
        print(f"- {problem}")
    sys.exit(1)

print("auth axis: no token in any log, PKCE + exclusive loopback present, no embedded webview")
