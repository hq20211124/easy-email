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
    /// Mailboxes found, when the IMAP step succeeded.
    pub mailboxes: Vec<String>,
}
