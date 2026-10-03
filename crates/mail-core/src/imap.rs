//! IMAP receive path.
//!
//! Design notes that are not obvious from the code:
//!
//! * `async-imap` 0.12 has no `connect()` helper (the doc examples are `ignore`d and reference a
//!   function that no longer exists). We build the transport ourselves.
//! * Under the `runtime-tokio` feature, async-imap uses *tokio's* `AsyncRead`/`AsyncWrite`, not
//!   the futures ones. Both crates default to a non-tokio runtime, hence the explicit feature
//!   flags in `Cargo.toml`.
//! * `Client<T>` requires `T: Debug`, but `async_native_tls::TlsStream` is not `Debug`. Hence
//!   [`DebugStream`].
//! * We read the server greeting explicitly. `Client::new` does not consume it, and leaving it in
//!   the buffer desynchronises the first command.
//! * NetEase servers reject `LOGIN` unless the client first sends the RFC 2971 `ID` command.

use std::fmt;
use std::future::Future;
use std::path::Path;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use async_imap::Session;
use async_imap::types::{Flag, Mailbox};
use async_native_tls::TlsConnector;
use futures_util::TryStreamExt;
use mail_parser::{MessageParser, MimeHeaders};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::TcpStream;

use crate::error::{MailError, Result};
use crate::folders::{self, Role};
use crate::model::{
    AccountConfig, AttachmentMeta, Endpoint, InlineImage, MailboxInfo, MessageDetail,
    MessageSummary, Security,
};
use crate::mutf7;

/// RFC 6154 special-use attribute → the folder role `folders` can name.
///
/// Servers that support RFC 6154 are the well-behaved ones: the attribute is language-independent,
/// so a German `Gesendet` is still recognised as Sent. Servers that do not (QQ, for one) are why
/// [`folders::label_for`] falls back to the conventional English name.
fn special_use(attributes: &[async_imap::imap_proto::NameAttribute<'_>]) -> Option<Role> {
    use async_imap::imap_proto::NameAttribute;
    attributes.iter().find_map(|attribute| match attribute {
        NameAttribute::All => Some(Role::All),
        NameAttribute::Archive => Some(Role::Archive),
        NameAttribute::Drafts => Some(Role::Drafts),
        NameAttribute::Flagged => Some(Role::Flagged),
        NameAttribute::Junk => Some(Role::Junk),
        NameAttribute::Sent => Some(Role::Sent),
        NameAttribute::Trash => Some(Role::Trash),
        _ => None,
    })
}

const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
const COMMAND_TIMEOUT: Duration = Duration::from_secs(60);

/// One page of incremental headers, with the folder generation it belongs to.
///
/// `uidvalidity` travels with the headers because the two must be used together: storing headers
/// without checking whether the generation changed is how a cache ends up showing the wrong message
/// under the wrong subject line.
#[derive(Debug, Clone)]
pub struct HeaderPage {
    /// `UIDVALIDITY` as reported by this `SELECT`.
    pub uidvalidity: u32,
    /// Headers with `uid > after_uid`, oldest first.
    pub summaries: Vec<MessageSummary>,
}

/// The `UID FETCH` set for an incremental pass.
///
/// Bounded on purpose: `UID FETCH <n>:*` asks for *everything* above the cursor, so a mailbox that
/// jumped by more messages than one batch would make the server stream headers we are about to
/// discard. With `limit > 0` we ask for exactly one batch; `limit == 0` keeps the old unbounded
/// behaviour for callers that really do want the rest of the folder.
///
/// A range whose high end is past the last message is legal — the server answers with whatever
/// exists in it (RFC 3501), which is what makes this safe on a small folder.
pub fn since_range(after_uid: u32, limit: usize) -> String {
    let low = after_uid.saturating_add(1);
    if limit == 0 {
        return format!("{low}:*");
    }
    // `low + limit - 1` cannot overflow in practice (UIDs are 32-bit and mailboxes are not 2^32
    // messages), but `saturating` keeps the type checker and a hostile server both happy.
    // Write it as `low + (limit - 1)` rather than `(low + limit) - 1`: the latter saturates the sum
    // first and then subtracts, which can produce a *reversed* range such as `MAX:MAX-1`.
    format!(
        "{low}:{}",
        low.saturating_add((limit as u32).saturating_sub(1))
    )
}

/// Trim a fetched batch to one page, **keeping the oldest entries**.
///
/// This is the rule that makes the incremental cursor safe. The cursor advances to the highest UID
/// that was stored (`sync::advance_cursor`), so if we kept the newest instead, every UID between
/// `after_uid + 1` and the first stored UID would sit *below* the cursor forever and could never be
/// fetched again — silent, permanent mail loss with no error to show the user.
///
/// `limit == 0` means "no page limit" and returns the batch untouched.
pub fn keep_oldest_page(mut summaries: Vec<MessageSummary>, limit: usize) -> Vec<MessageSummary> {
    if limit > 0 && summaries.len() > limit {
        summaries.truncate(limit);
    }
    summaries
}

/// Trim a fetched batch to one page, **keeping the newest entries**.
///
/// The mirror of [`keep_oldest_page`], and the right rule for a *history* page: when looking
/// backwards, the messages closest to what the user is reading are the ones to keep. No cursor is
/// involved, so dropping the far end costs nothing but a repeat fetch if the user keeps scrolling.
pub fn keep_newest_page(mut summaries: Vec<MessageSummary>, limit: usize) -> Vec<MessageSummary> {
    if limit > 0 && summaries.len() > limit {
        summaries = summaries.split_off(summaries.len() - limit);
    }
    summaries
}

/// Everything a transport must be, named once.
///
/// `dyn AsyncRead + AsyncWrite + Debug` is not a legal trait object — Rust allows only one
/// non-auto trait in the object type — so the combination needs a name to be boxable.
trait MailStream: AsyncRead + AsyncWrite + Unpin + Send + fmt::Debug {}

impl<T> MailStream for T where T: AsyncRead + AsyncWrite + Unpin + Send + fmt::Debug {}

/// The concrete transport we hand to async-imap.
///
/// Boxing lets SSL, STARTTLS and plaintext connections share one type instead of three generic
/// instantiations. The dynamic dispatch cost is irrelevant next to network latency.
type Transport = Box<dyn MailStream>;

/// Wraps a stream to supply the `Debug` bound async-imap requires.
///
/// `async_native_tls::TlsStream` deliberately does not implement `Debug` (it would risk leaking
/// handshake state into logs), so we supply an opaque one-shot instead.
struct DebugStream<S>(S);

impl<S> fmt::Debug for DebugStream<S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("MailTransport")
    }
}

impl<S: AsyncRead + Unpin> AsyncRead for DebugStream<S> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.0).poll_read(cx, buf)
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for DebugStream<S> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.0).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.0).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.0).poll_shutdown(cx)
    }
}

fn imap_err(e: async_imap::error::Error) -> MailError {
    // Goes through the `From` impl so the server's own sentence is unwrapped from
    // async-imap's `no response: ... info: Some("...")` rendering.
    e.into()
}

fn io_err(host: &str, port: u16) -> impl Fn(std::io::Error) -> MailError + '_ {
    move |source| MailError::Connect {
        host: host.to_string(),
        port,
        source,
    }
}

/// Apply the command timeout to any fallible future.
async fn timed<T>(fut: impl Future<Output = Result<T>>) -> Result<T> {
    match tokio::time::timeout(COMMAND_TIMEOUT, fut).await {
        Ok(r) => r,
        Err(_) => Err(MailError::Timeout(COMMAND_TIMEOUT.as_secs())),
    }
}

/// Open a TCP connection and apply TLS if the endpoint asks for it up-front.
///
/// For STARTTLS this returns the plain socket; the upgrade happens in [`ImapClient::connect`]
/// once the greeting and the `STARTTLS` command have been exchanged.
async fn open_transport(ep: &Endpoint, tls: &TlsConnector) -> Result<Transport> {
    let tcp = tokio::time::timeout(
        CONNECT_TIMEOUT,
        TcpStream::connect((ep.host.as_str(), ep.port)),
    )
    .await
    .map_err(|_| MailError::Timeout(CONNECT_TIMEOUT.as_secs()))?
    .map_err(io_err(&ep.host, ep.port))?;

    // Mail is latency-sensitive and messages are small; Nagle just adds delay.
    let _ = tcp.set_nodelay(true);

    match ep.security {
        Security::Ssl => {
            let stream =
                tls.connect(ep.host.as_str(), tcp)
                    .await
                    .map_err(|source| MailError::Tls {
                        host: ep.host.clone(),
                        source,
                    })?;
            Ok(Box::new(DebugStream(stream)))
        }
        Security::StartTls | Security::Plain => Ok(Box::new(tcp)),
    }
}

/// Build the wire format of an RFC 2971 ID command.
///
/// Values we control are safe to interpolate; there is no user input here.
fn id_command() -> String {
    format!(
        "ID (\"name\" \"Easy Email\" \"version\" \"{}\" \"vendor\" \"Easy Email\")",
        env!("CARGO_PKG_VERSION")
    )
}

/// An authenticated IMAP connection.
pub struct ImapClient {
    session: Session<Transport>,
    host: String,
    /// Whether the server advertises UIDPLUS (RFC 4315), i.e. supports `UID EXPUNGE`.
    ///
    /// Discovered once at login rather than by attempting `UID EXPUNGE` and reacting to the
    /// failure: async-imap closes the stream when a command is rejected, and a rejection can
    /// surface as a cleanly-ended stream rather than an `Err`, which would silently make delete
    /// a no-op.
    uidplus: bool,
}

impl ImapClient {
    /// Connect and authenticate.
    pub async fn connect(cfg: &AccountConfig, password: &str) -> Result<Self> {
        let ep = cfg.imap.clone();
        let tls = TlsConnector::new();

        let transport = open_transport(&ep, &tls).await?;
        let mut client = async_imap::Client::new(transport);

        // The greeting must be drained before any command is issued.
        let greeting = client
            .read_response()
            .await
            .map_err(|e| MailError::Imap(e.to_string()))?;
        if greeting.is_none() {
            return Err(MailError::Imap(format!(
                "{}:{} 没有返回 IMAP 问候语，可能不是 IMAP 端口",
                ep.host, ep.port
            )));
        }

        if ep.security == Security::StartTls {
            // This is `Connection::run_command_and_check_ok`, which takes the unsolicited-response
            // channel explicitly because there is no session yet to supply one.
            client
                .run_command_and_check_ok("STARTTLS", None)
                .await
                .map_err(|e| MailError::Imap(format!("STARTTLS 被拒绝：{e}")))?;

            let raw = client.into_inner();
            let stream =
                tls.connect(ep.host.as_str(), raw)
                    .await
                    .map_err(|source| MailError::Tls {
                        host: ep.host.clone(),
                        source,
                    })?;
            // There is no second greeting after a STARTTLS upgrade.
            client = async_imap::Client::new(Box::new(DebugStream(stream)));
        }

        // NetEase rejects the LOGIN that follows if the client has not identified itself.
        // A server that does not implement ID may answer BAD; that is legal, so we tolerate it
        // and let the login outcome speak for itself.
        if cfg.needs_id_command {
            let command = id_command();
            let _ = client.run_command_and_check_ok(&command, None).await;
        }

        // `login` takes the client by value, so the future must own everything it touches.
        let mut session = {
            let host = ep.host.clone();
            let username = cfg.username.clone();
            let password = password.to_string();
            let needs_id = cfg.needs_id_command;

            timed(async move {
                client
                    .login(&username, &password)
                    .await
                    .map_err(|(e, _)| MailError::classify_login(&host, e.to_string(), needs_id))
            })
            .await?
        };

        // Ask once, up front. Treating a missing CAPABILITY as "no UIDPLUS" is safe: the fallback
        // (plain EXPUNGE) works everywhere.
        let uidplus = session
            .capabilities()
            .await
            .map(|caps| caps.has_str("UIDPLUS"))
            .unwrap_or(false);

        Ok(Self {
            session,
            host: ep.host,
            uidplus,
        })
    }

    pub fn host(&self) -> &str {
        &self.host
    }

    /// List every mailbox the account exposes, `INBOX` first.
    pub async fn list_mailboxes(&mut self) -> Result<Vec<MailboxInfo>> {
        Ok(self
            .list_mailboxes_with_roles()
            .await?
            .into_iter()
            .map(|(mailbox, _)| mailbox)
            .collect())
    }

    /// The wire name of the folder that holds sent copies, if the server has one.
    ///
    /// Resolved through the same role logic the sidebar uses — the RFC 6154 special-use attribute
    /// first, then the conventional English names — rather than by searching for a literal `Sent`:
    /// QQ calls it `Sent Messages`, and a server may name it entirely in another language and only
    /// say so in the attribute.
    ///
    /// `None` means the mailbox has no Sent folder at all, which is not an error: the archive step
    /// then simply has nowhere to put a copy (`design/07` §5.4).
    pub async fn sent_mailbox(&mut self) -> Result<Option<String>> {
        Ok(self
            .list_mailboxes_with_roles()
            .await?
            .into_iter()
            .find(|(mailbox, role)| *role == Some(Role::Sent) && mailbox.selectable)
            .map(|(mailbox, _)| mailbox.name))
    }

    /// Every mailbox with the role it was derived from.
    ///
    /// The roles are kept here rather than on [`MailboxInfo`] because the UI has no use for the
    /// enum — but the flows that need "the Sent folder" or "the Drafts folder" do, and re-deriving
    /// a role from a display name is not possible once it has been localised.
    pub async fn list_mailboxes_with_roles(&mut self) -> Result<Vec<(MailboxInfo, Option<Role>)>> {
        let names = timed(async {
            let stream = self.session.list(None, Some("*")).await.map_err(imap_err)?;
            stream.try_collect::<Vec<_>>().await.map_err(imap_err)
        })
        .await?;

        // Keep the role alongside each mailbox so the sidebar order can use it; `MailboxInfo` only
        // carries what the UI needs, and the UI has no use for the enum itself.
        let mut out: Vec<(MailboxInfo, Option<Role>)> = names
            .into_iter()
            .map(|n| {
                let name = n.name().to_string();
                // Mailbox names arrive in IMAP modified UTF-7. `name` keeps the wire form — it is
                // the only form that may be sent back in SELECT/COPY/APPEND — while `display_name`
                // is what a human reads. `decode` is a no-op for plain ASCII names.
                let decoded = mutf7::decode(&name);
                let special = special_use(n.attributes());
                // The role that comes back is the effective one, including the English-name
                // fallback — that, not the attribute alone, is what ordering has to use.
                let labelled = folders::label_for(&decoded, n.delimiter(), special);
                (
                    MailboxInfo {
                        selectable: !n
                            .attributes()
                            .iter()
                            .any(|a| matches!(a, async_imap::imap_proto::NameAttribute::NoSelect)),
                        name,
                        display_name: labelled.label,
                        delimiter: n.delimiter().map(str::to_string),
                    },
                    labelled.role,
                )
            })
            .collect();

        out.sort_by_key(|(mailbox, role)| folders::sort_key(&mailbox.display_name, *role));
        Ok(out)
    }

    /// Select a mailbox, returning its metadata (message count, UIDVALIDITY, ...).
    pub async fn select(&mut self, mailbox: &str) -> Result<Mailbox> {
        timed(async { self.session.select(mailbox).await.map_err(imap_err) }).await
    }

    /// Fetch the newest `limit` messages in `mailbox`, headers only.
    ///
    /// UIDs are used rather than sequence numbers because sequence numbers are reassigned when
    /// other messages are expunged, which makes any cached cursor wrong.
    pub async fn list_messages(
        &mut self,
        mailbox: &str,
        limit: usize,
    ) -> Result<Vec<MessageSummary>> {
        Ok(self.fetch_newest_headers(mailbox, limit).await?.summaries)
    }

    /// The newest `limit` headers plus the folder's `UIDVALIDITY`.
    ///
    /// This is the first-sync primitive: a cache with nothing in it wants a recent window, not the
    /// whole folder. `UID SEARCH ALL` only returns UIDs, so enumerating a large folder is cheap —
    /// it is the header fetch that is scoped to `limit`.
    pub async fn fetch_newest_headers(
        &mut self,
        mailbox: &str,
        limit: usize,
    ) -> Result<HeaderPage> {
        timed(async {
            let selected = self.session.select(mailbox).await.map_err(imap_err)?;
            // A public field in async-imap, not an accessor.
            let uidvalidity = selected.uid_validity.unwrap_or(0);

            let mut uids: Vec<u32> = self
                .session
                .uid_search("ALL")
                .await
                .map_err(imap_err)?
                .into_iter()
                .collect();
            uids.sort_unstable();

            let newest: Vec<u32> = uids.iter().rev().take(limit).rev().copied().collect();
            if newest.is_empty() {
                return Ok(HeaderPage {
                    uidvalidity,
                    summaries: Vec::new(),
                });
            }
            let set = newest
                .iter()
                .map(u32::to_string)
                .collect::<Vec<_>>()
                .join(",");

            let mut stream = self
                .session
                .uid_fetch(&set, "(UID FLAGS RFC822.SIZE BODY.PEEK[HEADER])")
                .await
                .map_err(imap_err)?;

            let mut summaries = Vec::with_capacity(newest.len());
            while let Some(fetch) = stream.try_next().await.map_err(imap_err)? {
                summaries.push(summarise(&fetch));
            }
            summaries.sort_by_key(|m| m.uid);

            Ok(HeaderPage {
                uidvalidity,
                summaries,
            })
        })
        .await
    }

    /// Incremental header fetch: everything with a UID above `after_uid`, plus the folder's
    /// current `UIDVALIDITY`.
    ///
    /// This is what makes the local cache cheap to keep current. It does one `SELECT` (so the
    /// caller does not have to make a second one just to read `UIDVALIDITY`) and one `UID FETCH`.
    ///
    /// **The `after_uid` filter is not redundant.** In IMAP, `UID FETCH <n>:*` is a *range*, and a
    /// range whose start is past the end still matches the last message in the mailbox — so asking
    /// for `101:*` on a folder whose highest UID is 50 returns UID 50, not nothing. Trusting the
    /// server's range would re-insert an old message on every sync and, with a cached cursor, could
    /// keep doing so forever.
    pub async fn fetch_headers_since(
        &mut self,
        mailbox: &str,
        after_uid: u32,
        limit: usize,
    ) -> Result<HeaderPage> {
        timed(async {
            let selected = self.session.select(mailbox).await.map_err(imap_err)?;
            // A public field in async-imap, not an accessor.
            let uidvalidity = selected.uid_validity.unwrap_or(0);

            let set = since_range(after_uid, limit);
            let mut stream = self
                .session
                .uid_fetch(&set, "(UID FLAGS RFC822.SIZE BODY.PEEK[HEADER])")
                .await
                .map_err(imap_err)?;

            let mut summaries = Vec::new();
            while let Some(fetch) = stream.try_next().await.map_err(imap_err)? {
                let summary = summarise(&fetch);
                if summary.uid > after_uid {
                    summaries.push(summary);
                }
            }
            summaries.sort_by_key(|m| m.uid);

            // **Keep the OLDEST `limit` of this batch, never the newest.** The cursor only advances
            // to the highest UID we actually stored (`sync::advance_cursor`), so dropping the *old*
            // end would move the cursor past messages that were never stored and they could never be
            // fetched again — a permanent hole in the mailbox. Keeping the oldest makes the next
            // pass start exactly where this one stopped.
            //
            // History: this used to keep the newest (`split_off`), which silently skipped every UID
            // between `after_uid + 1` and the kept batch whenever more than `SYNC_BATCH` messages
            // arrived at once (laptop offline for a week, a big label, a mail archive copy).
            // See design/04-sync.md §5.3 and design/A2-final-review.md.
            let summaries = keep_oldest_page(summaries, limit);

            Ok(HeaderPage {
                uidvalidity,
                summaries,
            })
        })
        .await
    }

    /// One page of **history**: up to `limit` headers from the UIDs *below* `before_uid`.
    ///
    /// This is the primitive the "load older mail" button needs. It deliberately does **not** touch
    /// the incremental cursor: `last_uid` keeps pointing at the newest stored message, so a backfill
    /// pass can never make the updater skip anything (design/04 §5.3, and the gate in
    /// `tools/check.ps1` that pins it).
    ///
    /// The two-step shape (`UID SEARCH` for the UIDs, then one `UID FETCH` for the chosen ones) is
    /// what keeps it cheap: asking for `1:<before>` directly would make the server stream the entire
    /// older mailbox and we would throw all but one page away. `SEARCH` returns only numbers.
    pub async fn fetch_older_headers(
        &mut self,
        mailbox: &str,
        before_uid: u32,
        limit: usize,
    ) -> Result<Vec<MessageSummary>> {
        if before_uid <= 1 || limit == 0 {
            return Ok(Vec::new());
        }
        timed(async {
            self.session.select(mailbox).await.map_err(imap_err)?;

            // `1:<before-1>` — the older half. `before_uid - 1` cannot underflow because of the guard.
            let query = format!("1:{}", before_uid - 1);
            let mut uids: Vec<u32> = self
                .session
                .uid_search(query)
                .await
                .map_err(imap_err)?
                .into_iter()
                .collect();
            uids.sort_unstable();
            // The page closest to `before_uid`: those are the messages the list shows next.
            if uids.len() > limit {
                uids = uids.split_off(uids.len() - limit);
            }
            if uids.is_empty() {
                return Ok(Vec::new());
            }

            let set = uids
                .iter()
                .map(|uid| uid.to_string())
                .collect::<Vec<_>>()
                .join(",");
            let mut stream = self
                .session
                .uid_fetch(&set, "(UID FLAGS RFC822.SIZE BODY.PEEK[HEADER])")
                .await
                .map_err(imap_err)?;

            let mut summaries = Vec::new();
            while let Some(fetch) = stream.try_next().await.map_err(imap_err)? {
                summaries.push(summarise(&fetch));
            }
            // A server is free to answer with more than asked (or in any order); the caller wants
            // exactly this window, oldest first.
            summaries.retain(|m| m.uid < before_uid);
            summaries.sort_by_key(|m| m.uid);
            let summaries = keep_newest_page(summaries, limit);
            Ok(summaries)
        })
        .await
    }

    /// Fetch and fully parse one message.
    pub async fn fetch_message(&mut self, uid: u32) -> Result<MessageDetail> {
        let raw: Vec<u8> = timed(async {
            let mut stream = self
                .session
                .uid_fetch(uid.to_string(), "(UID FLAGS RFC822.SIZE BODY.PEEK[])")
                .await
                .map_err(imap_err)?;

            let mut body = None;
            while let Some(fetch) = stream.try_next().await.map_err(imap_err)? {
                if let Some(bytes) = fetch.body() {
                    body = Some(bytes.to_vec());
                    break;
                }
            }
            body.ok_or_else(|| MailError::Imap(format!("服务器没有返回 UID {uid} 的内容")))
        })
        .await?;

        parse_detail(uid, &raw)
    }

    /// Save one attachment to `dest`.
    ///
    /// **This refetches the whole message and takes the part out of it**, so peak memory is
    /// proportional to the message rather than to the attachment. That is a real limitation rather
    /// than an oversight: the cheaper approach is an IMAP section fetch (`BODY.PEEK[2]`), but the
    /// IMAP part path cannot be derived from what `mail-parser` exposes — it numbers attachments by
    /// document order, not by the dotted `1.2.3` path the protocol uses — and deriving it by walking
    /// the MIME tree is exactly the kind of clever code that breaks on real mail. Partial fetches
    /// would bound memory properly, but their responses do not parse at all; the measurement and the
    /// reason are recorded below.
    ///
    /// Returns the number of bytes written.
    pub async fn save_attachment(&mut self, uid: u32, part: u32, dest: &Path) -> Result<u64> {
        let raw = self.fetch_raw(uid).await?;

        let msg = MessageParser::default()
            .parse(&raw)
            .ok_or_else(|| MailError::Parse("MIME 结构无法解析".into()))?;
        let attachment = msg
            .attachment(part)
            .ok_or_else(|| MailError::Parse(format!("这封邮件里没有编号为 {part} 的附件")))?;
        let contents = attachment.contents();

        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| MailError::Parse(format!("无法创建目录 {}：{e}", parent.display())))?;
        }
        // Written straight from the parsed part: `contents()` is already the decoded bytes, so
        // decoding again — or copying into another buffer first — would only cost memory.
        std::fs::write(dest, contents)
            .map_err(|e| MailError::Parse(format!("无法写入 {}：{e}", dest.display())))?;

        Ok(contents.len() as u64)
    }

    /// Fetch a message's raw bytes.
    async fn fetch_raw(&mut self, uid: u32) -> Result<Vec<u8>> {
        timed(async {
            let mut stream = self
                .session
                .uid_fetch(uid.to_string(), "(UID FLAGS RFC822.SIZE BODY.PEEK[])")
                .await
                .map_err(imap_err)?;

            let mut body = None;
            while let Some(fetch) = stream.try_next().await.map_err(imap_err)? {
                if let Some(bytes) = fetch.body() {
                    body = Some(bytes.to_vec());
                    break;
                }
            }
            body.ok_or_else(|| MailError::Imap(format!("服务器没有返回 UID {uid} 的内容")))
        })
        .await
    }

    /// Fetch one byte range of the raw message.
    ///
    /// **Not used by the download path, and that is deliberate.** IMAP partial fetches
    /// (`BODY.PEEK[]<start.length>`) are legal to *request*, but a server answers with the section
    /// echoed as `BODY[]<start.length>`, and `imap-proto` 0.17's response grammar has no
    /// `<origin.offset>` in it. The reply then fails to parse with a bare `TakeWhile1` — measured
    /// against the loopback fixture. Making the fixture omit the suffix would pass the test and fail
    /// against every real server, which is worse than not having the optimisation.
    #[allow(dead_code)]
    pub async fn fetch_message_range(
        &mut self,
        uid: u32,
        start: u32,
        length: u32,
    ) -> Result<Vec<u8>> {
        // **Refuses, because no partial fetch can work before `imap-proto` is patched.**
        //
        // Measured, not inferred: `imap-proto` 0.17's `msg_att_body_section` parses
        // `BODY [section] <number> " " nstring` and never accepts the `<origin.offset>` form, so a
        // server answering `BODY[]<0.10>` fails to parse just as `<5.10>` does. async-imap answers a
        // parse failure by **closing the connection**, so attempting this would cost a pooled session
        // (A1-F42; the probe in `tests/loopback.rs` is the written record and flips when this works).
        //
        // The fix is three lines in a vendored `imap-proto` — accept `number ('.' number)?` inside the
        // angle brackets. Carrying that fork belongs with streaming attachment downloads, which are M2
        // work (design/19 §4, M0-4), so until then this returns an ordinary actionable error and
        // everything downloads whole messages.
        let _ = (uid, start, length);
        Err(MailError::Imap(
            "分部取件暂不可用（imap-proto 不接受 `<origin.offset>`；服务器那样回会让连接被关闭）。\
             大附件目前整封取回，分块下载见 design/19 §4 的 M0-4（三行补丁）。"
                .to_string(),
        ))
    }

    /// Set or clear the `\Seen` flag.
    pub async fn mark_seen(&mut self, uid: u32, seen: bool) -> Result<()> {
        let op = if seen { "+FLAGS" } else { "-FLAGS" };
        let query = format!("{op} (\\Seen)");
        timed(async {
            self.session
                .uid_store(uid.to_string(), &query)
                .await
                .map_err(imap_err)?
                .try_collect::<Vec<_>>()
                .await
                .map_err(imap_err)
        })
        .await?;
        Ok(())
    }

    /// Mark one message `\Deleted` and expunge just that message.
    ///
    /// `UID EXPUNGE` (RFC 4315) removes only the given UID. A plain `EXPUNGE` would also destroy
    /// anything else already flagged `\Deleted` — by the user, or by another client — which is a
    /// destructive surprise. So the precise form is used whenever the server advertises UIDPLUS,
    /// which is decided by capability rather than by trial and error (see `uidplus`).
    async fn flag_deleted_and_expunge(&mut self, uid: u32) -> Result<()> {
        self.session
            .uid_store(uid.to_string(), "+FLAGS (\\Deleted)")
            .await
            .map_err(imap_err)?
            .try_collect::<Vec<_>>()
            .await
            .map_err(imap_err)?;

        if self.uidplus {
            self.expunge_uid(uid).await
        } else {
            self.expunge_all().await
        }
    }

    /// Expunge exactly one UID. Only valid on servers that advertise UIDPLUS.
    async fn expunge_uid(&mut self, uid: u32) -> Result<()> {
        let stream = self
            .session
            .uid_expunge(uid.to_string())
            .await
            .map_err(imap_err)?;
        // Drain the stream: the command is only complete once every EXPUNGE response is read.
        stream.try_collect::<Vec<_>>().await.map_err(imap_err)?;
        Ok(())
    }

    /// Expunge everything flagged `\Deleted` in the selected mailbox.
    async fn expunge_all(&mut self) -> Result<()> {
        let stream = self.session.expunge().await.map_err(imap_err)?;
        stream.try_collect::<Vec<_>>().await.map_err(imap_err)?;
        Ok(())
    }

    /// Permanently delete one message.
    pub async fn delete_message(&mut self, mailbox: &str, uid: u32) -> Result<()> {
        timed(async {
            self.session.select(mailbox).await.map_err(imap_err)?;
            self.flag_deleted_and_expunge(uid).await
        })
        .await
    }

    /// Append a message to a mailbox: the "put a copy in Sent" and "upload an archive" primitives.
    ///
    /// **This deliberately returns no UID.** A UIDPLUS server answers `APPEND` with
    /// `[APPENDUID <uidvalidity> <uid>]`, and async-imap parses that response and then discards it
    /// (`append` only checks the tagged `OK`). So a caller that needs the new message's UID — which
    /// every flow does: the Sent copy has to be linked, and a re-appended archive must not be
    /// duplicated — has to look it up with [`ImapClient::find_uid_by_message_id`]. See
    /// `design/07-compose-send.md` §5.4 and A1-F43.
    ///
    /// `seen` sets the `\Seen` flag, which is right for a Sent copy and wrong for a draft.
    pub async fn append(&mut self, mailbox: &str, raw: &[u8], seen: bool) -> Result<()> {
        timed(async {
            let flags = if seen { Some("(\\Seen)") } else { None };
            self.session
                .append(mailbox, flags, None, raw)
                .await
                .map_err(imap_err)?;
            Ok(())
        })
        .await
    }

    /// Find a message's UID by its `Message-ID`, inside one mailbox.
    ///
    /// This is the documented replacement for the UID that `APPEND` does not give us. It is also the
    /// check that makes appends idempotent: searching *before* appending is what stops a retry from
    /// leaving two copies in the mailbox (only Gmail deduplicates by `Message-ID` on storage).
    ///
    /// Returns the highest matching UID — a `Message-ID` can legitimately appear more than once
    /// (Gmail keeps one copy per label; a forwarded message can carry the original header), and the
    /// newest one is the copy we just wrote.
    pub async fn find_uid_by_message_id(
        &mut self,
        mailbox: &str,
        message_id: &str,
    ) -> Result<Option<u32>> {
        let id = normalise_msg_id(message_id);
        timed(async {
            self.session.select(mailbox).await.map_err(imap_err)?;
            let query = format!("HEADER Message-ID \"{id}\"");
            let found = self.session.uid_search(query).await.map_err(imap_err)?;
            Ok(found.into_iter().max())
        })
        .await
    }

    /// Move a message to another mailbox (used for "Move to Trash").
    pub async fn move_message(&mut self, mailbox: &str, uid: u32, dest: &str) -> Result<()> {
        timed(async {
            self.session.select(mailbox).await.map_err(imap_err)?;
            self.session
                .uid_copy(uid.to_string(), dest)
                .await
                .map_err(imap_err)?;
            self.flag_deleted_and_expunge(uid).await
        })
        .await
    }

    /// Close the session politely.
    pub async fn logout(mut self) -> Result<()> {
        timed(async { self.session.logout().await.map_err(imap_err) }).await
    }

    /// Check that the connection is still usable.
    ///
    /// One `NOOP` round trip, which is what makes a connection pool safe: servers close idle
    /// connections without telling the client, and reusing one of those turns into a confusing
    /// failure at the top of some unrelated command. A round trip of a few milliseconds is a much
    /// better trade than a fresh TCP+TLS+LOGIN handshake, which is hundreds.
    pub async fn noop(&mut self) -> Result<()> {
        timed(async { self.session.noop().await.map_err(imap_err) }).await
    }

    /// Idle for `window`, then hand the connection back with the mailbox's current size.
    ///
    /// Returns `(client, exists)` where `exists` comes from a fresh `SELECT` after `DONE`. The
    /// caller compares it with the size it last saw.
    ///
    /// **Why it re-selects instead of using the pushed response.** `async_imap` 0.12's
    /// `Handle::wait_with_timeout` classifies an unsolicited `* n EXISTS` as
    /// `Response::Data { status: Status::Ok, .. }` and loops on that arm without yielding it, so
    /// `IdleResponse::NewData` never fires for new mail — measured against the loopback fixture,
    /// which pushes exactly that line and produced no `NewData` for ten seconds. (`NewData` is only
    /// reached by responses that fall through to the wildcard arm, which `EXISTS` does not.) So the
    /// window is what bounds notification latency, and a `SELECT` is how the change is seen.
    ///
    /// Takes `self` by value because `async_imap`'s `idle()` **consumes** the session and `DONE`
    /// hands it back. There is no `timed()` wrapper either: the window *is* the timeout, and the
    /// 60-second command timeout would cut every idle short.
    pub async fn idle_then_count(self, window: Duration, mailbox: &str) -> Result<(Self, u32)> {
        use async_imap::extensions::idle::IdleResponse;

        let ImapClient {
            session,
            host,
            uidplus,
        } = self;

        let mut handle = session.idle();
        // Sends IDLE and waits for the `+` continuation; a server without IDLE answers BAD here,
        // which is how the caller finds out polling is needed instead.
        handle.init().await.map_err(imap_err)?;

        // The `StopSource` is deliberately kept alive for the duration of the await; dropping it
        // would cancel the wait. A pushed response that the library does surface ends the wait
        // early, which is a bonus rather than something we depend on.
        let (future, _stop) = handle.wait_with_timeout(window);
        match future.await {
            Ok(IdleResponse::NewData(_))
            | Ok(IdleResponse::Timeout | IdleResponse::ManualInterrupt) => {}
            // The connection is gone; `done()` would fail too, so let the caller reconnect.
            Err(error) => return Err(imap_err(error)),
        }

        let session = handle.done().await.map_err(imap_err)?;
        let mut client = ImapClient {
            session,
            host,
            uidplus,
        };
        let selected = client.select(mailbox).await?;
        Ok((client, selected.exists))
    }
}

/// Build a list-row from a header-only FETCH response.
fn summarise(fetch: &async_imap::types::Fetch) -> MessageSummary {
    let flags: Vec<Flag<'_>> = fetch.flags().collect();
    let seen = flags.iter().any(|f| matches!(f, Flag::Seen));
    let flagged = flags.iter().any(|f| matches!(f, Flag::Flagged));

    let parsed = fetch
        .header()
        .and_then(|raw| MessageParser::default().parse(raw));

    let (subject, from_name, from_addr, date) = match parsed {
        Some(msg) => {
            let from = msg.from().and_then(|a| a.first());
            (
                msg.subject().unwrap_or("(无主题)").to_string(),
                from.and_then(|a| a.name()).map(str::to_string),
                from.and_then(|a| a.address()).map(str::to_string),
                msg.date().map(date_to_unix),
            )
        }
        // A malformed header must not sink the whole list; show a placeholder row instead.
        None => ("(无法解析的邮件头)".to_string(), None, None, None),
    };

    MessageSummary {
        uid: fetch.uid.unwrap_or(0),
        subject,
        from_name,
        from_addr,
        date,
        size: fetch.size,
        seen,
        flagged,
    }
}

/// Fully parse a raw RFC 5322 message.
fn parse_detail(uid: u32, raw: &[u8]) -> Result<MessageDetail> {
    let msg = MessageParser::default()
        .parse(raw)
        .ok_or_else(|| MailError::Parse("MIME 结构无法解析".into()))?;

    let from = msg.from().and_then(|a| a.first());

    let mime_of = |part: &mail_parser::MessagePart<'_>| {
        part.content_type()
            .map(|ct| {
                format!(
                    "{}/{}",
                    ct.c_type,
                    ct.c_subtype.as_deref().unwrap_or("octet-stream")
                )
            })
            .unwrap_or_else(|| "application/octet-stream".to_string())
    };

    let attachments = (0..msg.attachment_count())
        .filter_map(|pos| msg.attachment(pos as u32).map(|part| (pos as u32, part)))
        .map(|(pos, part)| AttachmentMeta {
            name: part.attachment_name().unwrap_or("未命名附件").to_string(),
            mime: mime_of(part),
            size: part.contents().len(),
            // Index within the message, re-usable later to pull the bytes on demand.
            part: pos,
            inline: part
                .content_disposition()
                .is_some_and(|cd| cd.c_type.eq_ignore_ascii_case("inline")),
            content_id: part.content_id().map(normalise_content_id),
        })
        .collect();

    // Inline images are part of the message, not fetched from anywhere, so they can be rendered
    // without any privacy cost — unlike remote images, which stay blocked.
    let inline_images = collect_inline_images(&msg, &mime_of);

    Ok(MessageDetail {
        uid,
        subject: msg.subject().unwrap_or("(无主题)").to_string(),
        from_name: from.and_then(|a| a.name()).map(str::to_string),
        from_addr: from.and_then(|a| a.address()).map(str::to_string),
        to: collect_addresses(msg.to()),
        cc: collect_addresses(msg.cc()),
        reply_to: msg
            .reply_to()
            .and_then(|a| a.first())
            .and_then(|a| a.address())
            .map(str::to_string),
        date: msg.date().map(date_to_unix),
        // mail-parser strips the angle brackets from Message-Id. RFC 5322 requires them in
        // `In-Reply-To`/`References`, and this value is fed straight back into those headers when
        // replying, so it is normalised here rather than at every call site.
        message_id: msg.message_id().map(normalise_msg_id),
        in_reply_to: msg
            .header_raw("In-Reply-To")
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string),
        references: msg
            .header_raw("References")
            .map(|r| r.split_whitespace().map(str::to_string).collect())
            .unwrap_or_default(),
        body_text: msg.body_text(0).map(|c| c.into_owned()),
        body_html: msg.body_html(0).map(|c| c.into_owned()),
        attachments,
        inline_images,
    })
}

/// Largest inline image we will embed as a `data:` URI.
///
/// These bytes are serialised into the JSON handed to the webview, so an oversized part would
/// turn into an oversized string and a stalled UI. Anything above this stays visible in the
/// attachment list, where it can be fetched deliberately instead.
const MAX_INLINE_IMAGE_BYTES: usize = 2 * 1024 * 1024;

/// Collect the images that HTML refers to by `cid:`.
fn collect_inline_images(
    msg: &mail_parser::Message<'_>,
    mime_of: &impl Fn(&mail_parser::MessagePart<'_>) -> String,
) -> Vec<InlineImage> {
    use base64::Engine as _;

    let mut out = Vec::new();

    // Walk every part, not just `attachments()`: an inline image referenced by `cid:` is not
    // always classified as an attachment by the parser.
    for pos in 0..msg.parts.len() {
        let Some(part) = msg.part(pos as u32) else {
            continue;
        };
        let Some(content_id) = part.content_id() else {
            continue;
        };

        let mime = mime_of(part);
        if !mime.starts_with("image/") {
            continue;
        }

        let contents = part.contents();
        if contents.is_empty() || contents.len() > MAX_INLINE_IMAGE_BYTES {
            continue;
        }

        out.push(InlineImage {
            content_id: normalise_content_id(content_id),
            mime,
            data_base64: base64::engine::general_purpose::STANDARD.encode(contents),
        });
    }

    out
}

/// Strip the angle brackets a `Content-ID` carries on the wire.
///
/// HTML writes `src="cid:logo@example.com"` while the header says `<logo@example.com>`, so the
/// two only match once the brackets are gone.
fn normalise_content_id(id: &str) -> String {
    id.trim()
        .trim_start_matches('<')
        .trim_end_matches('>')
        .to_string()
}

/// Ensure a Message-Id carries the angle brackets RFC 5322 requires.
///
/// mail-parser returns `abc@host`; `In-Reply-To` and `References` must contain `<abc@host>`.
fn normalise_msg_id(id: &str) -> String {
    let id = id.trim();
    if id.starts_with('<') && id.ends_with('>') {
        id.to_string()
    } else {
        format!("<{}>", id.trim_matches(|c| c == '<' || c == '>'))
    }
}

fn collect_addresses(addr: Option<&mail_parser::Address<'_>>) -> Vec<String> {
    let Some(addr) = addr else {
        return Vec::new();
    };
    addr.iter()
        .filter_map(|a| {
            let email = a.address()?;
            Some(match a.name() {
                Some(n) if !n.trim().is_empty() => format!("{n} <{email}>"),
                _ => email.to_string(),
            })
        })
        .collect()
}

/// Convert a mail `Date:` header into a Unix timestamp.
///
/// mail-parser hands back the wall-clock fields plus a separate UTC offset, so the offset has to
/// be applied by hand: `UTC = local - offset`, where an offset "before GMT" (e.g. `-0500`) is
/// negative.
fn date_to_unix(dt: &mail_parser::DateTime) -> i64 {
    let magnitude = dt.tz_hour as i64 * 3600 + dt.tz_minute as i64 * 60;
    let offset = if dt.tz_before_gmt {
        -magnitude
    } else {
        magnitude
    };

    let Some(date) =
        chrono::NaiveDate::from_ymd_opt(dt.year as i32, dt.month as u32, dt.day as u32)
    else {
        return 0;
    };
    let Some(local) = date.and_hms_opt(dt.hour as u32, dt.minute as u32, dt.second as u32) else {
        return 0;
    };
    local.and_utc().timestamp() - offset
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a `DateTime` with every field explicit.
    ///
    /// Nine positional arguments is normally a smell, but each test here wants to state all of
    /// them literally — the point is to pin down the exact field values, so spelling out a struct
    /// literal at every call site would be noisier, not clearer.
    #[allow(clippy::too_many_arguments)]
    fn dt(
        y: u16,
        mo: u8,
        d: u8,
        h: u8,
        mi: u8,
        s: u8,
        before_gmt: bool,
        tzh: u8,
        tzm: u8,
    ) -> mail_parser::DateTime {
        mail_parser::DateTime {
            year: y,
            month: mo,
            day: d,
            hour: h,
            minute: mi,
            second: s,
            tz_before_gmt: before_gmt,
            tz_hour: tzh,
            tz_minute: tzm,
        }
    }

    #[test]
    fn utc_timestamps_pass_through() {
        // 2024-01-01T00:00:00Z
        assert_eq!(
            date_to_unix(&dt(2024, 1, 1, 0, 0, 0, false, 0, 0)),
            1_704_067_200
        );
    }

    #[test]
    fn east_of_greenwich_is_subtracted() {
        // 2024-01-01T08:00:00+08:00 is the same instant as 2024-01-01T00:00:00Z
        assert_eq!(
            date_to_unix(&dt(2024, 1, 1, 8, 0, 0, false, 8, 0)),
            1_704_067_200
        );
    }

    #[test]
    fn west_of_greenwich_is_added() {
        // 2023-12-31T19:00:00-05:00 is 2024-01-01T00:00:00Z
        assert_eq!(
            date_to_unix(&dt(2023, 12, 31, 19, 0, 0, true, 5, 0)),
            1_704_067_200
        );
    }

    #[test]
    fn handles_leap_day() {
        // 2024-02-29T12:00:00Z
        assert_eq!(
            date_to_unix(&dt(2024, 2, 29, 12, 0, 0, false, 0, 0)),
            1_709_208_000
        );
    }

    #[test]
    fn impossible_dates_do_not_panic() {
        assert_eq!(date_to_unix(&dt(2024, 13, 45, 99, 99, 99, false, 0, 0)), 0);
    }

    #[test]
    fn parses_a_realistic_header_block() {
        // A GB2312-era Chinese message: a common shape that naive parsers trip over.
        // `1cXOsA==` is base64 for the GB2312 bytes D5 C5 CE B0, i.e. 张伟.
        let raw = concat!(
            "From: =?gb2312?B?1cXOsA==?= <someone@163.com>\r\n",
            "To: me@example.com\r\n",
            "Subject: =?UTF-8?B?5rWL6K+V6YKu5Lu2?=\r\n",
            "Date: Mon, 01 Jan 2024 08:00:00 +0800\r\n",
            "Message-ID: <abc@163.com>\r\n",
            "Content-Type: text/plain; charset=\"gb2312\"\r\n",
            "\r\n"
        );
        let detail = parse_detail(7, raw.as_bytes()).expect("header-only message must parse");
        assert_eq!(detail.uid, 7);
        assert_eq!(detail.subject, "测试邮件");
        assert_eq!(detail.from_addr.as_deref(), Some("someone@163.com"));
        assert_eq!(detail.from_name.as_deref(), Some("张伟"));
        assert_eq!(detail.date, Some(1_704_067_200));
        assert_eq!(detail.message_id.as_deref(), Some("<abc@163.com>"));
        assert!(detail.attachments.is_empty());
    }

    #[test]
    fn parses_a_multipart_message_with_an_attachment() {
        let raw = concat!(
            "From: a@example.com\r\n",
            "To: b@example.com\r\n",
            "Subject: report\r\n",
            "Date: Mon, 01 Jan 2024 00:00:00 +0000\r\n",
            "MIME-Version: 1.0\r\n",
            "Content-Type: multipart/mixed; boundary=\"BOUND\"\r\n",
            "\r\n",
            "--BOUND\r\n",
            "Content-Type: text/plain; charset=utf-8\r\n",
            "\r\n",
            "hello\r\n",
            "--BOUND\r\n",
            "Content-Type: application/pdf; name=\"quarterly report.pdf\"\r\n",
            "Content-Disposition: attachment; filename=\"quarterly report.pdf\"\r\n",
            "Content-Transfer-Encoding: base64\r\n",
            "\r\n",
            "aGVsbG8=\r\n",
            "--BOUND--\r\n"
        );
        let detail = parse_detail(1, raw.as_bytes()).expect("multipart must parse");
        assert_eq!(detail.body_text.as_deref().map(str::trim), Some("hello"));
        assert_eq!(detail.attachments.len(), 1);
        let att = &detail.attachments[0];
        assert_eq!(att.name, "quarterly report.pdf");
        assert_eq!(att.mime, "application/pdf");
        assert_eq!(att.size, 5);
    }

    #[test]
    fn id_command_is_well_formed() {
        let cmd = id_command();
        assert!(cmd.starts_with("ID ("));
        assert!(cmd.contains("\"name\" \"Easy Email\""));
        assert!(cmd.ends_with(')'));
    }

    #[test]
    fn message_ids_always_carry_angle_brackets() {
        assert_eq!(normalise_msg_id("abc@163.com"), "<abc@163.com>");
        assert_eq!(normalise_msg_id("<abc@163.com>"), "<abc@163.com>");
        assert_eq!(normalise_msg_id("  <a@b>  "), "<a@b>");
    }

    // ================================================================== page slicing (M0-5)
    //
    // The rule these pin down is the difference between "mail arrives late" and "mail is gone":
    // the cursor only ever moves to the highest UID that was stored.

    fn summary(uid: u32) -> MessageSummary {
        MessageSummary {
            uid,
            subject: format!("s{uid}"),
            from_name: None,
            from_addr: Some(format!("s{uid}@example.com")),
            date: Some(1_700_000_000),
            size: Some(100),
            seen: false,
            flagged: false,
        }
    }

    #[test]
    fn a_page_keeps_the_oldest_entries() {
        // 5 fetched, page of 2 ⇒ UIDs 1,2 — NOT 4,5. Keeping the newest would move the cursor to 5
        // and UIDs 3,4 would never be fetched again.
        let kept = keep_oldest_page(vec![summary(3), summary(4), summary(5)], 2);
        assert_eq!(
            kept.iter().map(|m| m.uid).collect::<Vec<_>>(),
            vec![3, 4],
            "the oldest entries must survive so the cursor stops before the gap"
        );
    }

    #[test]
    fn a_page_smaller_than_the_limit_is_returned_whole() {
        let kept = keep_oldest_page(vec![summary(1), summary(2)], 500);
        assert_eq!(kept.len(), 2);
    }

    #[test]
    fn a_limit_of_zero_means_no_page_limit() {
        let kept = keep_oldest_page(vec![summary(1), summary(2), summary(3)], 0);
        assert_eq!(kept.len(), 3, "0 is the documented 'unbounded' value");
    }

    #[test]
    fn a_page_of_exactly_the_limit_is_untouched() {
        let kept = keep_oldest_page(vec![summary(7), summary(8)], 2);
        assert_eq!(kept.iter().map(|m| m.uid).collect::<Vec<_>>(), vec![7, 8]);
    }

    #[test]
    fn the_since_range_is_bounded_by_the_page_size() {
        // 500 is SYNC_BATCH: asking for `501:1000` rather than `501:*` is what stops a big burst
        // from streaming headers we are about to throw away.
        assert_eq!(since_range(500, 500), "501:1000");
        assert_eq!(since_range(0, 200), "1:200");
    }

    #[test]
    fn the_since_range_stays_open_ended_without_a_limit() {
        assert_eq!(since_range(500, 0), "501:*");
        assert_eq!(since_range(0, 0), "1:*");
    }

    #[test]
    fn the_since_range_does_not_overflow_at_the_top_of_the_uid_space() {
        // A hostile or broken server could report UIDNEXT near u32::MAX; the range must stay a
        // valid number rather than panicking in debug builds.
        assert_eq!(since_range(u32::MAX, 10), "4294967295:4294967295");
        assert_eq!(since_range(u32::MAX - 1, 10), "4294967295:4294967295");
    }
}
