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
  Manager; OAuth refresh tokens *and* OAuth client secrets live in DPAPI-encrypted files
  (user scope, per-account entropy). Linux: passwords go to the D-Bus Secret Service; those
  secrets are files encrypted with a key derived from the machine id, mode `0600` in a `0700`
  directory. Nothing secret is a row in `user.db` — a gate in `tools/auth_axis_check.py` fails
  the build if code writes an OAuth client secret into the database again.
- **Every TLS connection validates against one trust anchor: the OS certificate store.**
  IMAP/SMTP/POP3 go through Schannel, OAuth and other HTTPS through rustls configured with
  native roots, so an enterprise CA or a TLS-inspecting gateway certificate is trusted
  everywhere or nowhere — no path silently keeps a private, compiled-in root list.
- **Nothing secret is written to logs.** `startup-error.log` and the console diagnostics
  never carry tokens or passwords; a gate in `tools/check.ps1` fails the build if a token is
  ever logged.
- **Mail bodies are rendered twice-sanitised.** A sandboxed `iframe` (no scripts, no
  same-origin, no top navigation) *and* an independent CSP whose default is
  `img-src data: blob:` — remote images are blocked until the user explicitly loads them
  **for that one message**, which is what stops tracking pixels from reporting a read.
- **OAuth uses PKCE S256 with a state check, a system browser, and an exclusively-bound
  loopback listener.** No OOB, no embedded webview, no client secrets baked into any build.
- **Message bodies can be kept off the disk entirely.** *Settings → Storage → 正文不落盘* stops
  the cache from ever writing a body: the text is downloaded into memory when a message is
  opened and re-downloaded the next time. Turning it on can also clear the bodies already
  stored, and the clear rebuilds the file rather than only deleting rows, because a delete
  leaves the text on a freed page. The preference lives in `user.db`, not in the cache it
  controls — "delete the cache" is an action a user takes to stop bodies accumulating, and it
  must not switch the rule off — and an *unreadable* preference fails **closed**: no preference
  read is not permission to put secret text in a file. What the mode costs is stated on the
  same screen: unread mail is not available offline, and body-text search only covers what was
  stored before the clear.

## Known Limitations (stated, not hidden)

- The on-disk protection does **not** stop malware running as the same OS user — that is a
  property of every user-scope secret store, including DPAPI.
- The cache database is not encrypted: it holds message metadata and bodies of mail the user
  has already downloaded. It never holds passwords or refresh tokens. `design/A3-local-store-
  protection.md` explains why no mainstream desktop client treats local-store encryption as
  the security boundary, and what to do instead: the body-storage mode above, plus
  **full-volume encryption as a written deployment precondition** — the app checks it
  (read-only, one `Get-CimInstance Win32_EncryptableVolume` at startup, its answer shown in the
  storage panel) but does not enforce it, and a non-elevated check honestly reports 无法判定
  because Windows returns Access Denied. `tools/disk-protection-check.ps1` is the elevated,
  archivable form for the deployment record: exit 0 precondition holds, 1 does not, 2 unknown.
  The remaining protection is not encryption either: a saved draft is mirrored to the server's
  Drafts folder and a sent message gets its server copy, so losing the machine does not lose the
  mail.
- Outgoing mail is spooled as plaintext `.eml` files, and it stays there longer than "until it is
  sent". The message is built **once, at enqueue time**, and frozen as `<outbox id>.eml` under the
  app's config directory (default `%APPDATA%\com.easyemail.desktop\spool`; if that directory cannot
  be created the spool falls back to a `easy-email-spool` folder under the machine temp directory) —
  full headers and body, unencrypted. After SMTP accepts the message the file is **deliberately kept**
  until the Sent copy is filed from those exact bytes, and a failed send keeps it so a retry delivers
  the identical message. It is removed when the send is filed, when the user cancels, or when the
  outbox row is dropped. **Nothing ages it out**: `design/07` §5.5 once promised a 7-day auto-clean
  and that is not implemented, so the honest statement is that an unsent message sits in plaintext on
  disk until the user resolves it. Drafts and outbox rows are plaintext too, in `user.db`
  (`payload_json`), and received bodies are plaintext in `cache.db` unless 正文不落盘 is switched on.
  The server-side mirror (draft → Drafts, sent → server copy) is what protects the *mail*; it does
  not remove the local plaintext.
- **The memory ceiling per message is ours, and it is a refusal rather than a stream.** A fetched
  message above `MAX_FETCH_BYTES` (128 MB) fails with a readable per-message error instead of taking
  the process out; an inline image above 2 MB is left out of the HTML view and stays in the
  attachment list, where the user can fetch it deliberately. But the bytes arrive whole before that
  check runs — `async-imap` buffers a literal up to its own 512 MiB ceiling first — and a batched
  body prefetch holds every parsed detail of the batch in memory at once, its 8 MB budget planned
  from the cached `RFC822.SIZE` rather than from what the server actually returns. Sending has the
  same shape: each attachment is read whole into the MIME builder, measured peak **98.5 MB = 3.000×
  the encoded message** for a 24 MB attachment (`reports/performance.md`, budget P10 — no headroom).
  Only the SHA-256 integrity check streams (256 KB chunks); the send path does not.
- **Attachments the user saves are theirs to clean up.** Saving one refetches the whole message and
  writes the decoded part in a single `fs::write` into `<Downloads>\Easy Email\` (the name is
  de-duplicated, never silently overwritten), and the app never deletes or ages those files. Peak
  cost is proportional to the message, not to the attachment: an IMAP section fetch would bound it,
  but the MIME part number `mail-parser` reports cannot be derived into the protocol's dotted part
  path, so the cheap-looking version of that fix would risk putting the wrong bytes in the file.
- Not yet audited by a third party.
