//! Loopback integration test: drives the real client against a minimal in-process IMAP and
//! SMTP server.
//!
//! Why this exists: `provider_reachability` proves the *database* points at real servers, and
//! the unit tests prove the parsing helpers work in isolation. Neither executes the code that
//! actually talks IMAP — the greeting handshake, the pre-`LOGIN` `ID` command, `UID FETCH`
//! framing, expunge draining, SMTP auth and message framing. A fake server that speaks just
//! enough of both protocols closes that gap without needing a real mailbox.
//!
//! It deliberately exercises the awkward paths:
//!   * a `\Noselect` container folder that must be filtered out of the folder list
//!   * a GB2312-encoded message, which is the most common shape of real Chinese mail
//!   * a multipart message with an attachment
//!   * a server with and without UIDPLUS, to cover both expunge strategies
//!   * `needs_id_command`, asserting `ID` really is sent *before* `LOGIN`

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use mail_core::smtp;
use mail_core::{
    AccountConfig, Cache, ConnectionPool, Draft, DraftMirror, Endpoint, ImapClient, MailboxInfo,
    MessageSummary, NewOutbox, OutboxRun, OutgoingAttachment, OutgoingMail, PushEvent, RecoveryRun,
    Security, Spool, SyncDriver, SyncOutcome, UserStore, ensure_sent_copy, mirror_draft, pool_op,
    recover_in_flight, run_due, watch_every,
};
use mail_core::{BackfillBudget, BackfillStop, SyncWindow, backfill_older, fill_thread_keys};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};

/// Every client call is bounded, so a protocol deadlock fails the test instead of hanging CI.
const CALL_TIMEOUT: Duration = Duration::from_secs(10);

async fn within<T>(future: impl std::future::Future<Output = T>) -> T {
    tokio::time::timeout(CALL_TIMEOUT, future)
        .await
        .expect("operation timed out — the client and the fake server disagree on the protocol")
}

// ===================================================================== fixtures

/// Plain UTF-8 message with a raw (non-encoded) Chinese subject.
fn message_one() -> Vec<u8> {
    concat!(
        "From: Alice <alice@example.com>\r\n",
        "To: me@example.com\r\n",
        "Subject: 简单邮件\r\n",
        "Date: Mon, 01 Jan 2024 08:00:00 +0800\r\n",
        "Message-ID: <one@example.com>\r\n",
        "Content-Type: text/plain; charset=utf-8\r\n",
        "\r\n",
        "Hello there.\r\n"
    )
    .as_bytes()
    .to_vec()
}

/// The GB2312 case. Bytes are spelled out rather than string-escaped so the fixture cannot
/// silently become UTF-8: `=?gb2312?B?1tDOxLLiytQ=?=` is 中文测试, and `xOO6ww==` is 你好.
fn message_two() -> Vec<u8> {
    let mut raw = Vec::new();
    raw.extend_from_slice(
        concat!(
            "From: =?gb2312?B?1cXOsA==?= <bob@163.com>\r\n",
            "To: me@example.com\r\n",
            "Subject: =?gb2312?B?1tDOxLLiytQ=?=\r\n",
            "Date: Tue, 02 Jan 2024 09:30:00 +0800\r\n",
            "Message-ID: <two@163.com>\r\n",
            "Content-Type: text/plain; charset=\"gb2312\"\r\n",
            "Content-Transfer-Encoding: base64\r\n",
            "\r\n",
            "xOO6ww==\r\n"
        )
        .as_bytes(),
    );
    raw
}

/// How large the second attachment of [`message_three`] is once decoded.
///
/// Comfortably more than one streaming slice, so the download path is exercised across several
/// partial fetches rather than only in the single-slice case.
const BIG_ATTACHMENT_BYTES: usize = 300 * 1024;

/// Multipart message carrying two attachments: one trivial, one big enough to need several slices.
fn message_three() -> Vec<u8> {
    // "QUJD" is base64 for "ABC", so repeating it needs no encoder here and the decoded content is
    // known exactly. Wrapped at 76 characters the way real MIME base64 is, which also means the
    // decoder has to ignore line breaks rather than choke on them.
    let encoded = "QUJD".repeat(BIG_ATTACHMENT_BYTES / 3);
    let mut wrapped = String::with_capacity(encoded.len() + encoded.len() / 76 * 2);
    for line in encoded.as_bytes().chunks(76) {
        wrapped.push_str(std::str::from_utf8(line).expect("ascii"));
        wrapped.push_str("\r\n");
    }

    let mut raw = concat!(
        "From: Carol <carol@example.com>\r\n",
        "To: me@example.com\r\n",
        "Subject: Report\r\n",
        "Date: Wed, 03 Jan 2024 10:00:00 +0000\r\n",
        "Message-ID: <three@example.com>\r\n",
        "MIME-Version: 1.0\r\n",
        "Content-Type: multipart/mixed; boundary=\"BOUND\"\r\n",
        "\r\n",
        "--BOUND\r\n",
        "Content-Type: text/plain; charset=utf-8\r\n",
        "\r\n",
        "See attached.\r\n",
        "--BOUND\r\n",
        "Content-Type: application/pdf; name=\"report.pdf\"\r\n",
        "Content-Disposition: attachment; filename=\"report.pdf\"\r\n",
        "Content-Transfer-Encoding: base64\r\n",
        "\r\n",
        "aGVsbG8=\r\n",
        "--BOUND\r\n",
        "Content-Type: application/octet-stream; name=\"big.bin\"\r\n",
        "Content-Disposition: attachment; filename=\"big.bin\"\r\n",
        "Content-Transfer-Encoding: base64\r\n",
        "\r\n"
    )
    .to_string();
    raw.push_str(&wrapped);
    raw.push_str("--BOUND--\r\n");
    raw.into_bytes()
}

/// HTML message whose body references an embedded image by `cid:`.
///
/// This is the shape that produced broken images in the UI: the bytes are in the message, but
/// nothing was resolving the reference.
fn message_four() -> Vec<u8> {
    concat!(
        "From: Dave <dave@example.com>\r\n",
        "To: me@example.com\r\n",
        "Subject: Newsletter\r\n",
        "Date: Thu, 04 Jan 2024 11:00:00 +0000\r\n",
        "Message-ID: <four@example.com>\r\n",
        "MIME-Version: 1.0\r\n",
        "Content-Type: multipart/related; boundary=\"REL\"\r\n",
        "\r\n",
        "--REL\r\n",
        "Content-Type: text/html; charset=utf-8\r\n",
        "\r\n",
        "<p>Logo:</p><img src=\"cid:logo@example.com\" alt=\"logo\">\r\n",
        "<img src=\"https://tracker.example.com/pixel.gif\">\r\n",
        "--REL\r\n",
        "Content-Type: image/png\r\n",
        "Content-ID: <logo@example.com>\r\n",
        "Content-Disposition: inline; filename=\"logo.png\"\r\n",
        "Content-Transfer-Encoding: base64\r\n",
        "\r\n",
        // 8 bytes of binary, base64.
        "iVBORw0KGgo=\r\n",
        "--REL--\r\n"
    )
    .as_bytes()
    .to_vec()
}

/// Number of "rich" fixture messages (uids 1..=RICH_MESSAGES), followed by synthetic ones.
const RICH_MESSAGES: u32 = 4;

/// The message the fixture serves for `uid`.
///
/// UIDs above the rich templates are generated header-only messages with **distinct Message-IDs**.
/// They exist so a test can grow the mailbox past the client's sync batch: the incremental-cursor
/// rule ("a page keeps the *oldest* entries") is only observable when one pass fetches more messages
/// than it can store, and that needs hundreds of them.
fn message_for(uid: u32) -> Vec<u8> {
    match uid {
        1 => message_one(),
        2 => message_two(),
        3 => message_three(),
        4 => message_four(),
        n => format!(
            "From: Sender {n} <s{n}@example.com>\r\n\
             To: me@example.com\r\n\
             Subject: Message {n}\r\n\
             Date: Mon, 01 Jan 2024 00:00:00 +0000\r\n\
             Message-ID: <filler-{n}@example.com>\r\n\
             \r\n"
        )
        .into_bytes(),
    }
}

/// Everything up to and including the blank line that ends the header block.
fn header_block(raw: &[u8]) -> &[u8] {
    match raw.windows(4).position(|w| w == b"\r\n\r\n") {
        Some(at) => &raw[..at + 4],
        None => raw,
    }
}

/// Parse the `<start.length>` suffix of a partial fetch, if the command carries one.
///
/// Takes an uppercased command line, which does not matter here: the suffix is all digits.
fn partial_range(query: &str) -> Option<(usize, usize)> {
    // `[]<` rather than `BODY[]<`: the client sends `BODY.PEEK[]<…>`, and `PEEK` does not stop a
    // section from being partial. Looking for the shorter prefix is also what the response echoes.
    let at = query.find("[]<")?;
    let rest = &query[at + 3..];
    let end = rest.find('>')?;
    let (start, length) = rest[..end].split_once('.')?;
    Some((start.parse().ok()?, length.parse().ok()?))
}

fn parse_uid_set(set: &str, available: u32) -> Vec<u32> {
    let mut out = Vec::new();
    for part in set.split(',') {
        let part = part.trim();
        if part == "*" {
            out.push(available);
        } else if let Some((from, to)) = part.split_once(':') {
            let from = from.parse::<u32>().unwrap_or(1);
            let to = if to == "*" {
                available
            } else {
                to.parse::<u32>().unwrap_or(available)
            };
            // RFC 3501: ranges are unordered, so `4:2` means the same as `2:4`. That matters for
            // the client: `UID FETCH <last+1>:*` on a folder with nothing new therefore resolves to
            // the LAST message rather than to an empty set, and a sync that trusted the server's
            // range would keep re-storing old mail. Keeping the fixture faithful to the spec is
            // what makes the incremental-sync test able to catch that.
            let (low, high) = if from <= to { (from, to) } else { (to, from) };
            for uid in low..=high {
                out.push(uid);
            }
        } else if let Ok(uid) = part.parse::<u32>() {
            out.push(uid);
        }
    }
    out.retain(|uid| (1..=available).contains(uid));
    out.dedup();
    out
}

type Transcript = Arc<Mutex<Vec<String>>>;

// ===================================================================== IMAP server

/// What the fake server does when a client enters `IDLE`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Idle {
    /// Acknowledge and stay silent, so the client sits in its idle window.
    Quiet,
    /// Push an unsolicited `EXISTS` — what a new arrival looks like.
    Announce,
    /// Push an `EXISTS` and then hang up, which is how a server ends an idle session it no longer
    /// wants. Needed to exercise the reconnect path without waiting for a real timeout.
    AnnounceThenHangUp,
}

/// Knobs the fake server needs.
///
/// A struct rather than a parameter list: this set keeps growing, and every addition would
/// otherwise touch every call site.
struct Server {
    /// Advertise and accept `UID EXPUNGE`, covering both delete strategies.
    uidplus: bool,
    /// `UIDVALIDITY` the server reports; a test can change it mid-run to simulate a renumbering.
    uidvalidity: Arc<AtomicU32>,
    /// When set, the server hangs up on the next `NOOP` instead of answering it.
    ///
    /// Real servers close idle connections without telling the client, which is precisely the case
    /// a connection pool has to survive. It cannot be reproduced by making the client misbehave, so
    /// the fixture has to do it.
    close_on_noop: Arc<AtomicBool>,
    /// How `IDLE` is answered.
    idle: Arc<Mutex<Idle>>,
    /// Messages that "arrived" mid-test, on top of the fixture's own count.
    ///
    /// `SELECT` reports `RICH_MESSAGES + extra`, so an `IDLE` announcement can genuinely change the mailbox
    /// size and a later `SELECT` sees it. Without this the watcher would compare against a constant
    /// and could never detect anything.
    extra: Arc<AtomicU32>,
    /// Advertise a `\Drafts` folder.
    ///
    /// Off by default because the folder list is asserted on elsewhere, and a fixture that quietly
    /// grows a folder makes those assertions lie about what they cover.
    drafts: bool,
}

impl Default for Server {
    fn default() -> Self {
        Self {
            uidplus: false,
            uidvalidity: Arc::new(AtomicU32::new(1)),
            close_on_noop: Arc::new(AtomicBool::new(false)),
            idle: Arc::new(Mutex::new(Idle::Quiet)),
            extra: Arc::new(AtomicU32::new(0)),
            drafts: false,
        }
    }
}

/// Start a fake IMAP server. `uidplus` controls whether `UID EXPUNGE` is advertised and
/// accepted, so both delete strategies can be covered.
async fn spawn_imap(uidplus: bool) -> (SocketAddr, Transcript) {
    spawn_server(Server {
        uidplus,
        ..Server::default()
    })
    .await
}

async fn spawn_server(server: Server) -> (SocketAddr, Transcript) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let transcript: Transcript = Arc::new(Mutex::new(Vec::new()));

    let log = Arc::clone(&transcript);
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            let log = Arc::clone(&log);
            let uidplus = server.uidplus;
            let drafts = server.drafts;
            let uidvalidity = Arc::clone(&server.uidvalidity);
            let close_on_noop = Arc::clone(&server.close_on_noop);
            let idle = Arc::clone(&server.idle);
            let extra = Arc::clone(&server.extra);
            tokio::spawn(async move {
                let _ = serve_imap(
                    stream,
                    uidplus,
                    drafts,
                    log,
                    uidvalidity,
                    close_on_noop,
                    idle,
                    extra,
                )
                .await;
            });
        }
    });

    (addr, transcript)
}

// The knobs are a parameter list rather than the `Server` struct because the accept loop needs owned
// `Arc`s per connection, and each one is a deliberate per-connection choice. A test fixture, not an API.
#[allow(clippy::too_many_arguments)]
async fn serve_imap(
    stream: TcpStream,
    uidplus: bool,
    drafts: bool,
    log: Transcript,
    uidvalidity: Arc<AtomicU32>,
    close_on_noop: Arc<AtomicBool>,
    idle: Arc<Mutex<Idle>>,
    extra: Arc<AtomicU32>,
) -> std::io::Result<()> {
    let (read_half, mut write) = stream.into_split();
    let mut reader = BufReader::new(read_half);

    let capabilities = if uidplus {
        "IMAP4rev1 UIDPLUS ID"
    } else {
        "IMAP4rev1 ID"
    };
    write
        .write_all(format!("* OK [CAPABILITY {capabilities}] loopback ready\r\n").as_bytes())
        .await?;

    // The mailbox size is `RICH_MESSAGES + extra`, and it is read **live** rather than snapshotted:
    // `extra` moves when the fixture simulates an arrival, and a test must then be able to fetch what
    // arrived. (A snapshot would make `SELECT` keep reporting the old size after a push, which is how
    // this line went wrong the first time.)
    let available = || RICH_MESSAGES + extra.load(Ordering::SeqCst);
    let mut line = String::new();

    loop {
        line.clear();
        if reader.read_line(&mut line).await? == 0 {
            return Ok(());
        }
        let trimmed = line.trim_end_matches(['\r', '\n']).to_string();
        if trimmed.is_empty() {
            continue;
        }
        log.lock().unwrap().push(trimmed.clone());

        let tokens: Vec<&str> = trimmed.split(' ').collect();
        let tag = tokens.first().copied().unwrap_or("*").to_string();
        let verb = tokens.get(1).copied().unwrap_or("").to_ascii_uppercase();

        match verb.as_str() {
            "CAPABILITY" => {
                write
                    .write_all(format!("* CAPABILITY {capabilities}\r\n").as_bytes())
                    .await?;
                ok(&mut write, &tag, "CAPABILITY completed").await?;
            }
            "ID" => {
                // RFC 2971. NetEase requires this before LOGIN; some servers answer BAD, which
                // the client tolerates, but here we accept it.
                write
                    .write_all(b"* ID (\"name\" \"loopback\" \"version\" \"1\")\r\n")
                    .await?;
                ok(&mut write, &tag, "ID completed").await?;
            }
            "LOGIN" => ok(&mut write, &tag, "LOGIN completed").await?,
            "LIST" => {
                // A \Noselect container must be reported but flagged unselectable. The Chinese
                // folder is in IMAP modified UTF-7: it must decode for display while staying
                // encoded on the wire for commands.
                write
                    .write_all(b"* LIST (\\HasNoChildren) \"/\" \"INBOX\"\r\n")
                    .await?;
                write
                    .write_all(b"* LIST (\\HasNoChildren \\Sent) \"/\" \"Sent\"\r\n")
                    .await?;
                if drafts {
                    write
                        .write_all(b"* LIST (\\HasNoChildren \\Drafts) \"/\" \"Drafts\"\r\n")
                        .await?;
                }
                write
                    .write_all(b"* LIST (\\HasNoChildren) \"/\" \"&UXZO1mWHTvZZOQ-\"\r\n")
                    .await?;
                write
                    .write_all(b"* LIST (\\Noselect \\HasChildren) \"/\" \"[Gmail]\"\r\n")
                    .await?;
                ok(&mut write, &tag, "LIST completed").await?;
            }
            "SELECT" | "EXAMINE" => {
                write
                    .write_all(format!("* {} EXISTS\r\n", available()).as_bytes())
                    .await?;
                write.write_all(b"* 0 RECENT\r\n").await?;
                write
                    .write_all(b"* FLAGS (\\Answered \\Flagged \\Deleted \\Seen \\Draft)\r\n")
                    .await?;
                write
                    .write_all(
                        b"* OK [PERMANENTFLAGS (\\Answered \\Flagged \\Deleted \\Seen \\Draft \\*)] Flags permitted\r\n",
                    )
                    .await?;
                write
                    .write_all(
                        format!(
                            "* OK [UIDVALIDITY {}] UIDs valid\r\n",
                            uidvalidity.load(Ordering::SeqCst)
                        )
                        .as_bytes(),
                    )
                    .await?;
                write
                    .write_all(
                        format!("* OK [UIDNEXT {}] Predicted next UID\r\n", available() + 1)
                            .as_bytes(),
                    )
                    .await?;
                ok(&mut write, &tag, "[READ-WRITE] SELECT completed").await?;
            }
            "UID" => {
                let sub = tokens.get(2).copied().unwrap_or("").to_ascii_uppercase();
                match sub.as_str() {
                    "SEARCH" => {
                        // A `HEADER Message-ID` search answers with the UID the earlier APPEND was
                        // given by the server (4242). That is the round trip the design relies on:
                        // APPEND tells us nothing, so we ask again.
                        if trimmed.to_ascii_uppercase().contains("HEADER MESSAGE-ID") {
                            write.write_all(b"* SEARCH 4242\r\n").await?;
                            ok(&mut write, &tag, "UID SEARCH completed").await?;
                            continue;
                        }
                        // Honour the range: a real server answers `UID SEARCH 1:100` with those UIDs
                        // only, and `UID SEARCH ALL` with the whole mailbox. Returning everything for
                        // any query (as this fixture used to) hides bugs in a caller that trusts the
                        // result to be the range it asked for.
                        let set = tokens.get(3).copied().unwrap_or("1:*");
                        let ids: Vec<String> = if set.eq_ignore_ascii_case("ALL") {
                            (1..=available()).map(|n| n.to_string()).collect()
                        } else {
                            parse_uid_set(set, available())
                                .into_iter()
                                .map(|n| n.to_string())
                                .collect()
                        };
                        write
                            .write_all(format!("* SEARCH {}\r\n", ids.join(" ")).as_bytes())
                            .await?;
                        ok(&mut write, &tag, "UID SEARCH completed").await?;
                    }
                    "FETCH" => {
                        let set = tokens.get(3).copied().unwrap_or("1");
                        let query = trimmed.to_ascii_uppercase();
                        let headers_only = query.contains("[HEADER]");
                        // A partial fetch (`BODY[]<start.length>`) must return only that slice, or a
                        // streaming download would silently receive the whole message every time.
                        let partial = partial_range(&query);
                        for uid in parse_uid_set(set, available()) {
                            let raw = &message_for(uid);
                            let (payload, section): (&[u8], String) = match partial {
                                Some((start, length)) => {
                                    let start = start.min(raw.len());
                                    let end = (start + length).min(raw.len());
                                    (&raw[start..end], format!("BODY[]<{start}.{length}>"))
                                }
                                None if headers_only => {
                                    (header_block(raw), "BODY[HEADER]".to_string())
                                }
                                None => (raw.as_slice(), "BODY[]".to_string()),
                            };
                            // `UID` must be echoed or the client cannot correlate the response.
                            let flags = if uid == 1 { "\\Seen" } else { "" };
                            let head = format!(
                                "* {uid} FETCH (UID {uid} FLAGS ({flags}) RFC822.SIZE {} {section} {{{}}}\r\n",
                                raw.len(),
                                payload.len()
                            );
                            write.write_all(head.as_bytes()).await?;
                            write.write_all(payload).await?;
                            write.write_all(b")\r\n").await?;
                        }
                        ok(&mut write, &tag, "UID FETCH completed").await?;
                    }
                    "STORE" => {
                        let set = tokens.get(3).copied().unwrap_or("1");
                        let flags = trimmed
                            .split_once('(')
                            .and_then(|(_, rest)| rest.split_once(')'))
                            .map(|(inner, _)| inner.to_string())
                            .unwrap_or_default();
                        for uid in parse_uid_set(set, available()) {
                            write
                                .write_all(
                                    format!("* {uid} FETCH (UID {uid} FLAGS ({flags}))\r\n")
                                        .as_bytes(),
                                )
                                .await?;
                        }
                        ok(&mut write, &tag, "UID STORE completed").await?;
                    }
                    "COPY" => ok(&mut write, &tag, "UID COPY completed").await?,
                    "EXPUNGE" => {
                        if uidplus {
                            let set = tokens.get(3).copied().unwrap_or("1");
                            for uid in parse_uid_set(set, available()) {
                                write
                                    .write_all(format!("* {uid} EXPUNGE\r\n").as_bytes())
                                    .await?;
                            }
                            ok(&mut write, &tag, "UID EXPUNGE completed").await?;
                        } else {
                            write
                                .write_all(
                                    format!("{tag} BAD Unknown command: UID EXPUNGE\r\n")
                                        .as_bytes(),
                                )
                                .await?;
                        }
                    }
                    _ => bad(&mut write, &tag, "unsupported UID subcommand").await?,
                }
            }
            "EXPUNGE" => {
                write.write_all(b"* 1 EXPUNGE\r\n").await?;
                ok(&mut write, &tag, "EXPUNGE completed").await?;
            }
            "APPEND" => {
                // async-imap sends `APPEND <mailbox> (flags) {N}`, waits for a continuation (`+`),
                // and only then writes the literal. Answering in the wrong order deadlocks the
                // client, which is how this arm was written the first time.
                let literal = trimmed
                    .rsplit_once('{')
                    .and_then(|(_, rest)| rest.trim_end_matches('}').parse::<usize>().ok());
                if let Some(size) = literal {
                    write.write_all(b"+ Ready for literal data\r\n").await?;
                    let mut body = vec![0u8; size];
                    tokio::io::AsyncReadExt::read_exact(&mut reader, &mut body).await?;
                    // Consume the CRLF that terminates the literal.
                    line.clear();
                    reader.read_line(&mut line).await?;
                }
                if uidplus {
                    write
                        .write_all(
                            format!("{tag} OK [APPENDUID 1 4242] APPEND completed\r\n").as_bytes(),
                        )
                        .await?;
                } else {
                    ok(&mut write, &tag, "APPEND completed").await?;
                }
            }
            "IDLE" => {
                write.write_all(b"+ idling\r\n").await?;
                let behaviour = *idle.lock().unwrap();

                if behaviour != Idle::Quiet {
                    // Let the client actually be idling before pushing, then announce one more
                    // message than the fixture holds — which is what an arrival looks like.
                    tokio::time::sleep(Duration::from_millis(120)).await;
                    // Record the arrival, so the `SELECT` after `DONE` reports the new size.
                    // That comparison is how the watcher notices anything at all, so a push that
                    // did not move `extra` would be a test that could never pass.
                    //
                    // The total is read from the atomic *after* the increment: the mailbox is
                    // `RICH_MESSAGES + extra`, so adding a separately-tracked "arrived" count on top
                    // of an already-inclusive number would double-count.
                    extra.fetch_add(1, Ordering::SeqCst);
                    let total = RICH_MESSAGES + extra.load(Ordering::SeqCst);
                    write
                        .write_all(format!("* {total} EXISTS\r\n").as_bytes())
                        .await?;
                }

                if behaviour == Idle::AnnounceThenHangUp {
                    // Dropping the stream ends the connection mid-idle, which is how a server
                    // closes a session it no longer wants.
                    return Ok(());
                }

                // Otherwise stay in this arm until `DONE`, so the client really is idling rather
                // than getting a tagged response immediately.
                loop {
                    line.clear();
                    if reader.read_line(&mut line).await? == 0 {
                        return Ok(());
                    }
                    let command = line.trim_end_matches(['\r', '\n']).to_string();
                    log.lock().unwrap().push(command.clone());
                    // `DONE` carries no tag — the tag belongs to the `IDLE` command it
                    // terminates — so this has to look at the FIRST token. Checking the second
                    // silently never matches, and both sides then wait for each other.
                    if command
                        .split(' ')
                        .next()
                        .is_some_and(|verb| verb.eq_ignore_ascii_case("DONE"))
                    {
                        ok(&mut write, &tag, "IDLE terminated").await?;
                        break;
                    }
                }
            }
            "NOOP" | "CHECK" | "CLOSE" => {
                // A pool must survive a server that closed an idle connection without saying so.
                if close_on_noop.swap(false, Ordering::SeqCst) {
                    return Ok(());
                }
                ok(&mut write, &tag, "completed").await?
            }
            "LOGOUT" => {
                write.write_all(b"* BYE logging out\r\n").await?;
                ok(&mut write, &tag, "LOGOUT completed").await?;
                return Ok(());
            }
            other => bad(&mut write, &tag, &format!("unsupported command {other}")).await?,
        }
    }
}

async fn ok<W: AsyncWriteExt + Unpin>(write: &mut W, tag: &str, text: &str) -> std::io::Result<()> {
    write
        .write_all(format!("{tag} OK {text}\r\n").as_bytes())
        .await
}

async fn bad<W: AsyncWriteExt + Unpin>(
    write: &mut W,
    tag: &str,
    text: &str,
) -> std::io::Result<()> {
    write
        .write_all(format!("{tag} BAD {text}\r\n").as_bytes())
        .await
}

/// Start a fake IMAP server that refuses every `LOGIN` with a QQ-shaped rejection.
///
/// Used to check what the client does *after* an authentication failure, which is where the
/// login-frequency-limit trap lives.
async fn spawn_imap_rejecting_login() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");

    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            tokio::spawn(async move {
                let (read_half, mut write) = stream.into_split();
                let mut reader = BufReader::new(read_half);
                let _ = write
                    .write_all(b"* OK [CAPABILITY IMAP4rev1] ready\r\n")
                    .await;

                let mut line = String::new();
                while reader.read_line(&mut line).await.unwrap_or(0) > 0 {
                    let trimmed = line.trim_end().to_string();
                    let tag = trimmed.split(' ').next().unwrap_or("1").to_string();
                    let upper = trimmed.to_ascii_uppercase();

                    if upper.contains("LOGIN") {
                        let _ = write
                            .write_all(
                                format!(
                                    "{tag} NO Login fail. Account is not open, password is \
                                     incorrect, login frequency limited, or system busy. For \
                                     more information at https://help.mail.qq.com/detail/108/1023\r\n"
                                )
                                .as_bytes(),
                            )
                            .await;
                    } else {
                        let _ = write.write_all(format!("{tag} OK\r\n").as_bytes()).await;
                    }
                    line.clear();
                }
            });
        }
    });

    addr
}

// ===================================================================== SMTP server

/// Start a fake SMTP server. Returns the address and the bytes it received in `DATA`.
async fn spawn_smtp() -> (SocketAddr, Arc<Mutex<Vec<u8>>>) {
    let (addr, captured, _connections) = spawn_smtp_counted().await;
    (addr, captured)
}

/// As [`spawn_smtp`], but also reports how many TCP connections were accepted.
///
/// The connection count is what proves "SMTP was never attempted" — an empty `DATA` buffer is
/// also what a server would report if it were contacted and then failed at AUTH.
async fn spawn_smtp_counted() -> (SocketAddr, Arc<Mutex<Vec<u8>>>, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let captured: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
    let connections = Arc::new(AtomicUsize::new(0));

    let sink = Arc::clone(&captured);
    let counter = Arc::clone(&connections);
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            counter.fetch_add(1, Ordering::SeqCst);
            let sink = Arc::clone(&sink);
            tokio::spawn(async move {
                let _ = serve_smtp(stream, sink).await;
            });
        }
    });

    (addr, captured, connections)
}

/// As [`spawn_smtp_counted`], but also returns the SMTP command transcript.
async fn spawn_smtp_with_commands() -> (
    SocketAddr,
    Arc<Mutex<Vec<u8>>>,
    Arc<AtomicUsize>,
    Arc<Mutex<Vec<String>>>,
) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let captured: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
    let connections = Arc::new(AtomicUsize::new(0));
    let commands: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));

    let sink = Arc::clone(&captured);
    let counter = Arc::clone(&connections);
    let log = Arc::clone(&commands);
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            counter.fetch_add(1, Ordering::SeqCst);
            let sink = Arc::clone(&sink);
            let log = Arc::clone(&log);
            tokio::spawn(async move {
                let _ = serve_smtp_logging(stream, sink, log).await;
            });
        }
    });

    (addr, captured, connections, commands)
}

async fn serve_smtp(stream: TcpStream, sink: Arc<Mutex<Vec<u8>>>) -> std::io::Result<()> {
    serve_smtp_logging(stream, sink, Arc::new(Mutex::new(Vec::new()))).await
}

/// As [`serve_smtp`], but records every command line.
///
/// The transcript is what lets a probe assert something about the *protocol* rather than about the
/// message: `MAIL FROM` must not carry `SIZE=` (lettre never sends it — see A1-F45), and it is the
/// only way to see that from the outside.
async fn serve_smtp_logging(
    stream: TcpStream,
    sink: Arc<Mutex<Vec<u8>>>,
    commands: Arc<Mutex<Vec<String>>>,
) -> std::io::Result<()> {
    let (read_half, mut write) = stream.into_split();
    let mut reader = BufReader::new(read_half);

    write
        .write_all(b"220 loopback.test ESMTP ready\r\n")
        .await?;

    let mut line = String::new();
    loop {
        line.clear();
        if reader.read_line(&mut line).await? == 0 {
            return Ok(());
        }
        let trimmed = line.trim_end_matches(['\r', '\n']).to_string();
        let upper = trimmed.to_ascii_uppercase();
        commands.lock().unwrap().push(trimmed.clone());

        if upper.starts_with("EHLO") {
            // The final line uses a space, not a hyphen, or the client keeps reading.
            // `XOAUTH2` is advertised as well, because a server that does not offer it is a different
            // scenario; the client is the one under test here.
            write
                .write_all(
                    b"250-loopback.test\r\n250-AUTH PLAIN LOGIN XOAUTH2\r\n250-8BITMIME\r\n250-SIZE 10485760\r\n250 HELP\r\n",
                )
                .await?;
        } else if upper.starts_with("HELO") {
            write.write_all(b"250 loopback.test\r\n").await?;
        } else if upper.starts_with("AUTH XOAUTH2") {
            // An OAuth2 access token is sent as `AUTH XOAUTH2 <base64>`; the base64 is recorded by the
            // caller, which is how the test asserts the *shape* of the line rather than just "auth
            // happened". A server that wants an empty challenge is not needed: lettre sends the initial
            // response on the same line.
            write
                .write_all(b"235 2.7.0 Authentication successful\r\n")
                .await?;
        } else if upper.starts_with("AUTH PLAIN") {
            // Credentials may arrive on the same line or on a continuation line.
            if trimmed.split_whitespace().count() < 3 {
                write.write_all(b"334 \r\n").await?;
                line.clear();
                reader.read_line(&mut line).await?;
            }
            write
                .write_all(b"235 2.7.0 Authentication successful\r\n")
                .await?;
        } else if upper.starts_with("AUTH LOGIN") {
            write.write_all(b"334 VXNlcm5hbWU6\r\n").await?;
            line.clear();
            reader.read_line(&mut line).await?;
            write.write_all(b"334 UGFzc3dvcmQ6\r\n").await?;
            line.clear();
            reader.read_line(&mut line).await?;
            write
                .write_all(b"235 2.7.0 Authentication successful\r\n")
                .await?;
        } else if upper.starts_with("MAIL FROM") || upper.starts_with("RCPT TO") {
            write.write_all(b"250 2.1.0 OK\r\n").await?;
        } else if upper.starts_with("DATA") {
            write
                .write_all(b"354 End data with <CR><LF>.<CR><LF>\r\n")
                .await?;
            let mut body = Vec::new();
            loop {
                line.clear();
                if reader.read_line(&mut line).await? == 0 {
                    break;
                }
                if line == ".\r\n" || line == ".\n" {
                    break;
                }
                // Undo dot-stuffing per RFC 5321 §4.5.2.
                if let Some(rest) = line.strip_prefix("..") {
                    body.extend_from_slice(b".");
                    body.extend_from_slice(rest.as_bytes());
                } else {
                    body.extend_from_slice(line.as_bytes());
                }
            }
            sink.lock().unwrap().extend_from_slice(&body);
            write.write_all(b"250 2.0.0 OK queued\r\n").await?;
        } else if upper.starts_with("QUIT") {
            write.write_all(b"221 2.0.0 Bye\r\n").await?;
            return Ok(());
        } else {
            // RSET, NOOP, and anything else this fixture does not need to model.
            write.write_all(b"250 2.0.0 OK\r\n").await?;
        }
    }
}

// ===================================================================== outbox worker

/// The whole queue path against a real SMTP conversation: `user.db` row -> spool file -> worker ->
/// `send_spool` -> the server's `DATA`.
///
/// This is the test that would have caught the class of bug M1a-7 found in the other direction: the
/// per-piece tests all passed while the pieces did not fit together. Here nothing is stubbed except
/// the server.
#[tokio::test]
async fn a_queued_message_reaches_smtp_through_the_worker() {
    let (addr, captured, connections) = spawn_smtp_counted().await;
    let cfg = account(addr, addr);
    let store = UserStore::open_in_memory().expect("user db");
    let spool_dir = std::env::temp_dir().join(format!(
        "easy-email-loopback-spool-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    ));
    let spool = Spool::open(&spool_dir).expect("spool");

    // A draft the send came from: the worker has to remove it once the message is out.
    let mut draft = Draft::new("d-outbox", &cfg.id, 100);
    draft.subject = "来自草稿".to_string();
    store.save_draft(&draft).expect("draft");

    let mail = OutgoingMail {
        to: vec!["alice@example.com".to_string()],
        cc: vec![],
        bcc: vec!["hidden@example.com".to_string()],
        subject: "队列里的信".to_string(),
        body_text: "正文".to_string(),
        body_html: None,
        in_reply_to: None,
        references: vec![],
        attachments: vec![],
        // Fixed on purpose: this is what the outbox relies on when it rebuilds a lost spool file,
        // and what the Sent-copy search looks for afterwards.
        message_id: Some("<frozen@example.com>".to_string()),
    };

    let id = store
        .enqueue_outbox(&NewOutbox {
            account_id: cfg.id.clone(),
            draft_id: Some("d-outbox".to_string()),
            payload_json: serde_json::to_string(&mail).expect("payload"),
            spool_path: None,
            spool_bytes: 0,
            // The undo window has passed: this message is due.
            undo_until: 0,
            scheduled_at: 0,
            created_at: 100,
        })
        .expect("enqueue");

    // The bytes are frozen at enqueue time, which is what makes the send reproducible.
    let message = smtp::build_message(&cfg, &mail).expect("build");
    let (path, size) = spool.write(id, &message.formatted()).expect("spool");
    store
        .set_outbox_spool(id, &path.to_string_lossy(), size as i64)
        .expect("record spool");

    let report = within(run_due(
        OutboxRun {
            store: &store,
            spool: &spool,
            cfg: &cfg,
            password: "authorization-code",
            now: 1_000,
            limit: 5,
        },
        |cfg, password, bytes, recipients| async move {
            smtp::send_spool(&cfg, &password, bytes, recipients).await
        },
    ))
    .await
    .expect("worker pass");

    assert_eq!(report.sent, 1, "one message, sent: {report:?}");
    assert_eq!(
        connections.load(Ordering::SeqCst),
        1,
        "exactly one SMTP connection"
    );

    let raw = String::from_utf8_lossy(&captured.lock().unwrap().clone()).to_string();
    assert!(
        raw.contains("Subject: "),
        "no message reached the server:\n{raw}"
    );
    assert!(
        raw.contains("队列里的信") || raw.contains("=?utf-8?"),
        "raw:\n{raw}"
    );
    assert!(
        raw.contains("Message-ID: <frozen@example.com>"),
        "the frozen Message-ID must be the one on the wire:\n{raw}"
    );
    assert!(
        !raw.contains("hidden@example.com"),
        "a Bcc address must never appear in the headers:\n{raw}"
    );

    let item = store.outbox_item(id).expect("item").expect("present");
    assert_eq!(
        item.sent_message_id.as_deref(),
        Some("<frozen@example.com>"),
        "the id is recorded for the Sent copy and for crash recovery"
    );
    assert!(
        path.exists(),
        "the bytes stay until the Sent copy is filed (design/07 §5.4)"
    );
    assert_eq!(
        store.sent_awaiting_copy().expect("awaiting").len(),
        1,
        "the row is queued up for the filing pass"
    );
    assert!(
        store.load_draft("d-outbox").expect("draft").is_none(),
        "the draft is removed once the message is out"
    );

    let _ = std::fs::remove_dir_all(&spool_dir);
}

#[tokio::test]
async fn a_send_cancelled_inside_its_undo_window_never_opens_a_connection() {
    // The undo window is the whole point of §5.2: the message must not have been *attempted*. An
    // empty DATA buffer alone would not prove that, because a connection that failed at AUTH also
    // leaves nothing behind — hence the connection counter.
    let (addr, captured, connections) = spawn_smtp_counted().await;
    let cfg = account(addr, addr);
    let store = UserStore::open_in_memory().expect("user db");
    let spool_dir = std::env::temp_dir().join(format!(
        "easy-email-loopback-undo-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    ));
    let spool = Spool::open(&spool_dir).expect("spool");

    let mail = OutgoingMail {
        to: vec!["alice@example.com".to_string()],
        cc: vec![],
        bcc: vec![],
        subject: "还来得及撤销".to_string(),
        body_text: "正文".to_string(),
        body_html: None,
        in_reply_to: None,
        references: vec![],
        attachments: vec![],
        message_id: None,
    };
    let id = store
        .enqueue_outbox(&NewOutbox {
            account_id: cfg.id.clone(),
            draft_id: None,
            payload_json: serde_json::to_string(&mail).expect("payload"),
            spool_path: None,
            spool_bytes: 0,
            // Five seconds of grace, as the design's default.
            undo_until: 1_005,
            scheduled_at: 0,
            created_at: 1_000,
        })
        .expect("enqueue");
    let message = smtp::build_message(&cfg, &mail).expect("build");
    let (path, size) = spool.write(id, &message.formatted()).expect("spool");
    store
        .set_outbox_spool(id, &path.to_string_lossy(), size as i64)
        .expect("record spool");

    // One pass before the window expires: nothing may happen.
    let before = within(run_due(
        OutboxRun {
            store: &store,
            spool: &spool,
            cfg: &cfg,
            password: "authorization-code",
            now: 1_003,
            limit: 5,
        },
        |cfg, password, bytes, recipients| async move {
            smtp::send_spool(&cfg, &password, bytes, recipients).await
        },
    ))
    .await
    .expect("worker pass");
    assert_eq!(before.attempted, 0);

    // The user clicks 撤销.
    assert!(store.cancel_outbox(id).expect("cancel"));

    // And the window expires.
    let after = within(run_due(
        OutboxRun {
            store: &store,
            spool: &spool,
            cfg: &cfg,
            password: "authorization-code",
            now: 9_999,
            limit: 5,
        },
        |cfg, password, bytes, recipients| async move {
            smtp::send_spool(&cfg, &password, bytes, recipients).await
        },
    ))
    .await
    .expect("worker pass");

    assert_eq!(after.attempted, 0, "a cancelled send stays cancelled");
    assert_eq!(
        connections.load(Ordering::SeqCst),
        0,
        "no SMTP connection may be opened for a cancelled message"
    );
    assert!(captured.lock().unwrap().is_empty());

    let _ = std::fs::remove_dir_all(&spool_dir);
}

// ===================================================================== crash recovery

/// The §5.3.1 check, against a real IMAP folder: a row a crash left in `sending` whose message the
/// Sent folder already has must be settled as **sent**, never queued again.
#[tokio::test]
async fn crash_recovery_finds_the_message_in_the_sent_folder_and_does_not_resend_it() {
    let (addr, log) = spawn_imap(true).await;
    let cfg = account(addr, addr);

    // The folder lookup is part of the contract: a server may localise the name entirely and only
    // say \Sent in the attribute, so the folder is found by role, not by the literal "Sent".
    let folder = {
        let mut client = within(ImapClient::connect(&cfg, "app-password"))
            .await
            .expect("connect");
        within(client.sent_mailbox())
            .await
            .expect("LIST must succeed")
            .expect("the fixture has a \\Sent folder")
    };
    assert_eq!(folder, "Sent");

    let store = UserStore::open_in_memory().expect("user db");
    let spool_dir = std::env::temp_dir().join(format!(
        "easy-email-loopback-recover-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    ));
    let spool = Spool::open(&spool_dir).expect("spool");

    let mail = OutgoingMail {
        to: vec!["alice@example.com".to_string()],
        cc: vec![],
        bcc: vec![],
        subject: "崩溃前发出去了吗".to_string(),
        body_text: "正文".to_string(),
        body_html: None,
        in_reply_to: None,
        references: vec![],
        attachments: vec![],
        message_id: Some("<recovered@example.com>".to_string()),
    };

    let id = store
        .enqueue_outbox(&NewOutbox {
            account_id: cfg.id.clone(),
            draft_id: None,
            payload_json: serde_json::to_string(&mail).expect("payload"),
            spool_path: None,
            spool_bytes: 0,
            undo_until: 0,
            scheduled_at: 0,
            created_at: 1_000,
        })
        .expect("enqueue");
    let message = smtp::build_message(&cfg, &mail).expect("build");
    let (path, size) = spool.write(id, &message.formatted()).expect("spool");
    store
        .set_outbox_spool(id, &path.to_string_lossy(), size as i64)
        .expect("record spool");
    // The crash: the row was claimed and never resolved.
    assert!(store.claim_outbox(id, 1_000).expect("claim"));
    assert_eq!(store.outbox_in_flight().expect("in flight").len(), 1);

    let report = within(recover_in_flight(
        RecoveryRun {
            store: &store,
            spool: &spool,
            now: 2_000,
            max_auto_bytes: mail_core::DEFAULT_CRASH_RESEND_LIMIT,
        },
        |message_id| {
            let cfg = cfg.clone();
            async move {
                let mut client = ImapClient::connect(&cfg, "app-password").await?;
                match client.sent_mailbox().await? {
                    Some(folder) => Ok(client
                        .find_uid_by_message_id(&folder, &message_id)
                        .await?
                        .is_some()),
                    None => Ok(false),
                }
            }
        },
    ))
    .await
    .expect("recovery");

    assert_eq!(report.already_sent, 1, "{report:?}");
    assert_eq!(
        report.requeued, 0,
        "a delivered message must not queue again"
    );
    assert!(
        store.due_outbox(i64::MAX / 2, 10).expect("due").is_empty(),
        "the row must not be eligible for another send"
    );
    let item = store.outbox_item(id).expect("item").expect("present");
    assert_eq!(item.parsed_state(), mail_core::OutboxState::Sent);
    assert_eq!(
        item.sent_message_id.as_deref(),
        Some("<recovered@example.com>")
    );

    // And the search really went over IMAP: an implementation that guessed would leave no trace.
    // (`saw_command` is for single-verb commands like LOGIN, so this looks at the whole line.)
    let lines = transcript(&log);
    assert!(
        lines.iter().any(|line| {
            let upper = line.to_ascii_uppercase();
            upper.contains("SEARCH") && upper.contains("MESSAGE-ID")
        }),
        "expected a Message-ID search: {lines:?}"
    );

    let _ = std::fs::remove_dir_all(&spool_dir);
}

// ===================================================================== Sent copy

/// `design/07` §5.4 against a real folder: a message the server already filed must be **found**, not
/// appended again. The duplicate is the failure mode this rule exists to prevent, and the only way to
/// see it is to check that no `APPEND` happened at all.
#[tokio::test]
async fn an_already_filed_message_is_found_instead_of_appended_again() {
    let (addr, log) = spawn_imap(true).await;
    let cfg = account(addr, addr);

    let mut client = within(ImapClient::connect(&cfg, "app-password"))
        .await
        .expect("connect");

    let raw = b"From: me@example.com\r\nTo: them@example.com\r\n\
                Message-ID: <filed@example.com>\r\nSubject: hi\r\n\r\nbody\r\n";
    let copy = within(ensure_sent_copy(
        &mut client,
        raw,
        "<filed@example.com>",
        None,
    ))
    .await
    .expect("filing must succeed")
    .expect("the fixture has a Sent folder");

    assert_eq!(copy.folder, "Sent");
    assert!(
        !copy.appended,
        "the server had it already; appending would leave two copies in 已发送"
    );

    let lines = transcript(&log);
    assert!(
        lines
            .iter()
            .any(|line| line.to_ascii_uppercase().contains("MESSAGE-ID")),
        "it has to look before it writes: {lines:?}"
    );
    assert!(
        !lines
            .iter()
            .any(|line| line.to_ascii_uppercase().contains("APPEND")),
        "and it must not write when the lookup succeeded: {lines:?}"
    );
}

// ===================================================================== quota

/// `design/04` §5.7's gate: once the daily byte budget is spent the walk must **stop asking**, not
/// wait to be refused. The provider's refusal is what throttles the account for a day, so "we kept
/// going and got told off" is the failure this test exists to rule out.
#[tokio::test]
async fn the_backfill_stops_fetching_once_the_byte_budget_is_spent() {
    let (addr, log) = spawn_imap(true).await;
    let cfg = account(addr, addr);
    let cache = Cache::open_in_memory().expect("cache");

    // Two messages are cached, so the walk has somewhere to start; `oldest_uid` is what it asks below.
    {
        let mut client = within(ImapClient::connect(&cfg, "app-password"))
            .await
            .expect("connect");
        let page = within(client.fetch_newest_headers("INBOX", 2))
            .await
            .expect("prime the cache");
        cache
            .upsert_summaries(&cfg.id, "INBOX", &page.summaries)
            .expect("store");
    }

    let before = transcript(&log).len();
    let report = within(backfill_older(
        &mut within(ImapClient::connect(&cfg, "app-password"))
            .await
            .expect("connect"),
        &cache,
        &cfg.id,
        "INBOX",
        SyncWindow::All,
        // One byte: the first page's sizes already exceed it, so the walk has to stop after that page
        // however many pages (or messages) the other limits would still allow.
        BackfillBudget {
            max_pages: 40,
            max_messages: 20_000,
            max_bytes: 1,
        },
        1_700_000_000,
    ))
    .await
    .expect("walk");

    assert_eq!(report.stop, BackfillStop::QuotaSpent, "{report:?}");
    assert_eq!(
        report.pages, 1,
        "it stops after the page that spent the budget, not after the next one"
    );
    assert!(report.bytes >= 1, "and it reports what it spent");

    // The proof that it stopped asking: exactly one page worth of FETCH after the priming read.
    let after = transcript(&log);
    let fetches = after[before..]
        .iter()
        .filter(|line| line.to_ascii_uppercase().contains("UID FETCH"))
        .count();
    assert_eq!(
        fetches,
        1,
        "a walk that kept going would show more fetches: {:?}",
        &after[before..]
    );
}

/// The budget is a *stop*, not a cap on what is stored: the page that crossed the line is the page
/// the user is about to read, so it must land in the cache.
#[tokio::test]
async fn the_page_that_spent_the_budget_is_still_stored() {
    let (addr, _log) = spawn_imap(true).await;
    let cfg = account(addr, addr);

    // Four messages cached, then walk with a budget that the first fetched page will exhaust.
    let mut client = within(ImapClient::connect(&cfg, "app-password"))
        .await
        .expect("connect");
    let page = within(client.fetch_newest_headers("INBOX", 4))
        .await
        .expect("prime");

    let cache = Cache::open_in_memory().expect("cache");
    // Only the newest two, so there is something older to walk towards.
    cache
        .upsert_summaries(&cfg.id, "INBOX", &page.summaries[2..])
        .expect("store");
    let before = cache.summary_count(&cfg.id, "INBOX").expect("count");

    let report = within(backfill_older(
        &mut client,
        &cache,
        &cfg.id,
        "INBOX",
        SyncWindow::All,
        BackfillBudget {
            max_pages: 40,
            max_messages: 20_000,
            max_bytes: 1,
        },
        1_700_000_000,
    ))
    .await
    .expect("walk");

    assert_eq!(report.stop, BackfillStop::QuotaSpent);
    assert!(
        cache.summary_count(&cfg.id, "INBOX").expect("count") > before,
        "the page it stopped on has to be readable, or the next walk fetches it again"
    );
}

// ===================================================================== draft mirror

/// `design/07` §3: the draft has two copies, and the server one must not pile up.
///
/// Two things are easy to get wrong and both are asserted here: a draft mirror must not be marked
/// `\Seen` (a draft has not been read), and mirroring the same draft again must delete the copy we
/// wrote last time instead of leaving one more behind at every autosave.
#[tokio::test]
async fn mirroring_a_draft_replaces_our_previous_copy() {
    let (addr, log) = spawn_server(Server {
        uidplus: true,
        drafts: true,
        ..Server::default()
    })
    .await;
    let cfg = account(addr, addr);
    let mut client = within(ImapClient::connect(&cfg, "app-password"))
        .await
        .expect("connect");

    let mut draft = Draft::new("d1", &cfg.id, 1);
    draft.to_addrs = "a@example.com".into();
    draft.subject = "草稿镜像".into();
    draft.body_text = "正文".into();

    let first = within(mirror_draft(&mut client, &cfg, &draft, None))
        .await
        .expect("mirror must succeed")
        .expect("the fixture advertises a Drafts folder");
    assert_eq!(first.folder, "Drafts");
    assert_eq!(
        first.uid,
        Some(4242),
        "the copy is found again by its Message-ID, because APPEND returns no UID"
    );
    assert_eq!(first.replaced, None, "there was nothing to replace");

    let lines = transcript(&log);
    let append = lines
        .iter()
        .find(|line| line.to_ascii_uppercase().contains("APPEND"))
        .unwrap_or_else(|| panic!("no APPEND in {lines:?}"));
    assert!(
        append.contains("Drafts"),
        "the copy belongs in the Drafts folder: {append}"
    );
    assert!(
        !append.to_ascii_uppercase().contains("\\SEEN"),
        "a draft has not been read: {append}"
    );

    // Mirroring again: the copy we wrote last time has to go, or every autosave leaves another one.
    let previous = DraftMirror {
        draft_id: "d1".into(),
        account_id: cfg.id.clone(),
        folder: first.folder.clone(),
        uid: first.uid,
        message_id: first.message_id.clone(),
        updated_at: 1,
    };
    let second = within(mirror_draft(&mut client, &cfg, &draft, Some(&previous)))
        .await
        .expect("mirror")
        .expect("folder");
    assert_eq!(second.replaced, Some(4242), "{second:?}");

    let lines = transcript(&log);
    assert!(
        lines
            .iter()
            .any(|line| line.to_ascii_uppercase().contains("UID STORE 4242")),
        "the previous copy is what gets flagged deleted: {lines:?}"
    );
}

/// `design/07` §5.4's `SkippedTooLarge`: when the server has told us the message is too big for an
/// APPEND, the attempt must not happen at all — and, just as importantly, the caller must not retry.
#[tokio::test]
async fn a_copy_larger_than_the_append_limit_is_never_appended() {
    let (addr, log) = spawn_imap(true).await;
    let cfg = account(addr, addr);
    let mut client = within(ImapClient::connect(&cfg, "app-password"))
        .await
        .expect("connect");

    let raw = b"From: me@example.com\r\nMessage-ID: <huge@example.com>\r\n\r\nbody\r\n";
    let copy = within(ensure_sent_copy(
        &mut client,
        raw,
        "<huge@example.com>",
        Some(1), // one byte: everything is too big for this server
    ))
    .await
    .expect("must not be an error");

    assert_eq!(
        copy, None,
        "nothing is filed, and nothing ever will be for this message"
    );
    let lines = transcript(&log);
    assert!(
        !lines
            .iter()
            .any(|line| line.to_ascii_uppercase().contains("APPEND")),
        "an APPEND the server has already refused must not be attempted: {lines:?}"
    );
}

/// No Drafts folder on the account is not a mirror failure: the local draft is still saved, which is
/// the promise that matters. `None` says "nowhere to put it".
#[tokio::test]
async fn an_account_without_a_drafts_folder_simply_has_no_mirror() {
    let (addr, _log) = spawn_imap(true).await;
    let cfg = account(addr, addr);
    let mut client = within(ImapClient::connect(&cfg, "app-password"))
        .await
        .expect("connect");

    let draft = Draft::new("d1", &cfg.id, 1);
    let report = within(mirror_draft(&mut client, &cfg, &draft, None))
        .await
        .expect("must not be an error");
    assert_eq!(report, None);
}

/// The key refresh (`design/07` §2, PROGRESS "会话链"): mail cached before the `thread_key` column
/// existed has no key, no future sync will fill it (an incremental sync asks only for UIDs above the
/// cursor), and the rows the user scrolls into are exactly the ones that need it.
#[tokio::test]
async fn the_key_refresh_walks_back_and_fills_what_the_sync_will_never_touch() {
    let (addr, _log) = spawn_imap(true).await;
    let cfg = account(addr, addr);
    let cache = Cache::open_in_memory().expect("cache");
    let mut client = within(ImapClient::connect(&cfg, "app-password"))
        .await
        .expect("connect");

    // Stand in for rows written before the column existed: same messages, no key.
    let primed = within(client.fetch_newest_headers("INBOX", 4))
        .await
        .expect("prime");
    let keyless: Vec<_> = primed
        .summaries
        .iter()
        .map(|summary| MessageSummary {
            thread_key: None,
            ..summary.clone()
        })
        .collect();
    cache
        .upsert_summaries(&cfg.id, "INBOX", &keyless)
        .expect("store");
    assert!(
        cache
            .list_summaries(&cfg.id, "INBOX", 10, 0)
            .expect("read")
            .iter()
            .all(|row| row.thread_key.is_none()),
        "the fixture has to start keyless, or this test proves nothing"
    );

    let filled = within(fill_thread_keys(
        &mut client,
        &cache,
        &cfg.id,
        "INBOX",
        3,
        10,
    ))
    .await
    .expect("refresh");

    assert!(
        filled > 0,
        "the headers carry Message-IDs, so keys must appear"
    );
    let rows = cache.list_summaries(&cfg.id, "INBOX", 10, 0).expect("read");
    assert_eq!(
        rows.len(),
        keyless.len(),
        "re-storing headers must not add or remove rows"
    );
    assert!(
        rows.iter().all(|row| row.thread_key.is_some()),
        "every cached row the walk reached now has its key: {:?}",
        rows.iter().map(|row| &row.thread_key).collect::<Vec<_>>()
    );
}

/// `design/05` §3.2: an OAuth2 account sends with `AUTH XOAUTH2`, not with the token as a password.
///
/// The difference is not cosmetic. Passing a bearer token through the password path produces
/// `AUTH PLAIN` with a secret in it — a line the server rejects, which then looks like "the token is
/// wrong" — so the assertion here is about the mechanism, not about "authentication happened".
#[tokio::test]
async fn an_oauth_account_authenticates_with_xoauth2_and_not_plain() {
    let (addr, _raw, _count, commands) = spawn_smtp_with_commands().await;
    let cfg = account(addr, addr);

    let mail = OutgoingMail {
        to: vec!["alice@example.com".to_string()],
        cc: vec![],
        bcc: vec![],
        subject: "OAuth 发送".to_string(),
        body_text: "用访问令牌发送。".to_string(),
        body_html: None,
        in_reply_to: None,
        references: vec![],
        attachments: vec![],
        message_id: None,
    };

    within(smtp::send_oauth(&cfg, "an-access-token", &mail))
        .await
        .expect("the fake server accepts XOAUTH2");

    let lines = commands.lock().expect("commands").clone();
    let auth = lines
        .iter()
        .find(|line| line.to_ascii_uppercase().starts_with("AUTH"))
        .unwrap_or_else(|| panic!("no AUTH line in {lines:?}"));
    assert!(
        auth.to_ascii_uppercase().starts_with("AUTH XOAUTH2"),
        "the mechanism has to be XOAUTH2, not PLAIN: {auth}"
    );
    assert!(
        !lines
            .iter()
            .any(|line| line.to_ascii_uppercase().contains("AUTH PLAIN")),
        "a bearer token must never be sent as a password: {lines:?}"
    );
    assert!(
        lines.iter().any(|line| line.starts_with("DATA")),
        "the message body is sent after authenticating: {lines:?}"
    );
}

// ===================================================================== helpers

fn account(imap: SocketAddr, smtp_addr: SocketAddr) -> AccountConfig {
    AccountConfig {
        id: "test@example.com".to_string(),
        email: "test@example.com".to_string(),
        display_name: "测试用户".to_string(),
        username: "test@example.com".to_string(),
        provider_id: "custom".to_string(),
        // Plain because the fixture is a loopback socket; TLS is covered by the reachability test.
        imap: Endpoint::new(&imap.ip().to_string(), imap.port(), Security::Plain),
        smtp: Endpoint::new(
            &smtp_addr.ip().to_string(),
            smtp_addr.port(),
            Security::Plain,
        ),
        needs_id_command: false,
        max_connections: 1,
    }
}

fn transcript(log: &Transcript) -> Vec<String> {
    log.lock().unwrap().clone()
}

// ===================================================================== tests

#[tokio::test]
async fn receive_loop_works_against_a_real_imap_exchange() {
    let (addr, log) = spawn_imap(true).await;
    let cfg = account(addr, addr);

    let mut client = within(ImapClient::connect(&cfg, "app-password"))
        .await
        .expect("the greeting handshake and LOGIN must succeed");

    // --- folders
    let mailboxes = within(client.list_mailboxes()).await.expect("LIST");
    let names: Vec<&str> = mailboxes.iter().map(|m| m.name.as_str()).collect();
    assert!(names.contains(&"INBOX"), "got {names:?}");
    // The library reports every folder and flags the ones that cannot be SELECTed; filtering is
    // the UI's job. What matters here is that `\Noselect` is actually parsed.
    let container = mailboxes
        .iter()
        .find(|m| m.name == "[Gmail]")
        .expect("the container folder must still be reported");
    assert!(
        !container.selectable,
        "\\Noselect must map to selectable: false, got {container:?}"
    );
    assert!(
        mailboxes
            .iter()
            .find(|m| m.name == "INBOX")
            .expect("INBOX")
            .selectable
    );
    assert_eq!(names[0], "INBOX", "INBOX must sort first, got {names:?}");

    // Modified UTF-7: the wire name must stay encoded (commands send it back verbatim) while
    // the display name is readable. Getting this backwards either breaks SELECT or shows
    // `&UXZO1mWHTvZZOQ-` in the folder list.
    let chinese = mailboxes
        .iter()
        .find(|m| m.name == "&UXZO1mWHTvZZOQ-")
        .expect("the encoded folder must be listed");
    assert_eq!(
        chinese.display_name, "其他文件夹",
        "the folder name must be decoded for display"
    );
    // Well-known folders are shown in the user's language. The fixture sends `\Sent` for Sent and
    // no attribute at all for INBOX — the second half is what matters, because that is how QQ
    // behaves and the English-name fallback is the only thing that can label it.
    assert_eq!(
        mailboxes
            .iter()
            .find(|m| m.name == "INBOX")
            .expect("INBOX")
            .display_name,
        "收件箱",
        "a server that sends no special-use attribute must still get a localised name"
    );
    assert_eq!(
        mailboxes
            .iter()
            .find(|m| m.name == "Sent")
            .expect("Sent")
            .display_name,
        "已发送"
    );
    assert_eq!(
        mailboxes
            .iter()
            .find(|m| m.name == "[Gmail]")
            .expect("[Gmail]")
            .display_name,
        "[Gmail]",
        "a folder with no known role keeps the name the server gave it"
    );

    // --- headers only
    let summaries = within(client.list_messages("INBOX", 10))
        .await
        .expect("UID SEARCH + UID FETCH headers");
    assert_eq!(summaries.len(), 4, "got {summaries:?}");
    assert_eq!(
        summaries[0].uid, 1,
        "UIDs must come back in ascending order"
    );
    assert_eq!(summaries[0].subject, "简单邮件");
    assert_eq!(summaries[0].from_addr.as_deref(), Some("alice@example.com"));
    assert!(summaries[0].seen, "the \\Seen flag must be read");
    assert!(!summaries[1].seen);
    // The GB2312 message: this is the assertion that would have caught a missing
    // `full_encoding` feature on mail-parser.
    assert_eq!(summaries[1].subject, "中文测试");
    assert_eq!(summaries[1].from_name.as_deref(), Some("张伟"));
    assert_eq!(summaries[2].subject, "Report");

    // --- full body of the GB2312 message
    let detail = within(client.fetch_message(2))
        .await
        .expect("UID FETCH body");
    assert_eq!(detail.uid, 2);
    assert_eq!(detail.subject, "中文测试");
    assert_eq!(
        detail.body_text.as_deref().map(str::trim),
        Some("你好"),
        "GB2312 body must decode, not mojibake"
    );
    assert_eq!(detail.message_id.as_deref(), Some("<two@163.com>"));
    // 2024-01-02 09:30:00+08:00 == 2024-01-02 01:30:00Z
    assert_eq!(detail.date, Some(1_704_159_000));

    // --- multipart with an attachment
    let with_attachment = within(client.fetch_message(3))
        .await
        .expect("UID FETCH body");
    assert_eq!(with_attachment.attachments.len(), 2);
    assert_eq!(with_attachment.attachments[0].name, "report.pdf");
    assert_eq!(with_attachment.attachments[0].mime, "application/pdf");
    assert_eq!(with_attachment.attachments[0].size, 5);
    assert_eq!(
        with_attachment.attachments[1].name, "big.bin",
        "the second attachment is the one the download test needs"
    );

    // --- HTML body with an image embedded by Content-ID
    let newsletter = within(client.fetch_message(4))
        .await
        .expect("UID FETCH body");
    assert_eq!(newsletter.subject, "Newsletter");
    assert!(
        newsletter
            .body_html
            .as_deref()
            .is_some_and(|h| h.contains("cid:logo@example.com")),
        "the fixture should reference the image by cid:"
    );
    assert_eq!(
        newsletter.inline_images.len(),
        1,
        "the embedded image must be extracted so the UI can resolve the cid: reference"
    );
    let logo = &newsletter.inline_images[0];
    assert_eq!(
        logo.content_id, "logo@example.com",
        "the Content-ID must be normalised without angle brackets, or it will never match the HTML"
    );
    assert_eq!(logo.mime, "image/png");
    // 8 bytes: 0x89 50 4E 47 0D 0A 1A 0A
    assert_eq!(logo.data_base64, "iVBORw0KGgo=");
    assert!(!logo.data_base64.is_empty());

    // The remote tracking pixel is NOT an inline image and must not be fetched or embedded.
    assert!(
        newsletter
            .inline_images
            .iter()
            .all(|i| !i.content_id.contains("tracker")),
        "remote images must never be pulled into the payload"
    );

    // --- flag / delete
    within(client.mark_seen(2, true)).await.expect("UID STORE");
    within(client.delete_message("INBOX", 2))
        .await
        .expect("delete");

    let log = transcript(&log);
    assert!(
        log.iter()
            .any(|l| l.to_ascii_uppercase().contains("+FLAGS (\\SEEN)")),
        "mark_seen must issue a UID STORE: {log:?}"
    );
    assert!(
        log.iter()
            .any(|l| l.to_ascii_uppercase().contains("UID EXPUNGE")),
        "a UIDPLUS server should get the precise UID EXPUNGE: {log:?}"
    );

    within(client.logout()).await.expect("LOGOUT");
}

#[tokio::test]
async fn delete_uses_plain_expunge_when_uidplus_is_missing() {
    let (addr, log) = spawn_imap(false).await;
    let cfg = account(addr, addr);

    let mut client = within(ImapClient::connect(&cfg, "x"))
        .await
        .expect("connect");
    within(client.delete_message("INBOX", 1))
        .await
        .expect("delete must still succeed when UIDPLUS is missing");

    let log = transcript(&log);
    let upper: Vec<String> = log.iter().map(|l| l.to_ascii_uppercase()).collect();

    assert!(
        !upper.iter().any(|l| l.contains("UID EXPUNGE")),
        "must not attempt UID EXPUNGE on a server that does not advertise it: {log:?}"
    );
    assert!(
        upper
            .iter()
            .any(|l| l.trim_end().ends_with("EXPUNGE") && !l.contains("UID EXPUNGE")),
        "should fall back to plain EXPUNGE: {log:?}"
    );
}

#[tokio::test]
async fn netease_style_login_sends_id_before_login() {
    let (addr, log) = spawn_imap(true).await;
    let mut cfg = account(addr, addr);
    cfg.needs_id_command = true;

    let client = within(ImapClient::connect(&cfg, "authorization-code"))
        .await
        .expect("connect with the ID command enabled");
    within(client.logout()).await.expect("LOGOUT");

    let log = transcript(&log);
    let id_at = log
        .iter()
        .position(|l| {
            l.split(' ')
                .nth(1)
                .is_some_and(|v| v.eq_ignore_ascii_case("ID"))
        })
        .unwrap_or_else(|| panic!("ID was never sent: {log:?}"));
    let login_at = log
        .iter()
        .position(|l| l.to_ascii_uppercase().contains("LOGIN"))
        .unwrap_or_else(|| panic!("LOGIN was never sent: {log:?}"));

    assert!(
        id_at < login_at,
        "NetEase rejects LOGIN that is not preceded by ID; got {log:?}"
    );
}

#[tokio::test]
async fn id_is_not_sent_unless_the_provider_needs_it() {
    let (addr, log) = spawn_imap(true).await;
    let cfg = account(addr, addr); // needs_id_command: false
    let client = within(ImapClient::connect(&cfg, "x"))
        .await
        .expect("connect");
    within(client.logout()).await.expect("LOGOUT");

    let log = transcript(&log);
    assert!(
        !log.iter().any(|l| l
            .split(' ')
            .nth(1)
            .is_some_and(|v| v.eq_ignore_ascii_case("ID"))),
        "ID must only be sent for providers that require it: {log:?}"
    );
}

#[tokio::test]
async fn a_rejected_login_is_classified_as_an_actionable_auth_error() {
    // A server that refuses LOGIN, to prove the error path carries usable guidance.
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            tokio::spawn(async move {
                let (read_half, mut write) = stream.into_split();
                let mut reader = BufReader::new(read_half);
                let _ = write
                    .write_all(b"* OK [CAPABILITY IMAP4rev1] ready\r\n")
                    .await;
                let mut line = String::new();
                while reader.read_line(&mut line).await.unwrap_or(0) > 0 {
                    let trimmed = line.trim_end();
                    let tag = trimmed.split(' ').next().unwrap_or("1").to_string();
                    if trimmed.to_ascii_uppercase().contains("LOGIN") {
                        let _ = write
                            .write_all(
                                format!(
                                    "{tag} NO [AUTHENTICATIONFAILED] Authentication failed\r\n"
                                )
                                .as_bytes(),
                            )
                            .await;
                    } else {
                        let _ = write.write_all(format!("{tag} OK\r\n").as_bytes()).await;
                    }
                    line.clear();
                }
            });
        }
    });

    let cfg = account(addr, addr);
    let error = within(ImapClient::connect(&cfg, "account-password"))
        .await
        .err()
        .expect("LOGIN must fail");

    let hint = error
        .user_hint()
        .expect("an auth failure must carry a hint");
    assert!(
        hint.contains("授权码") || hint.contains("应用专用密码"),
        "the hint should point at the authorization code, got: {hint}"
    );
}

#[tokio::test]
async fn smtp_send_carries_an_attachment_the_recipient_can_open() {
    let (addr, captured) = spawn_smtp().await;
    let cfg = account(addr, addr);

    // A real file on disk, because that is the contract: the message model carries a path and
    // `smtp` reads it, so a large attachment never travels through the IPC boundary as JSON.
    let path = std::env::temp_dir().join(format!(
        "easy-email-loopback-attachment-{}-{:?}.pdf",
        std::process::id(),
        std::thread::current().id()
    ));
    let contents = b"%PDF-1.4 loopback attachment";
    std::fs::write(&path, contents).expect("write fixture");

    let mail = OutgoingMail {
        to: vec!["alice@example.com".to_string()],
        cc: vec![],
        bcc: vec![],
        subject: "带附件的邮件".to_string(),
        body_text: "见附件。".to_string(),
        body_html: None,
        in_reply_to: None,
        references: vec![],
        attachments: vec![OutgoingAttachment {
            name: "报表.pdf".to_string(),
            mime: "application/pdf".to_string(),
            path: path.to_string_lossy().to_string(),
            size: contents.len() as u64,
            sha256: None,
        }],
        message_id: None,
    };

    within(smtp::send(&cfg, "authorization-code", &mail))
        .await
        .expect("SMTP send with an attachment must succeed");

    let bytes = captured.lock().unwrap().clone();
    let raw = String::from_utf8_lossy(&bytes);

    assert!(
        raw.contains("multipart/mixed"),
        "attachments require multipart/mixed:\n{raw}"
    );
    assert!(
        raw.contains("Content-Disposition: attachment"),
        "the part must be marked as an attachment:\n{raw}"
    );
    assert!(
        raw.contains("utf-8''%E6%8A%A5%E8%A1%A8.pdf"),
        "报表.pdf must be RFC 2231 encoded in the filename:\n{raw}"
    );
    assert!(
        raw.contains("Content-Type: application/pdf"),
        "the declared type must reach the recipient:\n{raw}"
    );
    assert!(
        raw.contains("%PDF-1.4 loopback attachment"),
        "the file's bytes must be in the delivered message:\n{raw}"
    );
    // The body has to survive alongside the attachment, or the recipient gets a file with no words.
    assert!(
        raw.contains("Content-Type: text/plain"),
        "the body must still be a part of the message:\n{raw}"
    );

    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn smtp_send_delivers_a_well_formed_message() {
    let (addr, captured) = spawn_smtp().await;
    let cfg = account(addr, addr);

    let mail = OutgoingMail {
        to: vec!["Alice <alice@example.com>".to_string()],
        cc: vec!["bob@example.com".to_string()],
        bcc: vec![],
        subject: "回复：测试".to_string(),
        body_text: "这是正文。".to_string(),
        body_html: Some("<p>这是正文。</p>".to_string()),
        in_reply_to: Some("<parent@example.com>".to_string()),
        references: vec![
            "<root@example.com>".to_string(),
            "<parent@example.com>".to_string(),
        ],
        attachments: vec![],
        message_id: None,
    };

    within(smtp::send(&cfg, "authorization-code", &mail))
        .await
        .expect("SMTP send must succeed against the loopback server");

    let bytes = captured.lock().unwrap().clone();
    assert!(!bytes.is_empty(), "the server received no DATA");
    let raw = String::from_utf8_lossy(&bytes);

    assert!(raw.contains("From: "), "missing From:\n{raw}");
    assert!(raw.contains("test@example.com"), "missing sender:\n{raw}");
    assert!(raw.contains("To: "), "missing To:\n{raw}");
    assert!(
        raw.contains("alice@example.com"),
        "the named recipient must survive parsing:\n{raw}"
    );
    assert!(raw.contains("bob@example.com"), "missing Cc:\n{raw}");
    assert!(raw.contains("Subject:"), "missing Subject:\n{raw}");
    assert!(
        raw.contains("multipart/alternative"),
        "an HTML body must produce multipart/alternative:\n{raw}"
    );
    // Threading: without these the reply starts a new conversation in the recipient's client.
    assert!(raw.contains("In-Reply-To:"), "missing In-Reply-To:\n{raw}");
    assert!(
        raw.contains("parent@example.com"),
        "missing In-Reply-To value:\n{raw}"
    );
    assert!(raw.contains("References:"), "missing References:\n{raw}");
    assert!(
        raw.contains("root@example.com"),
        "the References chain is truncated:\n{raw}"
    );
}

#[tokio::test]
async fn verify_account_skips_smtp_when_imap_authentication_fails() {
    let imap = spawn_imap_rejecting_login().await;
    let (smtp, _captured, smtp_connections) = spawn_smtp_counted().await;
    let cfg = account(imap, smtp);

    let report = within(mail_core::verify_account(&cfg, "wrong-code")).await;

    assert!(!report.imap_ok, "{}", report.imap_detail);
    assert_eq!(
        smtp_connections.load(Ordering::SeqCst),
        0,
        "a failed IMAP login must not also burn an SMTP authentication attempt — QQ rate-limits \
         logins, and the same credential cannot possibly succeed on the other service"
    );
    assert!(
        report.smtp_detail.contains("跳过"),
        "the report must explain why SMTP was skipped, got: {}",
        report.smtp_detail
    );
}

#[tokio::test]
async fn verify_account_does_probe_smtp_when_imap_authentication_succeeds() {
    let (imap, _log) = spawn_imap(true).await;
    let (smtp, _captured, smtp_connections) = spawn_smtp_counted().await;
    let cfg = account(imap, smtp);

    let report = within(mail_core::verify_account(&cfg, "app-password")).await;

    assert!(report.imap_ok, "{}", report.imap_detail);
    assert!(!report.mailboxes.is_empty(), "mailboxes should be reported");
    assert!(report.smtp_ok, "{}", report.smtp_detail);
    assert!(
        smtp_connections.load(Ordering::SeqCst) >= 1,
        "SMTP must actually be probed when the credential is good"
    );
}

#[tokio::test]
async fn smtp_test_connection_authenticates_without_sending() {
    let (addr, captured) = spawn_smtp().await;
    let cfg = account(addr, addr);

    within(smtp::test_connection(&cfg, "authorization-code"))
        .await
        .expect("test_connection must succeed");

    assert!(
        captured.lock().unwrap().is_empty(),
        "a connection test must not deliver a message"
    );
}

#[tokio::test]
async fn a_bad_smtp_password_returns_an_actionable_error() {
    // A server that rejects AUTH, to prove the SMTP error path is classified too.
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            tokio::spawn(async move {
                let (read_half, mut write) = stream.into_split();
                let mut reader = BufReader::new(read_half);
                let _ = write.write_all(b"220 bad.test ESMTP\r\n").await;
                let mut line = String::new();
                while reader.read_line(&mut line).await.unwrap_or(0) > 0 {
                    let upper = line.to_ascii_uppercase();
                    if upper.starts_with("EHLO") {
                        let _ = write
                            .write_all(b"250-bad.test\r\n250-AUTH PLAIN LOGIN\r\n250 SIZE 1000\r\n")
                            .await;
                    } else if upper.starts_with("AUTH PLAIN") {
                        if line.split_whitespace().count() < 3 {
                            let _ = write.write_all(b"334 \r\n").await;
                            line.clear();
                            let _ = reader.read_line(&mut line).await;
                        }
                        let _ = write
                            .write_all(b"535 5.7.8 Username and Password not accepted\r\n")
                            .await;
                    } else if upper.starts_with("QUIT") {
                        let _ = write.write_all(b"221 Bye\r\n").await;
                        return;
                    } else {
                        let _ = write.write_all(b"250 OK\r\n").await;
                    }
                    line.clear();
                }
            });
        }
    });

    let cfg = account(addr, addr);
    let error = within(smtp::test_connection(&cfg, "wrong"))
        .await
        .expect_err("AUTH must fail");

    assert!(
        error.user_hint().is_some(),
        "an SMTP auth rejection must carry a hint too, got: {error}"
    );
}

// ===================================================================== local cache + sync

fn inbox() -> MailboxInfo {
    MailboxInfo {
        name: "INBOX".to_string(),
        display_name: "收件箱".to_string(),
        delimiter: Some("/".to_string()),
        selectable: true,
    }
}

/// Drive one sync exactly the way `src-tauri` does: plan from the cache, await the fetch, absorb
/// into the cache, and loop when the server renumbered the folder.
///
/// The composition is duplicated deliberately rather than shared. The phase functions are the
/// tested core; an `async fn` that took `&Cache` would not be `Send`, because the SQLite connection
/// is `!Sync`, so the app cannot call a shared helper. Keeping the two shaped identically is what
/// makes this test cover the shipped path — if `refresh_folder` ever stops looping, this is the only
/// place that would notice.
async fn sync_once(
    client: &mut ImapClient,
    cache: &Cache,
    account_id: &str,
    folder: &MailboxInfo,
) -> SyncOutcome {
    let mut driver = SyncDriver::start(cache, account_id, folder).expect("plan");
    loop {
        let page = within(driver.execute(client)).await.expect("fetch");
        if let Some(outcome) = driver.absorb(cache, &page).expect("absorb") {
            return outcome;
        }
    }
}

#[tokio::test]
async fn syncing_a_folder_twice_only_fetches_what_is_new() {
    let (addr, _log) = spawn_imap(true).await;
    let cfg = account(addr, addr);
    let mut client = within(ImapClient::connect(&cfg, "app-password"))
        .await
        .expect("connect");
    let cache = Cache::open_in_memory().expect("cache");

    let first = within(sync_once(&mut client, &cache, &cfg.id, &inbox())).await;
    assert_eq!(first.fetched, 4, "a cold cache takes the newest window");
    assert_eq!(first.total_cached, 4);
    assert!(!first.uidvalidity_changed);
    assert_eq!(first.last_uid, 4, "the cursor is the highest stored UID");

    let second = within(sync_once(&mut client, &cache, &cfg.id, &inbox())).await;
    assert_eq!(second.fetched, 0, "nothing new must mean nothing fetched");
    assert_eq!(
        second.total_cached, 4,
        "and nothing re-inserted — the fixture answers `UID FETCH 5:*` with UID 4, because RFC 3501 \
         ranges are unordered, so a sync that trusted the server's range would duplicate it"
    );
}

/// **M0-5 regression.** A burst bigger than one sync batch must not leave a hole.
///
/// The rule under test is subtle and the failure it prevents is silent: the incremental cursor
/// advances to the highest UID that was *stored*, so if a page kept the **newest** entries of a
/// too-large batch, every UID between the old cursor and the first stored UID would sit below the
/// cursor forever. No error, no empty folder — just mail that never appears.
///
/// The fixture grows by more than `SYNC_BATCH` (500) at once, which is what a laptop that was
/// offline for a week, a freshly-subscribed big label, or a mail-archive copy looks like.
#[tokio::test]
async fn a_burst_larger_than_one_batch_leaves_no_hole() {
    let extra = Arc::new(AtomicU32::new(0));
    let (addr, _log) = spawn_server(Server {
        uidplus: true,
        extra: Arc::clone(&extra),
        ..Server::default()
    })
    .await;
    let cfg = account(addr, addr);
    let mut client = within(ImapClient::connect(&cfg, "app-password"))
        .await
        .expect("connect");
    let cache = Cache::open_in_memory().expect("cache");

    // Cold: the four fixture messages.
    let first = within(sync_once(&mut client, &cache, &cfg.id, &inbox())).await;
    assert_eq!(first.total_cached, 4);
    assert_eq!(first.last_uid, 4);

    // 600 messages arrive at once — more than the 500-message page the driver asks for.
    let burst = 600;
    extra.store(burst, Ordering::SeqCst);

    let second = within(sync_once(&mut client, &cache, &cfg.id, &inbox())).await;
    assert_eq!(
        second.fetched, 500,
        "one pass may only store one page, so the rest waits for the next pass"
    );
    assert_eq!(
        second.last_uid, 504,
        "the cursor stops at the last UID of the OLDEST page (4 + 500), not at the newest message"
    );

    // The stored set must be contiguous from 1: a hole here is permanently unfetchable mail.
    let uids = cached_uids(&cache, &cfg.id);
    let expected: Vec<u32> = (1..=504).collect();
    assert_eq!(
        uids, expected,
        "UIDs must be 1..=504 with no hole — a gap means the page kept the newest entries"
    );

    // The next pass continues exactly where the previous one stopped.
    let third = within(sync_once(&mut client, &cache, &cfg.id, &inbox())).await;
    assert_eq!(third.fetched, 100, "the remaining messages of the burst");
    assert_eq!(third.last_uid, 4 + burst);
    let uids = cached_uids(&cache, &cfg.id);
    let expected: Vec<u32> = (1..=4 + burst).collect();
    assert_eq!(uids, expected, "after two passes the whole burst is cached");

    assert_eq!(
        cache
            .folder_state(&cfg.id, "INBOX")
            .expect("state")
            .map(|s| s.last_uid),
        Some(4 + burst)
    );
}

/// Every stored UID for INBOX, ascending. Reads through the public cache API on purpose: the test
/// must observe what the app would show, not an internal cursor.
fn cached_uids(cache: &Cache, account_id: &str) -> Vec<u32> {
    let mut uids: Vec<u32> = cache
        .list_summaries(account_id, "INBOX", u32::MAX, 0)
        .expect("summaries")
        .into_iter()
        .map(|m| m.uid)
        .collect();
    uids.sort_unstable();
    uids
}

/// **M1a history backfill.** Loading older mail must page backwards through the mailbox **without
/// touching the incremental cursor**.
///
/// The fixture is grown to 104 messages and the cache is given a cursor at 100, which is what a real
/// mailbox looks like after the first window sync: the newest few are cached, everything older is
/// still on the server. The two invariants under test are the ones a mail client gets wrong:
///   * the page returned is the mail *just above* what the user is looking at (not the oldest mail),
///   * `last_uid` stays where it was, so the updater can never be pushed past unstored messages.
#[tokio::test]
async fn history_pages_backwards_without_moving_the_cursor() {
    let extra = Arc::new(AtomicU32::new(100));
    let (addr, _log) = spawn_server(Server {
        uidplus: true,
        extra: Arc::clone(&extra),
        ..Server::default()
    })
    .await;
    let cfg = account(addr, addr);
    let mut client = within(ImapClient::connect(&cfg, "app-password"))
        .await
        .expect("connect");
    let cache = Cache::open_in_memory().expect("cache");

    // A mailbox of 104, with the cache already knowing about the newest four. The folder row has to
    // exist first: `set_cursor` updates it, and a missing row would leave the driver on its cold path
    // — which takes the newest window and hides the very case under test.
    cache
        .upsert_folder(&cfg.id, &inbox(), 1)
        .expect("folder row");
    cache.set_cursor(&cfg.id, "INBOX", 1, 100).expect("cursor");

    let first = within(sync_once(&mut client, &cache, &cfg.id, &inbox())).await;
    assert_eq!(
        first.total_cached, 4,
        "only the newest four are in the window"
    );
    assert_eq!(first.last_uid, 104);
    assert_eq!(
        cache.oldest_uid(&cfg.id, "INBOX").expect("oldest"),
        Some(101),
        "the oldest cached message is where history starts"
    );

    // Page one: the ten messages immediately older than 101.
    let page = within(client.fetch_older_headers("INBOX", 101, 10))
        .await
        .expect("history page");
    let uids: Vec<u32> = page.iter().map(|m| m.uid).collect();
    assert_eq!(
        uids,
        (91..=100).collect::<Vec<u32>>(),
        "history must return the mail closest to the reading position, oldest first"
    );

    // Store it the way the app does — plain upsert, no cursor involvement.
    cache
        .upsert_summaries(&cfg.id, "INBOX", &page)
        .expect("store history");
    assert_eq!(cache.summary_count(&cfg.id, "INBOX").expect("count"), 14);
    assert_eq!(
        cache.oldest_uid(&cfg.id, "INBOX").expect("oldest"),
        Some(91)
    );
    assert_eq!(
        cache
            .folder_state(&cfg.id, "INBOX")
            .expect("state")
            .map(|s| s.last_uid),
        Some(104),
        "**the incremental cursor must not move**: history is added below it, never past it"
    );

    // Page two continues from there.
    let page = within(client.fetch_older_headers("INBOX", 91, 10))
        .await
        .expect("second page");
    assert_eq!(
        page.iter().map(|m| m.uid).collect::<Vec<u32>>(),
        (81..=90).collect::<Vec<u32>>()
    );

    // Rewinding far enough reaches the bottom and says so by returning nothing.
    let bottom = within(client.fetch_older_headers("INBOX", 6, 10))
        .await
        .expect("bottom page");
    assert_eq!(
        bottom.iter().map(|m| m.uid).collect::<Vec<u32>>(),
        (1..=5).collect::<Vec<u32>>(),
        "the last page is short rather than padded"
    );
    let done = within(client.fetch_older_headers("INBOX", 1, 10))
        .await
        .expect("done");
    assert!(done.is_empty(), "there is nothing older than UID 1");
}

/// **M1a-3b: the window now decides when the walk stops.**
///
/// Same fixture as the paging test (104 messages, cursor at 100), but the walk is driven by
/// `backfill_older` instead of by hand. `Count(20)` must stop after the cache holds twenty messages —
/// **not** after reaching the bottom of the mailbox — and `Years(2)` must stop at the date boundary.
/// A window that does not change where the walk stops is a preference that does nothing.
#[tokio::test]
async fn the_sync_window_decides_where_the_walk_stops() {
    let extra = Arc::new(AtomicU32::new(100));
    let (addr, _log) = spawn_server(Server {
        uidplus: true,
        extra: Arc::clone(&extra),
        ..Server::default()
    })
    .await;
    let cfg = account(addr, addr);
    let cache = Cache::open_in_memory().expect("cache");
    cache
        .upsert_folder(&cfg.id, &inbox(), 1)
        .expect("folder row");
    cache.set_cursor(&cfg.id, "INBOX", 1, 100).expect("cursor");

    // Count(20): the newest four are cached first — a cursor without rows would leave nothing to
    // walk back from, which is exactly what `oldest_uid` returning `None` means.
    {
        let mut client = within(ImapClient::connect(&cfg, "app-password"))
            .await
            .expect("connect");
        let first = within(sync_once(&mut client, &cache, &cfg.id, &inbox())).await;
        assert_eq!(first.total_cached, 4);
        assert_eq!(first.last_uid, 104);
        let report = within(backfill_older(
            &mut client,
            &cache,
            &cfg.id,
            "INBOX",
            SyncWindow::Count(20),
            BackfillBudget::generous(),
            NOW,
        ))
        .await
        .expect("backfill");
        assert_eq!(
            report.stop,
            BackfillStop::WindowSatisfied,
            "twenty messages is the window, so the walk stops there"
        );
        assert_eq!(cache.summary_count(&cfg.id, "INBOX").expect("count"), 20);
        assert_eq!(
            cache.oldest_uid(&cfg.id, "INBOX").expect("oldest"),
            Some(85),
            "UIDs 85..=104: the count bound, not the bottom of the mailbox"
        );
    }

    // Years(2) on the same fixture: every message is dated 2024-01-01, which is outside a two-year
    // window from NOW, so the very first page crosses the boundary and the walk stops immediately.
    {
        // The same 104-message mailbox, so the page below the cursor is real: a four-message fixture
        // would make "one page crosses the boundary" vacuously true.
        let extra2 = Arc::new(AtomicU32::new(100));
        let (addr2, _log2) = spawn_server(Server {
            uidplus: true,
            extra: Arc::clone(&extra2),
            ..Server::default()
        })
        .await;
        let cfg2 = account(addr2, addr2);
        let cache2 = Cache::open_in_memory().expect("cache");
        cache2
            .upsert_folder(&cfg2.id, &inbox(), 1)
            .expect("folder row");
        cache2.set_cursor(&cfg2.id, "INBOX", 1, 3).expect("cursor");
        let mut client = within(ImapClient::connect(&cfg2, "app-password"))
            .await
            .expect("connect");
        let first = within(sync_once(&mut client, &cache2, &cfg2.id, &inbox())).await;
        assert_eq!(
            first.total_cached, 101,
            "everything from 4 upwards, so the walk starts at UID 3"
        );

        let report = within(backfill_older(
            &mut client,
            &cache2,
            &cfg2.id,
            "INBOX",
            SyncWindow::Years(2),
            BackfillBudget::generous(),
            NOW,
        ))
        .await
        .expect("backfill");
        assert_eq!(
            report.stop,
            BackfillStop::WindowSatisfied,
            "crossing the two-year boundary ends the walk"
        );
        assert_eq!(report.pages, 1, "one page was enough to cross it");
    }
}

/// Fixed "now" for the window tests: 2026-10-03, so the fixture's 2024 dates are outside Years(2).
const NOW: i64 = 1_793_000_000;

#[tokio::test]
async fn a_uidvalidity_change_wipes_the_folder_and_refills_it() {
    let uidvalidity = Arc::new(AtomicU32::new(1));
    let (addr, _log) = spawn_server(Server {
        uidplus: true,
        uidvalidity: Arc::clone(&uidvalidity),
        ..Server::default()
    })
    .await;
    let cfg = account(addr, addr);
    let mut client = within(ImapClient::connect(&cfg, "app-password"))
        .await
        .expect("connect");
    let cache = Cache::open_in_memory().expect("cache");

    let first = within(sync_once(&mut client, &cache, &cfg.id, &inbox())).await;
    assert_eq!(first.total_cached, 4);

    // The server renumbered the folder. Every stored UID may now refer to a different message, so
    // showing the cached headers would attribute the wrong subject and sender to the wrong mail.
    uidvalidity.store(2, Ordering::SeqCst);

    let after = within(sync_once(&mut client, &cache, &cfg.id, &inbox())).await;
    assert!(
        after.uidvalidity_changed,
        "the generation change must be seen"
    );
    assert_eq!(
        after.fetched, 4,
        "everything is re-read under the new generation"
    );
    assert_eq!(after.total_cached, 4, "refilled, not left empty");
    assert_eq!(
        cache
            .folder_state(&cfg.id, "INBOX")
            .expect("state")
            .map(|state| state.uidvalidity),
        Some(2),
        "the new generation must be recorded, or every later sync would wipe again"
    );
}

#[tokio::test]
async fn cached_mail_survives_a_restart_and_reads_without_the_network() {
    let (addr, _log) = spawn_imap(true).await;
    let cfg = account(addr, addr);
    let path = std::env::temp_dir().join(format!(
        "easy-email-loopback-cache-{}-{:?}.db",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_file(&path);

    {
        let mut client = within(ImapClient::connect(&cfg, "app-password"))
            .await
            .expect("connect");
        let cache = Cache::open(&path).expect("open cache");
        within(sync_once(&mut client, &cache, &cfg.id, &inbox())).await;
        // Reading one message is what puts a body in the cache.
        let detail = within(client.fetch_message(1)).await.expect("fetch body");
        cache
            .put_detail(&cfg.id, "INBOX", &detail)
            .expect("cache body");
    } // both the client and the cache are dropped: this is "the app was closed"

    // Reopened with no client in scope — nothing below can touch the network, which is the whole
    // point: this is the offline path.
    let cache = Cache::open(&path).expect("reopen cache");
    let summaries = cache.list_summaries(&cfg.id, "INBOX", 50, 0).expect("list");
    assert_eq!(
        summaries.len(),
        4,
        "the message list must come back from disk"
    );
    assert!(summaries.iter().any(|m| m.subject == "中文测试"));
    assert!(
        cache
            .get_detail(&cfg.id, "INBOX", 1)
            .expect("read")
            .is_some(),
        "and so must a body that was read before the restart"
    );

    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn search_covers_headers_immediately_and_bodies_once_read() {
    let (addr, _log) = spawn_imap(true).await;
    let cfg = account(addr, addr);
    let mut client = within(ImapClient::connect(&cfg, "app-password"))
        .await
        .expect("connect");
    let cache = Cache::open_in_memory().expect("cache");
    within(sync_once(&mut client, &cache, &cfg.id, &inbox())).await;

    // Headers are indexed by the sync alone. This is the two-character CJK case that the FTS5
    // `trigram` tokenizer cannot answer.
    assert_eq!(
        cache.search(&cfg.id, "中文测试", 10).expect("search").len(),
        1
    );

    // Message 3's body is "See attached." — a phrase that appears in no header on this fixture.
    assert!(
        cache
            .search(&cfg.id, "attached", 10)
            .expect("search")
            .is_empty(),
        "a body that has never been fetched cannot be searchable"
    );

    let detail = within(client.fetch_message(3)).await.expect("fetch body");
    cache.put_detail(&cfg.id, "INBOX", &detail).expect("cache");

    let hits = cache.search(&cfg.id, "attached", 10).expect("search");
    assert_eq!(hits.len(), 1, "once read, the body becomes searchable");
    assert_eq!(hits[0].uid, 3);
    assert_eq!(hits[0].folder, "INBOX", "the hit must say where to open it");
}

// ===================================================================== connection pool

/// How many times the client logged in, i.e. how many connections were really opened.
fn logins(log: &Transcript) -> usize {
    transcript(log)
        .iter()
        .filter(|line| {
            line.split(' ')
                .nth(1)
                .is_some_and(|verb| verb.eq_ignore_ascii_case("LOGIN"))
        })
        .count()
}

fn saw_command(log: &Transcript, command: &str) -> bool {
    transcript(log).iter().any(|line| {
        line.split(' ')
            .nth(1)
            .is_some_and(|verb| verb.eq_ignore_ascii_case(command))
    })
}

#[tokio::test]
async fn the_pool_reuses_an_idle_connection_instead_of_handshaking_again() {
    let (addr, log) = spawn_imap(true).await;
    let cfg = account(addr, addr);
    let pool = ConnectionPool::new();

    {
        let mut client = within(pool.acquire(&cfg, "app-password"))
            .await
            .expect("first acquire");
        client.list_mailboxes().await.expect("use it");
    } // dropped, so it goes back to the pool

    assert_eq!(pool.live_count(&cfg.id), 1);

    {
        let mut client = within(pool.acquire(&cfg, "app-password"))
            .await
            .expect("second acquire");
        client.list_mailboxes().await.expect("reuse it");
    }

    assert_eq!(
        logins(&log),
        1,
        "the second acquire must not have opened another connection"
    );
    assert!(
        saw_command(&log, "NOOP"),
        "a pooled connection has to be probed before it is handed out"
    );
}

#[tokio::test]
async fn a_connection_the_server_closed_is_recovered_on_a_fresh_one() {
    let close_on_noop = Arc::new(AtomicBool::new(false));
    let (addr, log) = spawn_server(Server {
        uidplus: true,
        close_on_noop: Arc::clone(&close_on_noop),
        ..Server::default()
    })
    .await;
    let cfg = account(addr, addr);
    let pool = ConnectionPool::new();

    {
        let mut client = within(pool.acquire(&cfg, "app-password"))
            .await
            .expect("first acquire");
        client.list_mailboxes().await.expect("use it");
    }
    assert_eq!(logins(&log), 1);

    // The server hangs up on the now-idle connection without telling the client. This is the case
    // the probe exists for: handing it back would make the next command fail for no visible reason.
    close_on_noop.store(true, Ordering::SeqCst);

    let client = within(pool.acquire(&cfg, "app-password"))
        .await
        .expect("second acquire");

    // The probe alone does NOT catch this: the transcript above shows `NOOP` was sent and answered
    // to `Ok`, because a write to a just-closed socket can still succeed and the end of the stream
    // reads as an empty response. What must hold is that the caller recovers.
    assert_eq!(
        logins(&log),
        1,
        "the probe is expected to miss this; if it starts catching it, this test needs revisiting. \
         transcript: {:?}",
        transcript(&log)
    );
    drop(client);

    let mailboxes = within(async {
        let attempt: mail_core::Result<_> = pool_op!(&pool, &cfg, "app-password", |client| {
            client.list_mailboxes().await
        });
        attempt
    })
    .await
    .unwrap_or_else(|e| {
        panic!(
            "the pool must recover from a connection the server closed: {e}\ntranscript: {:?}",
            transcript(&log)
        )
    });

    assert!(!mailboxes.is_empty(), "the retry must return real data");
    assert_eq!(
        logins(&log),
        2,
        "recovery means opening a replacement connection. transcript: {:?}",
        transcript(&log)
    );
    assert_eq!(
        pool.live_count(&cfg.id),
        1,
        "and the replacement should itself be pooled"
    );
}

#[tokio::test]
async fn idle_push_reports_a_change_without_polling() {
    let (addr, log) = spawn_server(Server {
        uidplus: true,
        idle: Arc::new(Mutex::new(Idle::Announce)),
        ..Server::default()
    })
    .await;
    let cfg = account(addr, addr);
    let (tx, mut rx) = tokio::sync::mpsc::channel(8);

    let watcher = tokio::spawn(watch_every(
        cfg.clone(),
        "app-password".to_string(),
        "INBOX".to_string(),
        // The real window is 120 s; these tests only care about ordering and recovery.
        Duration::from_millis(300),
        tx,
    ));

    let event = within(async { rx.recv().await })
        .await
        .expect("the watcher must report the change");
    assert_eq!(
        event,
        PushEvent::Changed {
            mailbox: "INBOX".to_string()
        },
        "got {event:?}"
    );
    assert!(
        saw_command(&log, "IDLE"),
        "the change must have arrived over an idle connection, not a poll"
    );

    watcher.abort();
}

#[tokio::test]
async fn idle_recovers_after_the_server_drops_the_connection() {
    let (addr, _log) = spawn_server(Server {
        uidplus: true,
        idle: Arc::new(Mutex::new(Idle::AnnounceThenHangUp)),
        ..Server::default()
    })
    .await;
    let cfg = account(addr, addr);
    let (tx, mut rx) = tokio::sync::mpsc::channel(16);

    let watcher = tokio::spawn(watch_every(
        cfg.clone(),
        "app-password".to_string(),
        "INBOX".to_string(),
        // The real window is 120 s; these tests only care about ordering and recovery.
        Duration::from_millis(300),
        tx,
    ));

    // Expected order: the change arrives, the connection dies, then a reconnect is announced.
    let mut seen = Vec::new();
    for _ in 0..3 {
        match within(async { rx.recv().await }).await {
            Some(event) => seen.push(event),
            None => break,
        }
    }
    watcher.abort();

    // Order matters, and it is a consequence of the design: a change seen while the connection was
    // dying cannot be verified, so the failure comes first, the reconnect is announced only once the
    // new connection has selected the mailbox, and the message that arrived meanwhile is reported
    // after that — against the size remembered from before the drop.
    assert!(
        matches!(seen.first(), Some(PushEvent::Failed { .. })),
        "the drop must be reported rather than swallowed. got {seen:?}"
    );
    assert!(
        matches!(seen.get(1), Some(PushEvent::Reconnected { attempt, .. }) if *attempt >= 1),
        "the watcher must come back on its own. got {seen:?}"
    );
    assert!(
        matches!(seen.get(2), Some(PushEvent::Changed { .. })),
        "and the message that arrived meanwhile must still be reported. got {seen:?}"
    );
}

#[tokio::test]
async fn an_attachment_is_saved_with_its_content_intact() {
    let (addr, log) = spawn_imap(true).await;
    let cfg = account(addr, addr);
    let mut client = within(ImapClient::connect(&cfg, "app-password"))
        .await
        .expect("connect");
    client.select("INBOX").await.expect("select");

    let detail = within(client.fetch_message(3)).await.expect("fetch body");
    let big = detail
        .attachments
        .iter()
        .find(|a| a.name == "big.bin")
        .expect("the large attachment must be listed");
    assert_eq!(
        big.size, BIG_ATTACHMENT_BYTES,
        "the parsed size must be the decoded size, because that is what ends up in the file"
    );

    let dest = std::env::temp_dir().join(format!(
        "easy-email-attachment-{}-{:?}.bin",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_file(&dest);

    let written = within(client.save_attachment(3, big.part, &dest))
        .await
        .expect("save");
    assert_eq!(written, BIG_ATTACHMENT_BYTES as u64);

    let saved = std::fs::read(&dest).expect("read back");
    assert_eq!(saved.len(), BIG_ATTACHMENT_BYTES);
    assert!(
        saved
            .iter()
            .all(|byte| *byte == b'A' || *byte == b'B' || *byte == b'C'),
        "the decoded content must be the ABC pattern the fixture encoded, base64 lines unwrapped"
    );

    // The size the message declared and the size on disk agreeing is the whole point of parsing the
    // message rather than slicing bytes: a wrong range or a broken decoder would show up as a
    // mismatch, not as a plausible-looking file.
    assert_eq!(written, big.size as u64);

    // And exactly one message fetch: the download is deliberately *not* a partial fetch, because
    // those responses cannot be parsed. See `ImapClient::fetch_message_range`.
    let partial = transcript(&log)
        .iter()
        .filter(|line| line.contains("[]<"))
        .count();
    assert_eq!(
        partial,
        0,
        "no partial fetch may be attempted. transcript: {:?}",
        transcript(&log)
    );

    let _ = std::fs::remove_file(&dest);
}

#[tokio::test]
async fn saving_a_part_that_does_not_exist_fails_instead_of_writing_junk() {
    let (addr, _log) = spawn_imap(true).await;
    let cfg = account(addr, addr);
    let mut client = within(ImapClient::connect(&cfg, "app-password"))
        .await
        .expect("connect");
    client.select("INBOX").await.expect("select");

    let dest = std::env::temp_dir().join(format!(
        "easy-email-attachment-missing-{}-{:?}.bin",
        std::process::id(),
        std::thread::current().id()
    ));
    let error = within(client.save_attachment(3, 99, &dest))
        .await
        .expect_err("a part that does not exist must not produce a file");
    assert!(
        error.to_string().contains("99"),
        "the error should name the part, got: {error}"
    );
    assert!(!dest.exists(), "and nothing may be left behind");
}

#[tokio::test]
async fn the_pool_keeps_at_most_max_connections_idle() {
    let (addr, _log) = spawn_imap(true).await;
    let cfg = account(addr, addr);
    // The fixture models the QQ-family limit, which is also what makes this worth enforcing.
    assert_eq!(cfg.max_connections, 1);
    let pool = ConnectionPool::new();

    // Check several out at once, since returned ones would just be re-taken by the next acquire.
    let mut held = Vec::new();
    for attempt in 0..3 {
        held.push(
            within(pool.acquire(&cfg, "app-password"))
                .await
                .unwrap_or_else(|e| panic!("acquire {attempt}: {e}")),
        );
    }
    assert_eq!(
        pool.live_count(&cfg.id),
        0,
        "checked-out connections are not idle"
    );

    drop(held);

    assert_eq!(
        pool.live_count(&cfg.id),
        1,
        "idle connections must be capped at max_connections"
    );
}

// ===================================================================== 库能力探针（M0-1）
//
// These do not test our code — they pin **library facts** that the design depends on, so that a
// dependency upgrade cannot quietly invalidate a paragraph in design/ without a red test.
//
// They live in this file (rather than a separate `library_probe.rs`) because they need the same
// fake servers; a second copy of that fixture would be a second thing to keep faithful.
// Each probe names the A1 finding it corresponds to.

/// **A1-F41** — a pushed `* n EXISTS` reaches us as `IdleResponse::NewData`, *not* only when the
/// idle window closes.
///
/// The window here is deliberately long (5 s): if the `HANDOFF` claim ("async-imap never delivers
/// EXISTS, so push latency is the window") were still true, the event could not arrive before it.
#[tokio::test]
async fn probe_idle_delivers_exists_before_the_window_closes() {
    let (addr, log) = spawn_server(Server {
        uidplus: true,
        idle: Arc::new(Mutex::new(Idle::Announce)),
        ..Server::default()
    })
    .await;
    let cfg = account(addr, addr);
    let (tx, mut rx) = tokio::sync::mpsc::channel(8);

    let started = std::time::Instant::now();
    let watcher = tokio::spawn(watch_every(
        cfg.clone(),
        "app-password".to_string(),
        "INBOX".to_string(),
        Duration::from_secs(5),
        tx,
    ));

    let event = within(async { rx.recv().await })
        .await
        .expect("the pushed EXISTS must produce an event");
    let elapsed = started.elapsed();
    watcher.abort();

    assert_eq!(
        event,
        PushEvent::Changed {
            mailbox: "INBOX".to_string()
        },
        "a pushed EXISTS must be reported as a change"
    );
    assert!(
        saw_command(&log, "IDLE"),
        "the change must have come from the idle connection"
    );
    assert!(
        elapsed < Duration::from_secs(2),
        "the event must arrive from the push, not from the 5 s window closing (took {elapsed:?})"
    );
}

/// **A1-F43** — `APPEND` gives us no UID, even when the server sends `APPENDUID`.
///
/// The fixture answers with `[APPENDUID 1 4242]`. `ImapClient::append` returns `Result<()>`, so
/// there is no way to read 4242 from it — the probe records that, because the design's
/// "verify-or-append into Sent" and "upload an archive to the server" flows both depend on finding
/// the UID again with `UID SEARCH HEADER Message-ID`.
#[tokio::test]
async fn probe_append_uid_is_parsed_and_then_discarded() {
    let (addr, log) = spawn_imap(true).await;
    let cfg = account(addr, addr);
    let mut client = within(ImapClient::connect(&cfg, "app-password"))
        .await
        .expect("connect");

    let raw = b"From: me@example.com\r\nTo: you@example.com\r\nSubject: hi\r\nMessage-ID: <probe@example.com>\r\n\r\nbody\r\n";
    within(client.append("Sent", raw, true))
        .await
        .expect("APPEND must succeed against a UIDPLUS server");

    // The server answered `[APPENDUID 1 4242]`; `append` returns `()`, so that number is gone.
    // The only way back to it is the search — which is why the design makes this primitive
    // mandatory before and after every append.
    let found = within(client.find_uid_by_message_id("Sent", "<probe@example.com>"))
        .await
        .expect("the search must run");
    assert_eq!(
        found,
        Some(4242),
        "the UID must be recoverable by Message-ID, since APPEND does not return it"
    );

    let transcript = log.lock().unwrap().clone();
    assert!(
        transcript.iter().any(|line| line.contains("APPEND")),
        "the client must have sent APPEND: {transcript:?}"
    );
    assert!(
        transcript.iter().any(|line| line
            .to_ascii_uppercase()
            .contains("UID SEARCH HEADER MESSAGE-ID")),
        "and must have looked the UID up afterwards: {transcript:?}"
    );
}

/// **A1-F42** — no partial fetch is possible before `imap-proto` is patched, so the client refuses.
///
/// `imap-proto` 0.17's `msg_att_body_section` parses `BODY [section] <number> " " nstring` and never
/// accepts the `<origin.offset>` form a server uses to answer a partial fetch — **not even `<0.10>`**
/// (measured here, not inferred). A parse failure makes async-imap close the connection, so the guard
/// exists to turn a session-killing landmine into an ordinary error.
///
/// This test is the written record of what has to change: when the three-line `imap-proto` patch
/// (design/19 §4, M0-4) lands, both assertions below flip and streaming attachment downloads become
/// possible.
#[tokio::test]
async fn probe_partial_fetch_is_refused_instead_of_killing_the_connection() {
    let (addr, _log) = spawn_imap(true).await;
    let cfg = account(addr, addr);
    let mut client = within(ImapClient::connect(&cfg, "app-password"))
        .await
        .expect("connect");

    // Both shapes are refused: a non-zero offset, and a zero origin with a length (which a server
    // still echoes as `<0.10>`).
    for start in [5u32, 0u32] {
        match within(client.fetch_message_range(1, start, 10)).await {
            Ok(_) => panic!(
                "a partial fetch (start={start}) must not be attempted: the reply would fail to \
                 parse and the connection would be closed"
            ),
            Err(error) => {
                let message = error.to_string();
                assert!(
                    message.contains("分部取件"),
                    "the error must be actionable and name the limitation, got: {message}"
                );
            }
        }
    }

    // The whole point of refusing: the session survives, which a server-side parse error would not
    // have allowed.
    let mailboxes = within(client.list_mailboxes())
        .await
        .expect("the connection must still be usable after the refusal");
    assert!(
        mailboxes.iter().any(|m| m.name == "INBOX"),
        "the connection must still list folders"
    );
}

/// **A1-F46** — `mail-parser`'s `references()` has three shapes, and `as_text()` loses all but the
/// last id when the header carried several.
#[test]
fn probe_references_has_three_shapes() {
    use mail_parser::MessageParser;

    // **This probe corrected a research note.** design/A1-F46 claimed that a single-reference
    // header comes back as `Text` and that `as_text_list()` therefore returns `None`. Measured on
    // mail-parser 0.11.9, that is false: `as_text_list()` answers for BOTH variants (one element
    // for `Text`), and the real trap is the other accessor — `as_text()` on a multi-id header
    // silently returns only the **last** id. Practical rule: read references with
    // `as_text_list()`, never with `as_text()`.
    //
    // Each case keeps its own buffer alive: `Message` borrows the bytes it was parsed from.
    {
        let raw =
            b"From: a@example.com\r\nReferences: <a@example.com>\r\nSubject: s\r\n\r\nbody\r\n";
        let msg = MessageParser::default().parse(raw).expect("parse");
        let refs = msg.references();
        assert_eq!(
            refs.as_text_list().map(|l| l.len()),
            Some(1),
            "one id must still be readable as a list of one (contradicting A1-F46's original claim)"
        );
        assert_eq!(refs.as_text(), Some("a@example.com"));
    }
    {
        let raw = b"From: a@example.com\r\nReferences: <a@example.com> <b@example.com>\r\nSubject: s\r\n\r\nbody\r\n";
        let msg = MessageParser::default().parse(raw).expect("parse");
        let refs = msg.references();
        assert_eq!(refs.as_text_list().map(|l| l.len()), Some(2));
        assert_eq!(
            refs.as_text(),
            Some("b@example.com"),
            "as_text() on a multi-id header returns only the last id — that is the trap"
        );
    }
    {
        let raw = b"From: a@example.com\r\nSubject: s\r\n\r\nbody\r\n";
        let msg = MessageParser::default().parse(raw).expect("parse");
        assert!(msg.references().as_text_list().is_none());
        assert!(msg.references().as_text().is_none());
    }
}

/// **A1-F45 (corrected)** — lettre does not send `SIZE=` on `MAIL FROM`, and it cannot even read
/// the limit the server advertises.
///
/// The fixture advertises `250-SIZE 10485760`. If lettre used it, the command would appear in the
/// transcript; it does not, which is why the design says the pre-send size check is a local table
/// plus a hard cap and nothing else.
#[tokio::test]
async fn probe_lettre_never_sends_size() {
    let (addr, _captured, _connections, commands) = spawn_smtp_with_commands().await;
    let cfg = account(addr, addr);

    let mail = OutgoingMail {
        to: vec!["alice@example.com".to_string()],
        cc: vec![],
        bcc: vec![],
        subject: "size probe".to_string(),
        body_text: "hello".to_string(),
        body_html: None,
        in_reply_to: None,
        references: vec![],
        attachments: vec![],
        message_id: None,
    };

    within(smtp::send(&cfg, "authorization-code", &mail))
        .await
        .expect("send");

    let lines = commands.lock().unwrap().clone();
    let mail_from = lines
        .iter()
        .find(|line| line.to_ascii_uppercase().starts_with("MAIL FROM"))
        .expect("MAIL FROM must have been sent");

    assert!(
        !mail_from.to_ascii_uppercase().contains("SIZE="),
        "lettre must not send SIZE= (A1-F45: it has no such parameter): {mail_from}"
    );
    assert!(
        lines.iter().any(|l| l.contains("EHLO")),
        "the server advertised SIZE via EHLO; the point is that the client ignores it"
    );
}
