# Security Policy

## Supported Versions

Only the latest release receives security fixes. Easy Email is pre-1.0 and moves quickly;
upgrade to the newest release rather than expecting backports.

| Version | Supported |
|---|---|
| latest release | ✅ |
| anything older | ❌ |

## Reporting a Vulnerability

Please **do not open a public issue** for anything that could expose a user's mail,
credentials, or tokens. Instead:

1. Use GitHub's private vulnerability reporting:
   **Security → Report a vulnerability** on this repository (GitHub Privately Reports a
   Vulnerability), or
2. Email the maintainer directly (address on the GitHub profile).

Include: what you found, the affected version/platform, reproduction steps, and the impact
you can demonstrate. You will get an acknowledgement, and a fix or a public advisory with
credit if you want it. Reports are handled privately until a fix is released.

## What Is Protected by Design

- **Credentials never touch disk in plaintext.** Windows: passwords live in the Credential
  Manager, refresh tokens in DPAPI-encrypted files (user scope, per-account entropy).
  Linux: passwords go to the D-Bus Secret Service; refresh tokens are files encrypted with a
  key derived from the machine id, mode `0600` in a `0700` directory.
- **Nothing secret is written to logs.** `startup-error.log` and the console diagnostics
  never carry tokens or passwords; a gate in `tools/check.ps1` fails the build if a token is
  ever logged.
- **Mail bodies are rendered twice-sanitised.** A sandboxed `iframe` (no scripts, no
  same-origin, no top navigation) *and* an independent CSP whose default is
  `img-src data: blob:` — remote images are blocked until the user explicitly loads them
  **for that one message**, which is what stops tracking pixels from reporting a read.
- **OAuth uses PKCE S256 with a state check, a system browser, and an exclusively-bound
  loopback listener.** No OOB, no embedded webview, no client secrets baked into any build.

## Known Limitations (stated, not hidden)

- The on-disk protection does **not** stop malware running as the same OS user — that is a
  property of every user-scope secret store, including DPAPI.
- The cache database is not encrypted: it holds message metadata and bodies of mail the user
  has already downloaded. It never holds passwords or refresh tokens.
- Attachments are fetched whole into memory before being written; a very large message costs
  memory proportional to that message.
- Not yet audited by a third party.
