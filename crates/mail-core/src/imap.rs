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
    AccountConfig, AttachmentMeta, AuthMethod, Endpoint, InlineImage, MailboxInfo, MessageDetail,
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

/// The `UID` set meaning "everything above the cursor": `after_uid + 1 : *`.
///
/// **Open-ended on purpose.** An earlier version bounded the high end as `after_uid + limit` to ask
/// for "one batch" in a single round trip, which is not a page at all: IMAP UIDs are strictly
/// ascending but **not dense**, so a mailbox whose UIDs do not start near 1 matches nothing in a
/// small numeric window. NetEase hands out UIDs around 1.5e9, and `UID FETCH 1:500` on a 20-message
/// inbox therefore returned nothing — the folder never synced and the cursor never moved, for ever
/// (a full web inbox and an empty client). Bounding happens after the fact instead:
/// [`ImapClient::fetch_headers_since`] enumerates what is above the cursor with
/// `UID FETCH <set> (UID)` and fetches headers for only the oldest `limit` of the answer.
///
/// **Not `UID SEARCH`.** QQ answers `UID SEARCH 210:*` with nothing while `UID FETCH 210:* (UID)`
/// finds the message (measured 2026-11-01, `tests/sent_uid_probe.rs`, branch A) — the range is fine,
/// the search over it is not.
pub fn since_range(after_uid: u32) -> String {
    format!("{}:*", after_uid.saturating_add(1))
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

/// What `UID SEARCH` and `UID FETCH` each answer about one target UID in one folder.
///
/// `UID SEARCH <n>:*` and `UID FETCH <n>:*` are both *ranges*, and RFC 3501 says nothing about how a
/// server must resolve a range whose low end sits above the highest UID in the folder. `design/07`
/// §14.2 records a real account where `UID SEARCH 210:*` came back empty while UID 226 existed — and
/// "empty" has at least three causes with three different fixes. The previous plan picked one of them
/// and was about to pay for it; this struct is how the guess becomes evidence.
///
/// Read-only by construction: one `SELECT` plus three `UID` commands, no `STORE`, no `APPEND`,
/// nothing that can change the mailbox.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UidProbe {
    /// Folder the probe ran in, as the wire spells it — the verdict line is worthless without it.
    pub folder: String,
    /// `UIDVALIDITY` from the `SELECT` the commands below ran under.
    pub uidvalidity: u32,
    /// `UIDNEXT` from that same `SELECT`.
    pub uidnext: u32,
    /// The cursor the question is about: `after_uid + 1` is the low end of every range below.
    pub after_uid: u32,
    /// The UID we are asking about (the one the user can see in another client).
    pub target: u32,
    /// `UID SEARCH <after+1>:*` named the target. `None` = the command itself failed.
    pub search_since: Option<bool>,
    /// `UID SEARCH ALL` named the target.
    pub search_all: Option<bool>,
    /// `UID FETCH <after+1>:* (UID)` returned the target.
    pub fetch_since: Option<bool>,
    /// First failure, verbatim. "The server said no" and "the server said nothing" are different
    /// diagnoses and only the message tells them apart.
    pub error: Option<String>,
}

/// Which fix the evidence points at (`design/07` §14.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UidBranch {
    /// `SEARCH ALL` and `FETCH <n>:*` both see it, `SEARCH <n>:*` does not: the *search* range is
    /// what is broken, so the fix is to stop asking `SEARCH` about ranges.
    A,
    /// Only `SEARCH ALL` sees it: both range forms are untrustworthy, so the fix has to avoid ranges
    /// altogether (`UID FETCH 1:* (UID)` and filter locally).
    APrime,
    /// The server agrees the message is there, so the loss is on our side — branch B or C. Which one
    /// needs the local cursor, which this type cannot see on purpose.
    ServerAgrees,
    /// The evidence supports no conclusion: the target is not in the folder, or a command failed.
    /// Saying "unknown" is the honest answer, and the alternative is inventing a cause again.
    Undecided,
}

impl UidProbe {
    /// Classify the answers. Kept separate from the network call so the decision table can be tested
    /// without a server — a diagnosis nobody can re-run offline is a rumour, not a result.
    pub fn branch(&self) -> UidBranch {
        match (self.search_all, self.search_since, self.fetch_since) {
            (Some(true), Some(false), Some(true)) => UidBranch::A,
            (Some(true), Some(false), Some(false)) => UidBranch::APrime,
            (Some(true), Some(true), Some(true)) => UidBranch::ServerAgrees,
            _ => UidBranch::Undecided,
        }
    }

    /// The one line `design/07` §14.3 asks to be recorded, e.g.
    /// `同步 Sent Messages：SEARCH 空 / FETCH 有 226 ⇒ 分支 A`.
    pub fn verdict(&self) -> String {
        format!(
            "同步 {}：SEARCH {} / FETCH {} {} ⇒ 分支 {}",
            self.folder,
            answer(self.search_since),
            answer(self.fetch_since),
            self.target,
            self.branch().name()
        )
    }
}

impl UidBranch {
    pub fn name(self) -> &'static str {
        match self {
            UidBranch::A => "A（SEARCH 的区间不可信）",
            UidBranch::APrime => "A′（区间解释整体不可信）",
            UidBranch::ServerAgrees => "服务器一致（B/C，看本地游标）",
            UidBranch::Undecided => "无法判定",
        }
    }
}

fn answer(seen: Option<bool>) -> &'static str {
    match seen {
        Some(true) => "有",
        Some(false) => "空",
        None => "失败",
    }
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

/// Whether an `IDLE` attempt was refused because the server does not implement the command.
///
/// `async-imap` reports a tagged `BAD` answer to `IDLE` as an `io::Error` with kind
/// `ConnectionRefused` (`extensions/idle.rs`), so the *kind* is the signal — the server's wording
/// differs per server (`command not support` on 163) and matching on it would be fragile. A real
/// I/O failure on the same call has a different kind (`ConnectionReset`, `BrokenPipe`, …) and is
/// still treated as a dropped connection. The empty-message case is `async-imap`'s own "the stream
/// ended" sentinel rather than a server sentence, so it is not a refusal either.
fn idle_refused(error: &async_imap::error::Error) -> bool {
    matches!(
        error,
        async_imap::error::Error::Io(source)
            if source.kind() == std::io::ErrorKind::ConnectionRefused
                && !source.to_string().is_empty()
    )
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
    /// Which mailbox this session is currently `SELECT`ed into, as far as we know.
    ///
    /// IMAP's `UID FETCH`/`STORE`/`SEARCH` are **only meaningful inside a selection**, and a UID
    /// means a different message in every folder. Without this field the callers that forget to
    /// `SELECT` do not fail loudly — they read whatever folder the pooled connection happens to be
    /// sitting in, and store that body under another folder's key. That is exactly what the body
    /// prefetch pass did until 2026-10-20 (`index::BodySource for ImapClient` documents the fix).
    /// Every `SELECT` on this type goes through [`ImapClient::select_to`], so the field cannot
    /// drift behind the session.
    selected: Option<String>,
}

impl ImapClient {
    /// Connect and authenticate.
    /// Connect with whatever this account authenticates with.
    ///
    /// The second argument is "the secret": a password for a password account, an access token for an
    /// OAuth2 one. Which of the two it is comes from `cfg.auth` — a fact the account carries — rather
    /// than from a flag at every call site, so switching an account to OAuth does not mean finding
    /// every place that opens a connection.
    pub async fn connect(cfg: &AccountConfig, secret: &str) -> Result<Self> {
        let login = match cfg.auth {
            AuthMethod::Password => Login::Password(secret.to_string()),
            AuthMethod::OAuth2 => Login::OAuth2(secret.to_string()),
        };
        Self::connect_with(cfg, login).await
    }

    /// Connect with an OAuth2 access token instead of a password (`design/05` §3.2).
    ///
    /// The token comes from `oauth::refresh`, and it is short-lived by design: this function does not
    /// cache or renew it, so the caller renews before each connection rather than discovering halfway
    /// through a sync that the token expired.
    /// Connect with whichever credential the caller resolved.
    pub async fn connect_credential(cfg: &AccountConfig, credential: &Credential) -> Result<Self> {
        match credential {
            Credential::Password(password) => Self::connect(cfg, password).await,
            Credential::OAuth2(token) => Self::connect_oauth(cfg, token).await,
        }
    }

    pub async fn connect_oauth(cfg: &AccountConfig, access_token: &str) -> Result<Self> {
        Self::connect_with(cfg, Login::OAuth2(access_token.to_string())).await
    }

    /// Everything the two entry points share, up to and including authentication.
    async fn connect_with(cfg: &AccountConfig, login: Login) -> Result<Self> {
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

        // `login` and `authenticate` both take the client by value, so the future must own everything
        // it touches.
        let mut session = match login {
            Login::Password(password) => {
                let host = ep.host.clone();
                let username = cfg.username.clone();
                let needs_id = cfg.needs_id_command;

                timed(async move {
                    client.login(&username, &password).await.map_err(|(e, _)| {
                        MailError::classify_login_scrubbed(
                            &host,
                            e.to_string(),
                            needs_id,
                            &password,
                        )
                    })
                })
                .await?
            }
            Login::OAuth2(token) => {
                let host = ep.host.clone();
                let username = cfg.username.clone();
                let auth = Xoauth2::new(&username, &token);

                timed(async move {
                    client
                        .authenticate("XOAUTH2", auth)
                        .await
                        .map_err(|(e, _)| {
                            // The access token is scrubbed the same way as a password: it is a
                            // credential, and the server has just been shown it.
                            MailError::classify_login_scrubbed(&host, e.to_string(), false, &token)
                        })
                })
                .await?
            }
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
            // A fresh session is in the authenticated state: no mailbox is selected yet, and every
            // folder-scoped command has to earn its `SELECT` first.
            selected: None,
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

    /// The wire name of the folder that holds drafts, if the server has one.
    ///
    /// Same role-based resolution as [`ImapClient::sent_mailbox`], and for the same reason: a server
    /// may name the folder anything and only say `\Drafts` in the attribute.
    pub async fn drafts_mailbox(&mut self) -> Result<Option<String>> {
        Ok(self
            .list_mailboxes_with_roles()
            .await?
            .into_iter()
            .find(|(mailbox, role)| *role == Some(Role::Drafts) && mailbox.selectable)
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
        self.select_to(mailbox).await
    }

    /// The one place `SELECT` is sent, so [`ImapClient::selected`] cannot fall behind the session.
    async fn select_to(&mut self, mailbox: &str) -> Result<Mailbox> {
        let mailbox_metadata =
            timed(async { self.session.select(mailbox).await.map_err(imap_err) }).await?;
        self.selected = Some(mailbox.to_string());
        Ok(mailbox_metadata)
    }

    /// Make `mailbox` the current selection, skipping the command when it already is.
    ///
    /// Folder-scoped primitives (`UID FETCH`, `UID STORE`) carry no folder of their own, so anything
    /// that loops over messages from several folders has to call this before it asks for one —
    /// otherwise the answer it gets belongs to a different folder's UID space.
    pub async fn use_mailbox(&mut self, mailbox: &str) -> Result<()> {
        if self.selected.as_deref() == Some(mailbox) {
            return Ok(());
        }
        self.select_to(mailbox).await?;
        Ok(())
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
            let selected = self.select_to(mailbox).await?;
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
    /// This is what makes the local cache cheap to keep current: one `SELECT` (so the caller does not
    /// have to make a second one just to read `UIDVALIDITY`), one `UID FETCH … (UID)` that asks the
    /// server to *enumerate* the range, and one `UID FETCH` for at most one page of headers.
    ///
    /// **It used to ask `UID SEARCH <after+1>:*` and that is why a sent copy could be invisible.**
    /// Measured 2026-11-01 on QQ's `Sent Messages` (`tests/sent_uid_probe.rs`, branch A): with
    /// `UIDVALIDITY=1579573363`, cursor 209 and UID 226 present, `UID SEARCH 210:*` answered
    /// **empty**, `UID SEARCH ALL` contained 226, and `UID FETCH 210:* (UID)` contained 226. Two
    /// commands, same range, opposite answers — so the search range is what cannot be trusted, and a
    /// folder that never reports anything new never syncs (design/07 §14.2 root cause 2).
    ///
    /// **Enumeration is also what makes "one page" correct.** A single
    /// `UID FETCH <after+1>:<after+limit>` looks bounded but is not a page — UIDs are not dense, so
    /// on a server whose UIDs do not start near 1 the numeric window matches nothing and the folder
    /// never syncs (see [`since_range`]). The UID-only fetch is cheap in the ordinary case too: with
    /// nothing new the range resolves to the last message alone (RFC 3501 ranges are unordered), one
    /// row comes back, and the filter below drops it.
    pub async fn fetch_headers_since(
        &mut self,
        mailbox: &str,
        after_uid: u32,
        limit: usize,
    ) -> Result<HeaderPage> {
        timed(async {
            let selected = self.select_to(mailbox).await?;
            // A public field in async-imap, not an accessor.
            let uidvalidity = selected.uid_validity.unwrap_or(0);

            let query = since_range(after_uid);
            // `(UID)` and nothing else: no flags, no size, no header section. The cost is the round
            // trip, not the bytes, and asking for the numbers alone is what keeps it at the same
            // price as the `SEARCH` it replaces.
            let mut uids: Vec<u32> = Vec::new();
            {
                let mut stream = self
                    .session
                    .uid_fetch(&query, "(UID)")
                    .await
                    .map_err(imap_err)?;
                while let Some(fetch) = stream.try_next().await.map_err(imap_err)? {
                    if let Some(uid) = fetch.uid {
                        uids.push(uid);
                    }
                }
            }
            // **The filter is not redundant.** `low:*` is still a *range*, and RFC 3501 ranges are
            // unordered — so when `after_uid` is the folder's highest UID, `after+1:*` resolves to
            // that last message rather than to nothing, and every sync would re-store it for ever.
            // (The loopback fixture is deliberately faithful to this; it is what caught the missing
            // filter here.) Dropping the low ones also stops them wasting a slot of the page below.
            uids.retain(|uid| *uid > after_uid);
            uids.sort_unstable();
            // **Keep the OLDEST `limit`, never the newest.** The cursor only advances to the highest
            // UID we actually stored (`sync::advance_cursor`), so dropping the *old* end would move
            // it past messages that were never stored and they could never be fetched again — a
            // permanent hole in the mailbox. Keeping the oldest makes the next pass resume exactly
            // where this one stopped.
            //
            // History: an earlier version kept the newest (`split_off`), which silently skipped every
            // UID between `after_uid + 1` and the kept batch whenever more than `SYNC_BATCH` messages
            // arrived at once (laptop offline for a week, a big label, a mail archive copy).
            // See design/04-sync.md §5.3 and design/A2-final-review.md.
            if limit > 0 && uids.len() > limit {
                uids.truncate(limit);
            }
            if uids.is_empty() {
                return Ok(HeaderPage {
                    uidvalidity,
                    summaries: Vec::new(),
                });
            }

            let set = uids
                .iter()
                .map(u32::to_string)
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
            summaries.sort_by_key(|m| m.uid);

            Ok(HeaderPage {
                uidvalidity,
                summaries,
            })
        })
        .await
    }

    /// Read-only diagnosis of the two UID primitives (`design/07` §14.3, step D-1).
    ///
    /// The order is the design's, and it is not arbitrary: `SELECT` first, because `UIDVALIDITY` and
    /// `UIDNEXT` have to describe the same folder generation the three commands below run under;
    /// then `UID SEARCH <after+1>:*` (what the sync path actually does); then `UID SEARCH ALL`; then
    /// `UID FETCH <after+1>:* (UID)`. The last two are the controls — the third asks the server to
    /// *enumerate* rather than to search, which is why it can disagree with the first.
    ///
    /// A failing command is recorded rather than returned: a rejection is itself one of the answers,
    /// and the caller wants all of them in one report. It also means the session may be unusable
    /// afterwards (async-imap closes the stream on a rejection), so this belongs on a throwaway
    /// connection, not in the middle of a sync.
    pub async fn probe_uid_visibility(
        &mut self,
        mailbox: &str,
        after_uid: u32,
        target: u32,
    ) -> Result<UidProbe> {
        let selected = self.select_to(mailbox).await?;
        let mut probe = UidProbe {
            folder: mailbox.to_string(),
            // Public fields in async-imap, not accessors.
            uidvalidity: selected.uid_validity.unwrap_or(0),
            uidnext: selected.uid_next.unwrap_or(0),
            after_uid,
            target,
            ..UidProbe::default()
        };

        let range = since_range(after_uid);
        match self.session.uid_search(&range).await {
            Ok(uids) => probe.search_since = Some(uids.contains(&target)),
            Err(e) => probe.error = Some(format!("SEARCH {range}: {e}")),
        }

        match self.session.uid_search("ALL").await {
            Ok(uids) => probe.search_all = Some(uids.contains(&target)),
            Err(e) => {
                probe
                    .error
                    .get_or_insert_with(|| format!("SEARCH ALL: {e}"));
            }
        }

        let mut seen: Vec<u32> = Vec::new();
        match self.session.uid_fetch(&range, "(UID)").await {
            Ok(mut stream) => {
                let mut failed = None;
                loop {
                    match stream.try_next().await {
                        Ok(Some(fetch)) => {
                            if let Some(uid) = fetch.uid {
                                seen.push(uid);
                            }
                        }
                        Ok(None) => break,
                        Err(e) => {
                            failed = Some(format!("FETCH {range}: {e}"));
                            break;
                        }
                    }
                }
                // Only a stream that ran to completion is an answer; a half-read one would report
                // "no" for a target that simply had not arrived yet.
                if failed.is_none() {
                    probe.fetch_since = Some(seen.contains(&target));
                }
                if let Some(message) = failed {
                    probe.error.get_or_insert(message);
                }
            }
            Err(e) => {
                probe
                    .error
                    .get_or_insert_with(|| format!("FETCH {range}: {e}"));
            }
        }

        Ok(probe)
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
            self.select_to(mailbox).await?;

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

    /// Fetch several whole messages from the **current selection** in one command.
    ///
    /// This exists because the background body index used to pay a round trip per message, and
    /// `reports/performance.md` §16.3 measured 98 % of that pass's cost as nothing but waiting.
    ///
    /// **The echoed UID decides which message the bytes belong to, never the position** — a server may
    /// answer a set in any order, and the caller stores by `(folder, uid)`. Keying by position would
    /// file one message's body behind another message's key, which is the exact class of bug the
    /// 2026-10-20 fix removed. A response without a UID is dropped rather than guessed at.
    ///
    /// An `Err` *inside* the map is "this message is unusable" (too big to hold, unparsable); an
    /// `Err` from the call is about **the command as a whole**, and the caller retries those UIDs one
    /// by one — a server with an opinion about set sizes must not cost the user their mail.
    pub async fn fetch_messages(
        &mut self,
        uids: &[u32],
    ) -> Result<std::collections::HashMap<u32, Result<MessageDetail>>> {
        let mut out = std::collections::HashMap::new();
        if uids.is_empty() {
            return Ok(out);
        }
        // A comma set, never a range: UIDs are not dense, and `a:b` would fetch mail nobody asked
        // for (see [`since_range`], which is the same trap in the header path).
        let set = uids
            .iter()
            .map(u32::to_string)
            .collect::<Vec<_>>()
            .join(",");

        timed(async {
            let mut stream = self
                .session
                .uid_fetch(&set, "(UID FLAGS RFC822.SIZE BODY.PEEK[])")
                .await
                .map_err(imap_err)?;

            while let Some(fetch) = stream.try_next().await.map_err(imap_err)? {
                let Some(uid) = fetch.uid else { continue };
                let Some(bytes) = fetch.body() else { continue };
                // Same ceiling as a single fetch, applied per message: one 500 MB literal must not
                // take the process — or the other nineteen messages in the batch — out with it.
                if bytes.len() > MAX_FETCH_BYTES {
                    out.insert(
                        uid,
                        Err(MailError::Parse(format!(
                            "邮件太大（{} MB），超过本应用能安全处理的 {} MB 上限。",
                            bytes.len() / (1024 * 1024),
                            MAX_FETCH_BYTES / (1024 * 1024)
                        ))),
                    );
                    continue;
                }
                out.insert(uid, parse_detail(uid, bytes));
            }
            Ok::<(), MailError>(())
        })
        .await?;

        Ok(out)
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
    ///
    /// The size is checked against [`MAX_FETCH_BYTES`] **before** the bytes are copied: `async-imap`
    /// will happily buffer a literal up to its own 512 MiB ceiling, and `fetch.body()` hands back a
    /// buffer this code then copies wholesale. A server (or a sender) that answers with a 500 MiB
    /// literal would otherwise take the process out — and a mail client that dies on one message is
    /// a mail client that cannot show the user the other nine hundred.
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
                    if bytes.len() > MAX_FETCH_BYTES {
                        return Err(MailError::Parse(format!(
                            "邮件太大（{} MB），超过本应用能安全处理的 {} MB 上限。",
                            bytes.len() / (1024 * 1024),
                            MAX_FETCH_BYTES / (1024 * 1024)
                        )));
                    }
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
            self.select_to(mailbox).await?;
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
        // The `Message-ID` reaches `uid_search` as an IMAP search key, and `async-imap` does **not**
        // run its own CRLF/quote validation on `uid_search` (it does for `select`/`copy`/`append`).
        // The value comes from this app's own `make_message_id`, which is built from the account's
        // address, so a malformed address is the only way in — but "only way in" is not a reason to
        // build a protocol line out of unvalidated text, and the fix is two bytes wide.
        let id = id.replace(['"', '\\'], "");
        let id: String = id.chars().filter(|c| !c.is_control()).collect();
        timed(async {
            self.select_to(mailbox).await?;
            let query = format!("HEADER Message-ID \"{id}\"");
            let found = self.session.uid_search(query).await.map_err(imap_err)?;
            Ok(found.into_iter().max())
        })
        .await
    }

    /// Move a message to another mailbox (used for "Move to Trash").
    pub async fn move_message(&mut self, mailbox: &str, uid: u32, dest: &str) -> Result<()> {
        timed(async {
            self.select_to(mailbox).await?;
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
            selected: selection,
        } = self;

        let mut handle = session.idle();
        // Sends IDLE and waits for the `+` continuation. A server without IDLE answers BAD here,
        // which async-imap surfaces as a synthetic `io::Error`; that is turned into
        // `MailError::IdleUnsupported` so the watcher can fall back to polling instead of
        // reconnecting a command the server will refuse every time.
        handle.init().await.map_err(|error| {
            if idle_refused(&error) {
                MailError::IdleUnsupported(host.clone())
            } else {
                imap_err(error)
            }
        })?;

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
            // IDLE does not leave the selection, so carry it over rather than claiming the session
            // came back unselected — that would cost a `SELECT` the server does not need.
            selected: selection,
        };
        let mailbox_state = client.select(mailbox).await?;
        Ok((client, mailbox_state.exists))
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

    let (subject, from_name, from_addr, date, thread_key) = match parsed {
        Some(msg) => {
            let from = msg.from().and_then(|a| a.first());
            // The thread key comes from headers already in hand: the root of the `References` chain,
            // or the message's own id. Computing it here — rather than when the list is read — is what
            // lets the cache store it and the renderer group without the chain.
            let key = crate::threads::root_of(
                msg.message_id().unwrap_or_default(),
                msg.header_raw("References").unwrap_or_default(),
            );
            (
                msg.subject().unwrap_or("(无主题)").to_string(),
                from.and_then(|a| a.name()).map(str::to_string),
                from.and_then(|a| a.address()).map(str::to_string),
                msg.date().map(date_to_unix),
                (!key.is_empty()).then_some(key),
            )
        }
        // A malformed header must not sink the whole list; show a placeholder row instead.
        None => ("(无法解析的邮件头)".to_string(), None, None, None, None),
    };

    MessageSummary {
        uid: fetch.uid.unwrap_or(0),
        subject,
        from_name,
        from_addr,
        date,
        size: fetch.size,
        thread_key,
        // Computed by the cache, which is the layer that knows whose account this is: the key is only
        // for mail the user received, and "is this sender me?" cannot be answered from the headers.
        subject_key: None,
        seen,
        flagged,
    }
}

/// The credential for one connection, as the app hands it over.
///
/// A type rather than a string plus a flag, for the reason the SMTP side already spells out: the two
/// shapes behave differently (a token expires and is renewable; a password is neither), and a flag at
/// every call site is where that difference gets lost.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Credential {
    Password(String),
    OAuth2(String),
}

/// How the client proves who it is, for the two shapes the providers ship.
enum Login {
    Password(String),
    OAuth2(String),
}

/// The XOAUTH2 client response, exactly as Google and Microsoft specify it.
///
/// `user=<address>\x01auth=Bearer <token>\x01\x01` — the **two trailing `\x01`** are not decoration;
/// the SASL profile requires the double separator, and servers reject the response without it (which
/// then looks like an invalid token rather than a malformed request).
pub fn xoauth2_response(user: &str, access_token: &str) -> String {
    format!("user={user}\x01auth=Bearer {access_token}\x01\x01")
}

/// The `Authenticator` async-imap asks for: base64 of [`xoauth2_response`] on the first challenge,
/// and an empty response afterwards.
///
/// The second part matters: when XOAUTH2 fails, the server sends a challenge containing a JSON error
/// and expects an empty line before it reports the failure. Answering with the response again would
/// loop; answering with nothing lets the server's own message come through, which is what
/// `classify_login` turns into something a person can act on.
struct Xoauth2 {
    response: String,
    sent: bool,
}

impl Xoauth2 {
    fn new(user: &str, access_token: &str) -> Xoauth2 {
        Xoauth2 {
            response: xoauth2_response(user, access_token),
            sent: false,
        }
    }
}

impl async_imap::Authenticator for Xoauth2 {
    type Response = Vec<u8>;

    fn process(&mut self, _challenge: &[u8]) -> Vec<u8> {
        if self.sent {
            return Vec::new();
        }
        self.sent = true;
        self.response.clone().into_bytes()
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
        // Sanitised **here**, where the HTML enters the application, so what is written to the cache
        // is already safe and every later reader inherits that (see `sanitize`'s module comment for
        // the `meta http-equiv="refresh"` hole this closes). `Cache::get_detail` runs it again on the
        // way out for the rows that were stored before this existed.
        body_html: msg
            .body_html(0)
            .map(|c| crate::sanitize::sanitize_mail_html(&c)),
        attachments,
        inline_images,
    })
}

/// Largest inline image we will embed as a `data:` URI.
///
/// These bytes are serialised into the JSON handed to the webview, so an oversized part would
/// turn into an oversized string and a stalled UI. Anything above this stays visible in the
/// attachment list, where it can be fetched deliberately instead — and that second half is not a
/// hope: `an_oversized_inline_image_is_not_embedded_but_is_still_reachable` measures that
/// `mail-parser` really does list a `Content-Disposition: inline` part, because a dropped image
/// with no row to click would be a silent loss of mail content.
const MAX_INLINE_IMAGE_BYTES: usize = 2 * 1024 * 1024;

/// Largest message this app will buffer in memory.
///
/// Deliberately generous — mail with a 100 MB video attachment is real, and refusing it would be a
/// worse bug than the one this guards. The point is that the ceiling is *ours* and produces a
/// readable error, rather than being `async-imap`'s 512 MiB `Buffer::MAX_CAPACITY` arriving as an
/// out-of-memory kill on a machine that is also running the user's browser.
const MAX_FETCH_BYTES: usize = 128 * 1024 * 1024;

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
        let Some(subtype) = mime.strip_prefix("image/") else {
            continue;
        };
        if subtype.is_empty()
            || !subtype
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '+' || c == '-')
        {
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

    /// The decision table from `design/07` §14.3, one row per branch.
    ///
    /// This is the part of the diagnostic that can be re-run without a mailbox, so it is the part
    /// that has to be pinned: the four rows are exactly the four conclusions the design allows, and
    /// an unknown shape must fall through to `Undecided` rather than to the nearest guess.
    #[test]
    fn the_uid_probe_classifies_each_branch_from_its_three_answers() {
        let case = |search_all, search_since, fetch_since| UidProbe {
            folder: "Sent Messages".to_string(),
            uidvalidity: 1579573363,
            uidnext: 227,
            after_uid: 209,
            target: 226,
            search_since,
            search_all,
            fetch_since,
            error: None,
        };

        // A: SEARCH's range is the broken one; FETCH's range still works.
        assert_eq!(
            case(Some(true), Some(false), Some(true)).branch(),
            UidBranch::A
        );
        // A′: both range forms are broken, only the unfiltered search works.
        assert_eq!(
            case(Some(true), Some(false), Some(false)).branch(),
            UidBranch::APrime
        );
        // The server agrees the message is there: the loss is local (B or C).
        assert_eq!(
            case(Some(true), Some(true), Some(true)).branch(),
            UidBranch::ServerAgrees
        );
        // Target not in the folder at all — the premise is wrong, so no conclusion.
        assert_eq!(
            case(Some(false), Some(false), Some(false)).branch(),
            UidBranch::Undecided
        );
        // A failed command must not be read as "the server said no".
        assert_eq!(
            case(Some(true), None, Some(true)).branch(),
            UidBranch::Undecided
        );
    }

    /// The verdict line is the artefact `design/07` §14.3 requires to be recorded, so its shape is
    /// asserted rather than eyeballed: folder, both answers, the target UID, the branch.
    #[test]
    fn the_verdict_line_names_the_folder_both_answers_and_the_branch() {
        let probe = UidProbe {
            folder: "Sent Messages".to_string(),
            uidvalidity: 1579573363,
            uidnext: 227,
            after_uid: 209,
            target: 226,
            search_since: Some(false),
            search_all: Some(true),
            fetch_since: Some(true),
            error: None,
        };

        assert_eq!(
            probe.verdict(),
            "同步 Sent Messages：SEARCH 空 / FETCH 有 226 ⇒ 分支 A（SEARCH 的区间不可信）"
        );
    }

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
            subject_key: None,
            thread_key: None,
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
    fn the_since_range_is_open_ended_so_sparse_uids_are_not_missed() {
        // The bug this pins (found on a real 163 mailbox): the high end used to be
        // `after_uid + limit`, i.e. a numeric window such as `1:500`. IMAP UIDs are not dense — that
        // mailbox's UIDs were ~1.54e9 — so the window matched nothing, the folder never synced, and
        // the cursor never moved: a full web inbox and an empty client. The range must be open-ended;
        // the *page* is enforced afterwards by `fetch_headers_since` truncating the SEARCH result to
        // the oldest `limit`.
        assert_eq!(since_range(0), "1:*");
        assert_eq!(since_range(500), "501:*");
        assert_eq!(since_range(1_542_128_699), "1542128700:*");
    }

    #[test]
    fn the_since_range_does_not_overflow_at_the_top_of_the_uid_space() {
        // A hostile or broken server could report the cursor at u32::MAX; `+ 1` must saturate rather
        // than panic in a debug build, and the range must stay a valid number.
        assert_eq!(since_range(u32::MAX), "4294967295:*");
        assert_eq!(since_range(u32::MAX - 1), "4294967295:*");
    }

    /// What the 2 MB inline-image ceiling actually leaves the user.
    ///
    /// `MAX_INLINE_IMAGE_BYTES` drops an oversized part from the webview payload — that half is a
    /// one-line `continue`, easy to believe. The claim worth testing is the one the constant's
    /// comment makes back: that the part "stays visible in the attachment list, where it can be
    /// fetched deliberately instead". Whether `mail-parser` counts a `Content-Disposition: inline`
    /// part as an attachment is the parser's behaviour, not ours, so it gets measured.
    #[test]
    fn an_oversized_inline_image_is_not_embedded_but_is_still_reachable() {
        use base64::Engine as _;

        let b64 =
            base64::engine::general_purpose::STANDARD.encode(vec![7u8; MAX_INLINE_IMAGE_BYTES + 1]);
        let raw = format!(
            "From: a@example.com\r\n\
             To: me@example.com\r\n\
             Subject: 大图\r\n\
             MIME-Version: 1.0\r\n\
             Content-Type: multipart/related; boundary=\"REL\"\r\n\
             \r\n\
             --REL\r\n\
             Content-Type: text/html; charset=utf-8\r\n\
             \r\n\
             <p><img src=\"cid:big@example.com\"></p>\r\n\
             --REL\r\n\
             Content-Type: image/png\r\n\
             Content-ID: <big@example.com>\r\n\
             Content-Disposition: inline; filename=\"big.png\"\r\n\
             Content-Transfer-Encoding: base64\r\n\
             \r\n\
             {b64}\r\n\
             --REL--\r\n"
        );

        let detail = parse_detail(1, raw.as_bytes()).expect("the fixture parses");

        assert!(
            detail.inline_images.is_empty(),
            "a part above the ceiling must not be base64-serialised into the webview payload"
        );
        let listed: Vec<(String, bool, usize)> = detail
            .attachments
            .iter()
            .map(|a| (a.name.clone(), a.inline, a.size))
            .collect();
        assert_eq!(
            listed,
            vec![("big.png".to_string(), true, MAX_INLINE_IMAGE_BYTES + 1)],
            "the only way back to a dropped inline image is the attachment row that names its part"
        );
    }
}
