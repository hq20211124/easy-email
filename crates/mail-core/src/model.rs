//! Serializable data types shared between the protocol layer, the Tauri commands and the UI.

use serde::{Deserialize, Serialize};

/// Transport security for a single endpoint.
///
/// Note `Plain` exists for localhost-only transports such as Proton Mail Bridge. It must
/// never be selected for a remote host.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Security {
    /// Implicit TLS, i.e. connect and immediately handshake (IMAP 993, SMTP 465).
    Ssl,
    /// Connect in the clear, then upgrade with STARTTLS (IMAP 143, SMTP 587).
    StartTls,
    /// No encryption at all. Localhost only.
    Plain,
}

impl Security {
    pub fn label(self) -> &'static str {
        match self {
            Security::Ssl => "SSL/TLS",
            Security::StartTls => "STARTTLS",
            Security::Plain => "不加密",
        }
    }

    /// Every choice, in the order the wizard shows them. One source of truth: the wire values come
    /// from serde, and a renderer that hard-coded them would keep "working" after a rename.
    pub const ALL: [Security; 3] = [Security::Ssl, Security::StartTls, Security::Plain];

    /// The one-line explanation shown next to each choice.
    pub fn hint(self) -> &'static str {
        match self {
            Security::Ssl => "直接 TLS（IMAP 993 / SMTP 465）",
            Security::StartTls => "先明文连接再升级（IMAP 143 / SMTP 587）",
            Security::Plain => "不加密 —— 只用于本机",
        }
    }
}

/// How the provider expects the user to authenticate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthKind {
    /// Ordinary account password.
    Password,
    /// Provider-issued authorization code / app-specific password.
    AppPassword,
    /// OAuth2 bearer token. Not implemented in v0.1 — surfaced so the UI can say so plainly.
    OAuth2,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Region {
    Cn,
    Global,
}

/// A single host/port/security triple.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Endpoint {
    pub host: String,
    pub port: u16,
    pub security: Security,
}

impl Endpoint {
    pub fn new(host: &str, port: u16, security: Security) -> Self {
        Self {
            host: host.to_string(),
            port,
            security,
        }
    }

    pub fn label(&self) -> String {
        format!("{}:{} ({})", self.host, self.port, self.security.label())
    }
}

/// Hand-entered servers for a `custom` account (`design/05` §1.4's manual layer).
///
/// Separate from `Endpoint` because it is *input*: it carries a security choice the user made and has
/// not been validated yet, and `deny_unknown_fields` so a renderer typo is a loud error at the
/// boundary rather than a silently ignored key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EndpointOverride {
    pub imap_host: String,
    pub imap_port: u16,
    pub imap_security: Security,
    pub smtp_host: String,
    pub smtp_port: u16,
    pub smtp_security: Security,
}

/// Why a hand-entered endpoint cannot be used. An enum rather than a string so the tests pin the
/// *reason* and not the wording.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EndpointProblem {
    EmptyHost,
    HostLooksLikeUrl,
    HostCarriesPort,
    HostHasSpace,
    PortOutOfRange,
    PlainOnPublicHost,
}

impl EndpointProblem {
    /// The sentence shown under the field the user has to fix.
    pub fn message(self) -> String {
        match self {
            EndpointProblem::EmptyHost => "服务器地址不能为空。",
            EndpointProblem::HostLooksLikeUrl => {
                "只填主机名，不要带 http:// 或路径 —— 例如 imap.example.com。"
            }
            EndpointProblem::HostCarriesPort => {
                "端口请填到右边的「端口」框里，主机名里不要带端口（IPv6 请写成 [::1]）。"
            }
            EndpointProblem::HostHasSpace => "服务器地址里不能有空格。",
            EndpointProblem::PortOutOfRange => "端口必须在 1–65535 之间。",
            EndpointProblem::PlainOnPublicHost => {
                "「不加密」只允许本机地址；对外部服务器请选 SSL/TLS 或 STARTTLS，否则密码会明文上线。"
            }
        }
        .to_string()
    }
}

/// Clean one hand-entered host, or say why it cannot be used.
///
/// Pasting `imap.example.com:993` or `imaps://imap.example.com/` is what people actually do, and
/// silently accepting either would put a nonsense hostname into `accounts.json` and produce a
/// connection error nobody can read. A scheme is stripped (harmless, obviously a paste); a path, a
/// space, an `@` or an embedded port is refused with a message that names the field to use instead.
pub fn normalise_host(raw: &str) -> Result<String, EndpointProblem> {
    let mut host = raw.trim().to_string();

    // Whatever was pasted, the host is what follows the scheme. All of them are treated alike —
    // `imap://`, `imaps://`, `smtp://`, `https://` — because which one it is says nothing.
    if let Some((_, rest)) = host.split_once("://") {
        // The trailing slash is part of an address-bar paste and of nothing else, so it goes with the
        // scheme. A *path* (`imaps://imap.example.com/mail`) still reaches the check below and is
        // refused, which is the answer a user needs.
        host = rest.trim().trim_end_matches('/').to_string();
    }

    if host.is_empty() {
        return Err(EndpointProblem::EmptyHost);
    }
    if host.contains('/') || host.contains('?') || host.contains('#') || host.contains('@') {
        return Err(EndpointProblem::HostLooksLikeUrl);
    }
    if host.chars().any(char::is_whitespace) {
        return Err(EndpointProblem::HostHasSpace);
    }

    if host.starts_with('[') {
        // The bracketed form is how a literal IPv6 address has to be written; the brackets themselves
        // are punctuation, not part of the address.
        let inner = host
            .strip_prefix('[')
            .and_then(|rest| rest.strip_suffix(']'))
            .ok_or(EndpointProblem::HostCarriesPort)?;
        return Ok(inner.to_ascii_lowercase());
    }
    if host.contains(':') {
        return Err(EndpointProblem::HostCarriesPort);
    }

    Ok(host.to_ascii_lowercase())
}

/// Check the port and the security choice together, because the two only make sense as a pair.
pub fn validate_endpoint(host: &str, port: u16, security: Security) -> Result<(), EndpointProblem> {
    if port == 0 {
        return Err(EndpointProblem::PortOutOfRange);
    }
    // `Security::Plain`'s own documentation says "Localhost only", and it means the password goes up
    // the wire in the clear. A slip of the mouse should be refused here rather than written into
    // `accounts.json` and discovered by a packet capture.
    if security == Security::Plain && !is_local_host(host) {
        return Err(EndpointProblem::PlainOnPublicHost);
    }
    Ok(())
}

/// A port/security pairing that is unusual but not wrong — worth a sentence, not a refusal.
///
/// Plenty of servers do unusual things on purpose, so this never blocks; it is shown under the fields
/// so a typo is caught before a round trip.
pub fn endpoint_advisory(port: u16, security: Security) -> Option<&'static str> {
    match (port, security) {
        (465, Security::StartTls) => Some("465 通常是「直接 TLS」，STARTTLS 一般配 587"),
        (587, Security::Ssl) => Some("587 通常是「STARTTLS」，直接 TLS 一般配 465"),
        (993, Security::StartTls) => Some("993 通常是「直接 TLS」，STARTTLS 一般配 143"),
        (143, Security::Ssl) => Some("143 通常是「STARTTLS」，直接 TLS 一般配 993"),
        _ => None,
    }
}

/// Whether a host is this machine, for the one rule that depends on it.
fn is_local_host(host: &str) -> bool {
    matches!(host.trim(), "127.0.0.1" | "::1" | "localhost" | "[::1]")
}

/// A fully resolved account, ready to connect. Contains **no** secrets.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccountConfig {
    /// Stable local id; also the key under which the password is stored in the credential vault.
    pub id: String,
    /// The full email address, used as the IMAP/SMTP username by default.
    pub email: String,
    /// Shown in the UI and used as the `From` display name.
    pub display_name: String,
    /// Login name when it differs from the email address.
    pub username: String,
    /// Id of the matching entry in the built-in provider database, or `"custom"`.
    pub provider_id: String,
    pub imap: Endpoint,
    pub smtp: Endpoint,
    /// Send the RFC 2971 `ID` command before `LOGIN` (required by NetEase servers).
    pub needs_id_command: bool,
    /// Keep this many simultaneous connections to the server at most. QQ/NetEase throttle hard.
    pub max_connections: u8,
    /// How this account proves who it is.
    ///
    /// `#[serde(default)]` so every account written before OAuth existed — which is every account in
    /// every existing installation — keeps loading as what it is: a password account.
    #[serde(default)]
    pub auth: AuthMethod,
}

/// The two ways an account can authenticate (`design/05` §3.2).
///
/// It lives on the account rather than being inferred at each connection because "does this account
/// use OAuth" is a fact the user chose during setup, and inferring it from, say, the presence of a
/// refresh token would make an account flip behaviour when that file is deleted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthMethod {
    /// A password, or the "authorization code" the Chinese services issue in its place.
    #[default]
    Password,
    /// An OAuth2 access token, renewed from a stored refresh token before each connection.
    OAuth2,
}

impl AccountConfig {
    /// The name to show in the `From` header.
    pub fn from_display(&self) -> String {
        if self.display_name.trim().is_empty() {
            self.email.clone()
        } else {
            self.display_name.clone()
        }
    }
}

/// A selectable mailbox, as returned by `LIST`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MailboxInfo {
    /// The name exactly as it appears on the wire, still in IMAP modified UTF-7.
    ///
    /// This is the form that must be sent back in `SELECT`/`COPY`/`APPEND`. Use it for every
    /// command and never for display.
    pub name: String,
    /// The same name decoded for humans (`&UXZO1mWHTvZZOQ-` becomes 其他文件夹).
    pub display_name: String,
    pub delimiter: Option<String>,
    /// False for pure container nodes that cannot be `SELECT`ed.
    pub selectable: bool,
}

/// One row in the message list. Built from headers only, so it is cheap to fetch in bulk.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessageSummary {
    pub uid: u32,
    pub subject: String,
    pub from_name: Option<String>,
    pub from_addr: Option<String>,
    /// Unix timestamp in seconds, already normalised out of the header's timezone.
    pub date: Option<i64>,
    pub size: Option<u32>,
    pub seen: bool,
    pub flagged: bool,
    /// The thread this message belongs to, from its `References` chain (`crate::threads::root_of`).
    ///
    /// `None` for rows written before this existed, and for a message with no identity at all — the
    /// renderer then falls back to grouping by subject and sender, which is a guess but a labelled
    /// one. Stored rather than recomputed because the list is read from the cache far more often
    /// than it is fetched, and the headers are not kept.
    #[serde(default)]
    pub thread_key: Option<String>,
    /// The **heuristic** conversation signal: normalised subject plus the sender
    /// (`crate::threads::subject_key`), for mail that carries no `References` chain.
    ///
    /// Kept next to `thread_key` rather than folded into it because the two are unioned, not ranked:
    /// the chain is a fact and the subject is a guess, and a message may be grouped by either. Only
    /// mail the user received gets one (see the note on the function).
    ///
    /// `None` for rows written before this existed, and for the messages that must not participate.
    #[serde(default)]
    pub subject_key: Option<String>,
}

/// One message of a conversation, with the folder it lives in (`design/08` §4.2's `ThreadMessage`).
///
/// The folder is the whole point: a conversation crosses folders, because the user's own replies are
/// in the Sent folder. A thread view built from one folder would show an argument in which only one
/// side ever speaks — which is why the backend, and not the renderer, answers "what is in this
/// conversation".
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThreadMember {
    pub folder: String,
    pub summary: MessageSummary,
}

/// Metadata for an attachment. The bytes are fetched separately, on demand.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AttachmentMeta {
    pub name: String,
    pub mime: String,
    /// Size of the decoded content, i.e. what the file will be.
    pub size: usize,
    /// Index used to look the part up again with `Message::attachment` when saving it.
    pub part: u32,
    pub inline: bool,
    /// `Content-ID` for inline parts. HTML refers to these as `cid:` URLs.
    pub content_id: Option<String>,
}

/// An image embedded in the message body rather than attached to it.
///
/// HTML mail almost always references its logos and charts this way (`<img src="cid:...">`).
/// The bytes travel with the message, so rendering them costs no privacy: nothing is fetched
/// from the network and no tracking pixel is triggered. They are handed to the UI as `data:`
/// URIs, which is why the payload is base64.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InlineImage {
    /// Normalised without the angle brackets that `Content-ID` carries on the wire.
    pub content_id: String,
    pub mime: String,
    /// base64 of the raw bytes, ready to paste into a `data:` URI.
    pub data_base64: String,
}

/// A fully parsed message.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessageDetail {
    pub uid: u32,
    pub subject: String,
    pub from_name: Option<String>,
    pub from_addr: Option<String>,
    pub to: Vec<String>,
    pub cc: Vec<String>,
    pub reply_to: Option<String>,
    pub date: Option<i64>,
    pub message_id: Option<String>,
    pub in_reply_to: Option<String>,
    pub references: Vec<String>,
    pub body_text: Option<String>,
    /// Raw HTML. The UI **must** render this inside a sandboxed frame with remote content blocked.
    pub body_html: Option<String>,
    pub attachments: Vec<AttachmentMeta>,
    /// Images referenced by `cid:` from [`MessageDetail::body_html`].
    pub inline_images: Vec<InlineImage>,
}

/// A file to send along with a message.
///
/// Carries the **path**, not the bytes: the file stays on disk and is read when the message is
/// built. Serialising a 20 MB attachment through the IPC boundary as a JSON string would be both
/// slow and pointless when the file is already sitting on the same machine.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OutgoingAttachment {
    /// Filename to present to the recipient. Sent `Content-Disposition`-encoded.
    pub name: String,
    /// Declared MIME type. Falls back to `application/octet-stream` if it will not parse.
    pub mime: String,
    /// Absolute path on this machine.
    pub path: String,
    /// Size in bytes, for the composer to show. Not used when sending.
    #[serde(default)]
    pub size: u64,
    /// SHA-256 of the file when the user chose it.
    ///
    /// The path is not identity: the file can be edited or deleted between adding it and sending,
    /// and delivering the new contents would be sending something the user never approved. Filled
    /// in at pick time, and at send time for a draft that predates this (`design/07` §5.5/§7).
    #[serde(default)]
    pub sha256: Option<String>,
}

/// A message to be sent.
///
/// `deny_unknown_fields` is deliberate: this struct arrives as JSON from the renderer, and an
/// optional field whose key is spelled differently would be *ignored* rather than rejected — a
/// message with `bodyHtml` would quietly go out as plain text only. The keys are compared against
/// the frontend's builders by `tools/check.ps1`; this is what makes the mismatch loud at runtime
/// as well, for any caller the gate cannot see.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OutgoingMail {
    pub to: Vec<String>,
    pub cc: Vec<String>,
    pub bcc: Vec<String>,
    pub subject: String,
    /// Plain-text body. Sent alongside `body_html` as a multipart/alternative when both are set.
    pub body_text: String,
    pub body_html: Option<String>,
    /// Message-Id of the message being replied to, for correct threading.
    pub in_reply_to: Option<String>,
    /// Full `References` chain, oldest first.
    pub references: Vec<String>,
    /// Files to attach. `default` so a message serialised before this existed still loads.
    #[serde(default)]
    pub attachments: Vec<OutgoingAttachment>,
    /// This message's own `Message-ID`, when the sender already fixed one.
    ///
    /// The outbox sets it at enqueue time so the message keeps the same identity across a retry, a
    /// rebuilt spool file and a crash: everything downstream — the Sent-folder copy (`design/07`
    /// §5.4) and the "did it already go out?" check (§5.3.1) — searches by this value. Left unset
    /// for a one-shot send, where [`crate::smtp::build_message`] generates one.
    #[serde(default)]
    pub message_id: Option<String>,
}

/// Result of probing an account's endpoints before saving it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConnectionReport {
    pub imap_ok: bool,
    pub smtp_ok: bool,
    pub imap_detail: String,
    pub smtp_detail: String,
    /// True when the server **answered** and rejected the credential, rather than not answering.
    ///
    /// The two look the same in `imap_ok` and they must not be confused, because the right advice is
    /// opposite. "IMAP 没应答" is usually this network (measured 2026-10-04: Gmail's 993 answers two
    /// times out of five from here) and is worth waiting out. An explicit `AUTHENTICATIONFAILED` is the
    /// server's final answer about *these* credentials, and it never repairs itself: a freshly
    /// authorized OAuth account that gets one is almost always an account whose address does not match
    /// the Google/Microsoft account the browser actually authorized — the wizard takes the address
    /// from a text box and the token from whoever signed in, and nothing used to compare them.
    #[serde(default)]
    pub imap_auth_rejected: bool,
    /// True when SMTP **answered** and rejected the credential, rather than not answering.
    ///
    /// The Microsoft 365 case is why this exists: receiving works, sending is refused by tenant policy,
    /// and "网络恢复后会自动开始收信" is the wrong sentence for it — receiving already works, and what
    /// the user needs to know is that an administrator has to change a setting.
    #[serde(default)]
    pub smtp_auth_rejected: bool,
    /// Mailboxes found, when the IMAP step succeeded.
    pub mailboxes: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pasted_url_is_reduced_to_a_host() {
        // What people actually paste into a "server" box. Each is obviously a paste of an address
        // bar, so the scheme is dropped rather than refused.
        for raw in [
            "imaps://IMAP.Example.com/",
            "imap://imap.example.com",
            "https://imap.example.com/",
            "  imap.example.com  ",
        ] {
            assert_eq!(
                normalise_host(raw).expect(raw),
                "imap.example.com",
                "for {raw}"
            );
        }
    }

    #[test]
    fn a_host_that_carries_its_own_port_is_refused() {
        assert_eq!(
            normalise_host("imap.example.com:993"),
            Err(EndpointProblem::HostCarriesPort)
        );
        // The bracketed form is the documented way to write a literal IPv6 address, and the port still
        // has to go in its own field.
        assert_eq!(normalise_host("[::1]").expect("[::1]"), "::1");
        assert_eq!(
            normalise_host("[::1]:993"),
            Err(EndpointProblem::HostCarriesPort)
        );
    }

    #[test]
    fn an_empty_or_spaced_host_is_refused() {
        assert_eq!(normalise_host(""), Err(EndpointProblem::EmptyHost));
        assert_eq!(normalise_host("   "), Err(EndpointProblem::EmptyHost));
        assert_eq!(normalise_host("imaps://"), Err(EndpointProblem::EmptyHost));
        assert_eq!(
            normalise_host("imap example.com"),
            Err(EndpointProblem::HostHasSpace)
        );
        assert_eq!(
            normalise_host("user@example.com"),
            Err(EndpointProblem::HostLooksLikeUrl)
        );
        assert_eq!(
            normalise_host("imap.example.com/path"),
            Err(EndpointProblem::HostLooksLikeUrl)
        );
    }

    #[test]
    fn plain_security_is_refused_off_the_local_machine() {
        // `Security::Plain` means the password goes up the wire in the clear; its own doc comment says
        // "localhost only", so a slip of the mouse must not be recorded as a working account.
        assert_eq!(
            validate_endpoint("example.com", 143, Security::Plain),
            Err(EndpointProblem::PlainOnPublicHost)
        );
        assert_eq!(validate_endpoint("127.0.0.1", 143, Security::Plain), Ok(()));
        assert_eq!(
            validate_endpoint("localhost", 1143, Security::Plain),
            Ok(())
        );
        assert_eq!(validate_endpoint("::1", 1143, Security::Plain), Ok(()));
    }

    #[test]
    fn port_zero_is_refused() {
        assert_eq!(
            validate_endpoint("example.com", 0, Security::Ssl),
            Err(EndpointProblem::PortOutOfRange)
        );
    }

    #[test]
    fn the_usual_port_and_security_pairings_are_not_flagged() {
        for (port, security) in [
            (993, Security::Ssl),
            (143, Security::StartTls),
            (465, Security::Ssl),
            (587, Security::StartTls),
        ] {
            assert_eq!(
                endpoint_advisory(port, security),
                None,
                "{port} with {} is ordinary",
                security.label()
            );
        }
    }

    #[test]
    fn an_unusual_pairing_is_mentioned_but_not_refused() {
        // Servers do unusual things on purpose, so this is a sentence under the field and never a
        // refusal — the connection test is what decides.
        for (port, security) in [(465, Security::StartTls), (587, Security::Ssl)] {
            assert!(
                endpoint_advisory(port, security).is_some(),
                "{port} with {} is worth a sentence",
                security.label()
            );
            assert_eq!(validate_endpoint("example.com", port, security), Ok(()));
        }
    }

    #[test]
    fn every_security_choice_has_a_label_and_a_hint() {
        // The wizard renders these and the wire value comes from serde; a new variant must not be able
        // to appear in the dropdown without either.
        assert_eq!(Security::ALL.len(), 3);
        for security in Security::ALL {
            assert!(!security.label().is_empty());
            assert!(!security.hint().is_empty());
        }
    }
}
