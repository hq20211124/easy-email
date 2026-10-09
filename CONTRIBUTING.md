# Contributing

Thanks for looking at Easy Email. This is a Tauri v2 + Rust desktop mail client for
Chinese-market providers, built on one rule: **the user types an address and an
authorization code, and nothing else.**

## Before You Start

- **Rust 1.88+** (edition 2024) plus the platform toolchain: Windows needs VS 2022 Build
  Tools and the WebView2 Runtime; Linux needs `build-essential`, `pkg-config`,
  `libwebkit2gtk-4.1-dev`, `libsoup-3.0-dev`, `libgtk-3-dev`, `librsvg2-dev`,
  `libayatana-appindicator3-dev`, `libdbus-1-dev`. There is no Node build step — the
  frontend is static HTML/JS under `src/`.
- Read `README.md` (why things are shaped this way) and, if you change behaviour that a test
  pins, the note attached to that test. Most safeguards in this repo exist because a real bug
  got past a green build, and the test says so.

## The One Rule for a Change

**If you changed behaviour and there is no test that would have caught the bug you fixed,
add one.** A fix without a failing-then-passing test will be asked for one.

```bash
cargo test --workspace          # Rust: protocol layer + integration
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --check
```

On Windows there is also the frontend/contract gate, which is what catches a broken renderer
that `cargo build` cannot see:

```powershell
.\tools\check.ps1              # ~30 seconds, offline
.\tools\check.ps1 -Network      # also probes every built-in provider against the real servers
```

Note that the non-obvious invariants are pinned in `tools/check.ps1`: an element id a
stylesheet styles must exist, a command the UI calls must exist in Rust, the sanitiser must be
applied on both fetch and cache, the CSP intersection rule, and the installer configuration.
Please do not weaken a gate to make a change pass.

## Pull Requests

- Keep the change focused. One behaviour per PR, and say in the description *what it fixes for
  a user*, not what it changes in the code.
- Do not add dependencies casually. Every new crate needs a licence check — the project keeps
  a complete inventory in `THIRD-PARTY-NOTICES.md`, and adding a GPL/AGPL dep will be
  refused. A new dependency with a permissive licence needs a line in that file.
- Do not add a built-in OAuth client registration. Providers that need one ask the user for
  their own; that is deliberate (see the comment above `NO_BUILT_IN_REGISTRATION` in
  `crates/mail-core/src/oauth.rs`).
- Do not commit real mailbox data: no addresses, no screenshots of an inbox, no captured
  traffic. Test fixtures should use `example.com`.
- Platform rule: Windows behaviour must not regress when adding Linux support, and vice
  versa. Platform-specific code goes behind `cfg`, with the reason in a comment.

## Reporting Bugs

Open an issue with the provider (or "custom"), the platform, and the server's own reply text
rather than a screenshot of the error dialog where you can — the wording of a server's refusal
is usually the whole diagnosis.
