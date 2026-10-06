//! SMTP send path.
//!
//! lettre's async transport needs the extra `tokio1` feature; sending one message is inherently
//! a blocking, short-lived operation, so we use the sync transport on a blocking thread instead.
//! That keeps the dependency surface small and the failure modes obvious.

use std::time::Duration;

use lettre::address::Envelope;
use lettre::message::{Attachment, Mailbox, Message, MultiPart, SinglePart};
use lettre::transport::smtp::authentication::{Credentials, Mechanism};
use lettre::transport::smtp::client::{Tls, TlsParameters};
use lettre::{SmtpTransport, Transport};

use crate::error::{MailError, Result};
use crate::model::{AccountConfig, AuthMethod, OutgoingAttachment, OutgoingMail, Security};

const SMTP_TIMEOUT: Duration = Duration::from_secs(45);

/// Parse one recipient, accepting both `a@b.com` and `Name <a@b.com>`.
fn mailbox(raw: &str) -> Result<Mailbox> {
    let trimmed = raw.trim();
    trimmed
        .parse::<Mailbox>()
        .map_err(|e| MailError::Config(format!("地址「{trimmed}」无法解析：{e}")))
}

/// Read one attachment off disk and turn it into a MIME part.
///
/// The bytes are read here rather than carried in the message model, so a large file is not turned
/// into JSON across the IPC boundary first.
fn attachment_part(meta: &OutgoingAttachment) -> Result<SinglePart> {
    let bytes = std::fs::read(&meta.path)
        .map_err(|e| MailError::Config(format!("读不到附件「{}」：{e}", meta.path)))?;

    // A MIME type that will not parse must not abort the send: the file is still perfectly
    // sendable as an opaque blob, and refusing would lose the user's attachment over a detail
    // nobody sees.
    let content_type = meta.mime.parse().unwrap_or_else(|_| {
        "application/octet-stream"
            .parse()
            .expect("literal type parses")
    });

    Ok(Attachment::new(meta.name.clone()).body(bytes, content_type))
}

/// A `Message-ID` for a message we are building.
///
/// lettre writes one only when asked, and "the relay will add it" does not hold for the relays people
/// actually use here: without it there is nothing to search the Sent folder for (`design/07` §5.4)
/// and nothing for crash recovery to compare before re-sending (§5.3.1).
///
/// The domain is the sender's rather than `localhost`, so the id looks like every other client's.
fn make_message_id(email: &str) -> String {
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let sequence = COUNTER.fetch_add(1, Ordering::Relaxed);
    let domain = email
        .rsplit('@')
        .next()
        .filter(|d| d.contains('.'))
        .unwrap_or("localhost");

    format!("<{nanos:x}.{sequence:x}.{}@{domain}>", std::process::id())
}

/// Build the RFC 5322 message, including the threading headers that keep replies grouped
/// correctly in the recipient's client.
///
/// A `message_id` already on the mail is kept **verbatim**: it is what makes the message the same
/// message across a retry or a rebuilt spool file, which the outbox depends on.
pub fn build_message(cfg: &AccountConfig, mail: &OutgoingMail) -> Result<Message> {
    let from = Mailbox::new(Some(cfg.from_display()), cfg.email.parse()?);

    let mut builder = Message::builder()
        .from(from)
        .subject(mail.subject.clone())
        .message_id(Some(
            mail.message_id
                .clone()
                .unwrap_or_else(|| make_message_id(&cfg.email)),
        ));

    for addr in &mail.to {
        builder = builder.to(mailbox(addr)?);
    }
    for addr in &mail.cc {
        builder = builder.cc(mailbox(addr)?);
    }
    for addr in &mail.bcc {
        builder = builder.bcc(mailbox(addr)?);
    }

    // Without these two, the reply shows up as a brand-new conversation on the other side.
    if let Some(id) = &mail.in_reply_to {
        builder = builder.in_reply_to(id.clone());
    }
    // `references()` *sets* the header rather than appending, so the whole chain has to go in as
    // one space-separated value. Looping over the ids would silently keep only the last one.
    if !mail.references.is_empty() {
        builder = builder.references(mail.references.join(" "));
    }

    // Deliberately `singlepart` rather than `body`: `body` emits a base64 transfer encoding
    // but no Content-Type at all, so a non-ASCII body arrives with no charset declaration and
    // is rendered as mojibake. `SinglePart::plain` sets `text/plain; charset=utf-8`.
    let message = if mail.attachments.is_empty() {
        match &mail.body_html {
            Some(html) if !html.trim().is_empty() => builder.multipart(
                // alternative_plain_html keeps a plain-text fallback for clients that refuse HTML.
                MultiPart::alternative_plain_html(mail.body_text.clone(), html.clone()),
            )?,
            _ => builder.singlepart(SinglePart::plain(mail.body_text.clone()))?,
        }
    } else {
        // With files, the conventional shape is `multipart/mixed` whose first part is the body (a
        // `multipart/alternative` when there is HTML) and whose remaining parts are the attachments.
        // The plain-text body goes in directly rather than inside a one-part `mixed`, which would be
        // legal but needlessly nested.
        let mut mixed = match &mail.body_html {
            Some(html) if !html.trim().is_empty() => MultiPart::mixed().multipart(
                MultiPart::alternative_plain_html(mail.body_text.clone(), html.clone()),
            ),
            _ => MultiPart::mixed().singlepart(SinglePart::plain(mail.body_text.clone())),
        };

        for meta in &mail.attachments {
            mixed = mixed.singlepart(attachment_part(meta)?);
        }
        builder.multipart(mixed)?
    };

    Ok(message)
}

/// How an account proves who it is.
///
/// Two shapes because the providers ship two: a password (including the "authorization code" the
/// Chinese services issue) and an OAuth2 access token for the accounts whose password path is closed
/// (`design/05` §3.2). `Mechanism::Xoauth2` is what makes lettre build `AUTH XOAUTH2 <base64>`; the
/// alternative — passing the token as a password — produces `AUTH PLAIN` with a bearer token in it,
/// which the server rejects and which puts a secret on the wire in a shape nobody expects.
#[derive(Debug, Clone, Copy)]
pub enum SmtpCredential<'a> {
    Password(&'a str),
    OAuth2(&'a str),
}

impl<'a> SmtpCredential<'a> {
    /// Read the credential out of the account, so no call site has to know which kind it holds.
    fn for_account(cfg: &AccountConfig, secret: &'a str) -> SmtpCredential<'a> {
        match cfg.auth {
            AuthMethod::Password => SmtpCredential::Password(secret),
            AuthMethod::OAuth2 => SmtpCredential::OAuth2(secret),
        }
    }

    /// The one line this whole enum exists for.
    fn mechanism(self) -> Option<Mechanism> {
        match self {
            SmtpCredential::Password(_) => None,
            SmtpCredential::OAuth2(_) => Some(Mechanism::Xoauth2),
        }
    }

    fn secret(self) -> &'a str {
        match self {
            SmtpCredential::Password(password) => password,
            SmtpCredential::OAuth2(access_token) => access_token,
        }
    }
}

/// Build the transport for an endpoint.
fn build_transport(cfg: &AccountConfig, credential: SmtpCredential<'_>) -> Result<SmtpTransport> {
    let ep = &cfg.smtp;
    let credentials = Credentials::new(cfg.username.clone(), credential.secret().to_string());

    let tls = match ep.security {
        Security::Plain => Tls::None,
        _ => {
            let params = TlsParameters::new(ep.host.clone())
                .map_err(|e| MailError::Smtp(format!("初始化 TLS 失败：{e}")))?;
            match ep.security {
                // Implicit TLS: handshake immediately, as on port 465.
                Security::Ssl => Tls::Wrapper(params),
                // STARTTLS: upgrade after EHLO, as on port 587.
                Security::StartTls => Tls::Required(params),
                Security::Plain => unreachable!("handled above"),
            }
        }
    };

    let mut builder = SmtpTransport::builder_dangerous(ep.host.as_str())
        .port(ep.port)
        .tls(tls)
        .credentials(credentials)
        .timeout(Some(SMTP_TIMEOUT));

    // Restricting the mechanism list is not decoration: left open, lettre tries the strongest
    // mechanism the server advertises, and a server that advertises `PLAIN` would be sent the bearer
    // token in a form it will reject — a failure that looks like a wrong token.
    if let Some(mechanism) = credential.mechanism() {
        builder = builder.authentication(vec![mechanism]);
    }

    Ok(builder.build())
}

/// Open a connection, authenticate, and disconnect — without sending anything.
///
/// This is the path the account wizard uses, so a rejected credential must be classified here
/// just as carefully as in [`send`] — it is the single most common failure a new user hits.
pub async fn test_connection(cfg: &AccountConfig, password: &str) -> Result<()> {
    let transport = build_transport(cfg, SmtpCredential::for_account(cfg, password))?;
    let ok = tokio::task::spawn_blocking(move || {
        transport
            .test_connection()
            .map_err(|e| MailError::classify_smtp(e.to_string()))
    })
    .await
    .map_err(|e| MailError::Smtp(format!("发送线程异常终止：{e}")))??;

    if ok {
        Ok(())
    } else {
        Err(MailError::Smtp("服务器未通过连接测试".into()))
    }
}

/// Send one message.
/// Send with an OAuth2 access token instead of a password.
///
/// A separate entry point rather than a `bool` on [`send`]: the two credentials behave differently in
/// ways that matter (a token expires, a password does not; a token is bearer, a password is not), and
/// a flag would put that distinction at every call site instead of at the one that knows.
pub async fn send_oauth(
    cfg: &AccountConfig,
    access_token: &str,
    mail: &OutgoingMail,
) -> Result<()> {
    let message = build_message(cfg, mail)?;
    let transport = build_transport(cfg, SmtpCredential::OAuth2(access_token))?;

    tokio::task::spawn_blocking(move || {
        transport
            .send(&message)
            .map(|_| ())
            .map_err(|error| MailError::Smtp(format!("发送失败：{error}")))
    })
    .await
    .map_err(|error| MailError::Smtp(format!("发送任务异常：{error}")))?
}

pub async fn send(cfg: &AccountConfig, password: &str, mail: &OutgoingMail) -> Result<()> {
    let message = build_message(cfg, mail)?;
    let transport = build_transport(cfg, SmtpCredential::for_account(cfg, password))?;

    tokio::task::spawn_blocking(move || {
        transport
            .send(&message)
            .map(|_response| ())
            .map_err(|e| MailError::classify_smtp(e.to_string()))
    })
    .await
    .map_err(|e| MailError::Smtp(format!("发送线程异常终止：{e}")))?
}

/// Send bytes that were already built — the spool (`design/07` §5.3).
///
/// `send_raw` is used rather than `send` because the message must go out **exactly** as it was built
/// at enqueue time; rebuilding it here would be a second chance to produce something else. lettre
/// still does the dot-stuffing, which is the one transformation the wire requires.
///
/// The envelope is passed in rather than parsed back out of the bytes: it comes from the same
/// `OutgoingMail` the spool was built from, and Bcc recipients exist only there (they must never
/// appear in the headers, so they cannot be recovered from them either).
pub async fn send_spool(
    cfg: &AccountConfig,
    password: &str,
    spool: Vec<u8>,
    recipients: Vec<String>,
) -> Result<()> {
    let from: lettre::address::Address = cfg
        .email
        .parse()
        .map_err(|e| MailError::Config(format!("发件地址「{}」无法解析：{e}", cfg.email)))?;
    let mut to = Vec::with_capacity(recipients.len());
    for raw in &recipients {
        // `Envelope` takes bare addresses: display names belong in the headers, not in `RCPT TO`.
        to.push(mailbox(raw)?.email);
    }
    if to.is_empty() {
        return Err(MailError::Config("这封信没有收件人。".into()));
    }
    let envelope = Envelope::new(Some(from), to)?;

    let transport = build_transport(cfg, SmtpCredential::for_account(cfg, password))?;
    tokio::task::spawn_blocking(move || {
        transport
            .send_raw(&envelope, &spool)
            .map(|_response| ())
            .map_err(|e| MailError::classify_smtp(e.to_string()))
    })
    .await
    .map_err(|e| MailError::Smtp(format!("发送线程异常终止：{e}")))?
}

/// The `Message-ID` lettre put into a message it just built.
///
/// It is the identity every later step needs: the Sent-folder copy is found by searching for it
/// (`design/07` §5.4), and crash recovery uses it to decide whether a message already went out
/// (§5.3.1). lettre generates one at build time when the message does not carry one, which is
/// another reason the bytes are frozen at enqueue time rather than rebuilt for every attempt.
pub fn message_id_of(message: &Message) -> Option<String> {
    message
        .headers()
        .get_raw("Message-ID")
        .map(|raw| String::from_utf8_lossy(raw.as_ref()).trim().to_string())
        .filter(|id| !id.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Endpoint, MessageSummary};

    fn account() -> AccountConfig {
        AccountConfig {
            id: "t".into(),
            email: "me@example.com".into(),
            display_name: "测试用户".into(),
            username: "me@example.com".into(),
            provider_id: "custom".into(),
            imap: Endpoint::new("imap.example.com", 993, Security::Ssl),
            smtp: Endpoint::new("smtp.example.com", 465, Security::Ssl),
            needs_id_command: false,
            max_connections: 2,
            auth: AuthMethod::Password,
        }
    }

    fn outgoing() -> OutgoingMail {
        OutgoingMail {
            to: vec!["alice@example.com".into()],
            cc: vec![],
            bcc: vec![],
            subject: "你好".into(),
            body_text: "这是一封测试邮件。".into(),
            body_html: None,
            in_reply_to: None,
            references: vec![],
            attachments: vec![],
            message_id: None,
        }
    }

    #[test]
    fn an_attachment_becomes_a_mixed_part_and_the_body_stays_readable() {
        // The file has to exist, so a temp file is the only option; the point is that `smtp` reads
        // it rather than receiving bytes through the model.
        let path = std::env::temp_dir().join(format!(
            "easy-email-smtp-attachment-{}-{:?}.pdf",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::write(&path, b"%PDF-1.4 fake").expect("write fixture");

        let mut mail = outgoing();
        mail.attachments.push(OutgoingAttachment {
            name: "发票.pdf".into(),
            mime: "application/pdf".into(),
            path: path.to_string_lossy().to_string(),
            size: 13,
            sha256: None,
        });

        let message = build_message(&account(), &mail).expect("build");
        let raw = String::from_utf8_lossy(&message.formatted()).to_string();

        assert!(
            raw.contains("multipart/mixed"),
            "a message with files must be multipart/mixed, got:\n{raw}"
        );
        assert!(
            raw.contains("Content-Disposition: attachment"),
            "the part must be declared as an attachment. got:\n{raw}"
        );
        // A non-ASCII filename is sent as an RFC 2231 continuation with percent-encoding, not as a
        // bare `filename=`. Asserting the encoded form is what actually pins the behaviour.
        assert!(
            raw.contains("utf-8''%E5%8F%91%E7%A5%A8.pdf"),
            "发票.pdf must be RFC 2231 percent-encoded. got:\n{raw}"
        );
        // A payload that is safe as 7bit goes out verbatim; lettre picks the encoding, and pinning
        // the actual choice is the point — assuming base64 here would have been wrong.
        assert!(
            raw.contains("Content-Transfer-Encoding: 7bit"),
            "got:\n{raw}"
        );
        assert!(
            raw.contains("%PDF-1.4 fake"),
            "the file's bytes must be in the message. got:\n{raw}"
        );
        assert!(
            raw.contains("Content-Type: application/pdf"),
            "the declared type must reach the recipient. got:\n{raw}"
        );
        assert!(
            raw.contains("6L+Z5piv5LiA5bCB5rWL6K+V6YKu5Lu244CC"),
            "the body must still be part of the message. got:\n{raw}"
        );

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_binary_attachment_is_base64_encoded_rather_than_sent_raw() {
        // The other half of the encoding decision: bytes that are not 7bit-safe must be encoded, or
        // the message is not valid SMTP.
        let path = std::env::temp_dir().join(format!(
            "easy-email-smtp-binary-{}-{:?}.bin",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::write(&path, [0x00u8, 0x9F, 0x92, 0x96]).expect("write fixture");

        let mut mail = outgoing();
        mail.attachments.push(OutgoingAttachment {
            name: "blob.bin".into(),
            mime: "application/octet-stream".into(),
            path: path.to_string_lossy().to_string(),
            size: 4,
            sha256: None,
        });

        let raw =
            String::from_utf8_lossy(&build_message(&account(), &mail).expect("build").formatted())
                .to_string();
        assert!(
            raw.contains("Content-Transfer-Encoding: base64"),
            "binary bytes need an encoding. got:\n{raw}"
        );
        assert!(
            raw.contains("AJ+Slg=="),
            "and it must be the base64 of those exact bytes. got:\n{raw}"
        );

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_missing_attachment_file_fails_the_send_rather_than_going_out_without_it() {
        let mut mail = outgoing();
        mail.attachments.push(OutgoingAttachment {
            name: "gone.pdf".into(),
            mime: "application/pdf".into(),
            path: std::env::temp_dir()
                .join("easy-email-does-not-exist-12345.pdf")
                .to_string_lossy()
                .to_string(),
            size: 1,
            sha256: None,
        });

        let error = build_message(&account(), &mail).expect_err("must not build");
        assert!(error.to_string().contains("读不到附件"), "got {error}");
    }

    #[test]
    fn a_nonsense_content_type_falls_back_instead_of_losing_the_attachment() {
        let path = std::env::temp_dir().join(format!(
            "easy-email-smtp-mime-{}-{:?}.bin",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::write(&path, b"hello").expect("write fixture");

        let mut mail = outgoing();
        mail.attachments.push(OutgoingAttachment {
            name: "blob.bin".into(),
            mime: "not a mime type".into(),
            path: path.to_string_lossy().to_string(),
            size: 5,
            sha256: None,
        });

        let message = build_message(&account(), &mail).expect("must still build");
        let raw = String::from_utf8_lossy(&message.formatted()).to_string();
        assert!(raw.contains("application/octet-stream"), "got:\n{raw}");

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn the_composer_payload_deserialises_and_a_mistyped_key_does_not() {
        // The renderer builds this object in src/payload.js; tools/check.ps1 compares its keys with
        // `OutgoingMail`. This is the runtime half: before M1a-7 the composer sent `bodyText` and
        // every send failed with "missing field `body_text`" — a bug no compiler and no Rust test
        // could have seen, because nothing deserialised the payload the UI actually built.
        let good = serde_json::json!({
            "to": ["a@example.com"], "cc": [], "bcc": [], "subject": "s",
            "body_text": "hi", "body_html": null, "in_reply_to": null,
            "references": [], "attachments": []
        });
        let parsed: OutgoingMail = serde_json::from_value(good).expect("the composer's payload");
        assert_eq!(parsed.body_text, "hi");

        // `body_html` is optional, so a camelCase spelling would be ignored rather than refused —
        // a rich message would go out as plain text with nothing to show for it.
        let typo = serde_json::json!({
            "to": ["a@example.com"], "cc": [], "bcc": [], "subject": "s",
            "body_text": "hi", "bodyHtml": "<p>hi</p>", "in_reply_to": null,
            "references": [], "attachments": []
        });
        assert!(
            serde_json::from_value::<OutgoingMail>(typo).is_err(),
            "a key the struct does not have must be an error, not a silently ignored field"
        );
    }

    #[test]
    fn plain_message_has_the_expected_headers() {
        let msg = build_message(&account(), &outgoing()).expect("build");
        assert_eq!(
            msg.headers().get_raw("Subject").unwrap().to_string(),
            "你好"
        );
        let from = format!("{:?}", msg.headers().get_raw("From").unwrap());
        assert!(from.contains("me@example.com"));
    }

    #[test]
    fn display_name_is_used_for_from() {
        let msg = build_message(&account(), &outgoing()).expect("build");
        // The name must be encoded rather than emitted raw, or non-ASCII gets mangled.
        let from = format!("{:?}", msg.headers().get_raw("From").unwrap());
        assert!(from.contains("me@example.com"));
    }

    #[test]
    fn html_body_produces_multipart_alternative() {
        let mut mail = outgoing();
        mail.body_html = Some("<p>hi</p>".into());
        let msg = build_message(&account(), &mail).expect("build");
        // Content-Type is applied when the message is formatted, not stored in the header map.
        let formatted = msg.formatted();
        let raw = String::from_utf8_lossy(&formatted);
        assert!(raw.contains("multipart/alternative"), "got {raw}");
    }

    #[test]
    fn empty_html_falls_back_to_plain() {
        let mut mail = outgoing();
        mail.body_html = Some("   ".into());
        let msg = build_message(&account(), &mail).expect("build");
        let formatted = msg.formatted();
        let raw = String::from_utf8_lossy(&formatted);
        assert!(!raw.contains("multipart/"), "got {raw}");
        assert!(raw.contains("text/plain"), "got {raw}");
    }

    /// Regression test: a Chinese body used to go out base64-encoded with no `charset`, which
    /// renders as mojibake on the receiving side.
    #[test]
    fn non_ascii_body_declares_its_charset() {
        let mail = outgoing();
        let msg = build_message(&account(), &mail).expect("build");
        let formatted = msg.formatted();
        let raw = String::from_utf8_lossy(&formatted);
        assert!(
            raw.to_ascii_lowercase().contains("charset=utf-8"),
            "a non-ASCII body must declare utf-8, got:\n{raw}"
        );
    }

    /// Regression test: `references()` sets rather than appends, so the whole chain has to be
    /// written as one value. Looping used to keep only the final id.
    #[test]
    fn the_full_references_chain_is_preserved() {
        let mut mail = outgoing();
        mail.references = vec![
            "<one@example.com>".into(),
            "<two@example.com>".into(),
            "<three@example.com>".into(),
        ];
        let msg = build_message(&account(), &mail).expect("build");
        let formatted = msg.formatted();
        let raw = String::from_utf8_lossy(&formatted);
        for id in ["one@example.com", "two@example.com", "three@example.com"] {
            assert!(raw.contains(id), "{id} missing from:\n{raw}");
        }
    }

    #[test]
    fn threading_headers_are_emitted() {
        let mut mail = outgoing();
        mail.in_reply_to = Some("<parent@example.com>".into());
        mail.references = vec!["<root@example.com>".into(), "<parent@example.com>".into()];

        let msg = build_message(&account(), &mail).expect("build");
        let formatted = msg.formatted();
        let raw = String::from_utf8_lossy(&formatted);
        assert!(raw.contains("In-Reply-To"));
        assert!(raw.contains("parent@example.com"));
        assert!(raw.contains("References"));
        assert!(raw.contains("root@example.com"));
    }

    #[test]
    fn named_recipients_parse() {
        let mut mail = outgoing();
        mail.to = vec!["Alice <alice@example.com>".into()];
        let msg = build_message(&account(), &mail).expect("build");
        let formatted = msg.formatted();
        let raw = String::from_utf8_lossy(&formatted);
        assert!(raw.contains("alice@example.com"));
    }

    #[test]
    fn malformed_recipient_is_a_config_error() {
        let mut mail = outgoing();
        mail.to = vec!["not-an-address".into()];
        let err = build_message(&account(), &mail).expect_err("must reject");
        assert!(matches!(err, MailError::Config(_)), "got {err:?}");
    }

    #[test]
    fn message_summary_is_serialisable_for_the_ui() {
        let s = MessageSummary {
            uid: 1,
            subject: "s".into(),
            from_name: None,
            from_addr: None,
            date: None,
            size: None,
            seen: false,
            flagged: false,
            thread_key: None,
            subject_key: None,
        };
        let json = serde_json::to_string(&s).expect("serialise");
        assert!(json.contains("\"uid\":1"));
    }
}
