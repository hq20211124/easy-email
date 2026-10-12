//! Protocol layer for Easy Email.
//!
//! The crate is deliberately UI-free so the whole mail path can be exercised with `cargo test`
//! and a throwaway mailbox, without launching the desktop shell.
//!
//! Layers:
//! - [`providers`] — the built-in server database and its per-provider quirks.
//! - [`model`] — serialisable types shared with the UI.
//! - [`folders`] — human-readable folder names and sidebar order.
//! - [`imap`] — connect, list folders, list messages, read, flag, delete, move.
//! - [cache] — local SQLite copy of what has been synced, plus the FTS5 search index.
//! - [`sync`] — incremental folder sync on top of the cache.
//! - [pool] — reuse IMAP connections instead of handshaking per command.
//! - [idle] — IMAP IDLE push, so new mail arrives without polling.
//! - [index] — a bounded pass that fetches unread bodies so search can see them.
//! - [`smtp`] — compose and send, including threading headers.
//! - [`secret`] — Windows Credential Manager access.

pub mod attach;
pub mod backfill;
pub mod cache;
pub mod db;
pub mod diag;
pub mod error;
pub mod folders;
pub mod idle;
pub mod imap;
pub mod index;
pub mod links;
pub mod logrot;
pub mod model;
pub mod mutf7;
pub mod oauth;
pub mod oauthclient;
pub mod outbox;
pub mod pending;
pub mod pool;
pub mod providers;
pub mod query;
pub mod redact;
pub mod sanitize;
pub mod secret;
pub mod sentcopy;
pub mod size;
pub mod smtp;
pub mod spool;
pub mod sync;
pub mod threads;
pub mod tokenstore;
pub mod update;
pub mod userdb;
pub mod worker;

pub use attach::{AttachmentIssue, check_attachment, file_sha256, prepare_attachments};
pub use backfill::{
    BackfillBudget, BackfillReport, BackfillStop, backfill_older, fill_thread_keys,
};
pub use cache::{
    BodyCoverage, Cache, FolderState, HealReport, IndexHealth, RebuildProgress, SearchHit, to_query,
};
pub use error::{AuthHint, MailError, Result};
pub use idle::{PushEvent, watch, watch_every};
pub use imap::Credential;
pub use imap::{HeaderPage, ImapClient};
pub use index::{BodyIndexRun, IndexReport, PendingBody, index_bodies_pass, pick_pending};
pub use model::{
    AccountConfig, AttachmentMeta, AuthKind, AuthMethod, ConnectionReport, Endpoint,
    EndpointOverride, EndpointProblem, InlineImage, MailboxInfo, MessageDetail, MessageSummary,
    OutgoingAttachment, OutgoingMail, Region, Security, endpoint_advisory, normalise_host,
    validate_endpoint,
};
pub use outbox::{
    OutboxState, Verdict, is_permanent_failure, needs_user_decision, next_attempt_delay_secs,
    verdict_for_failure,
};
pub use pending::{DEFAULT_REPLAY_LIMIT, OpApplier, ReplayReport, ReplayRun, replay_pending_ops};
pub use pool::{ConnectionPool, PooledClient};
pub use providers::Provider;
pub use query::{
    QueryColumn, QueryTerm, known_prefixes, match_expression, parse_date, parse_query, query_issues,
};
pub use sentcopy::{DraftMirrorReport, SentCopy, draft_to_mail, ensure_sent_copy, mirror_draft};
pub use spool::Spool;
pub use sync::{Fetch, SyncDriver, SyncOutcome, SyncWindow};
pub use threads::root_of;
pub use userdb::{Draft, DraftMirror, NewOutbox, OutboxItem, UserStore};
pub use worker::{
    DEFAULT_CRASH_RESEND_LIMIT, FilingReport, FilingRun, OutboxRun, Recovery, RecoveryQuestion,
    RecoveryReport, RecoveryRun, RunReport, file_sent_copies, recover_in_flight, run_due,
};

/// Build a ready-to-use account from a provider preset.
///
/// `provider_id` is one of [`providers::all`]'s ids. The email address doubles as the login name
/// and as the stable local id, so re-adding the same address updates the existing account rather
/// than creating a duplicate.
///
/// Use [`account_with_login`] when the login name is not the address.
pub fn account_from_provider(
    provider_id: &str,
    email: &str,
    display_name: &str,
) -> Result<AccountConfig> {
    account_with_login(provider_id, email, display_name, None)
}

/// As [`account_from_provider`], but with an explicit login name.
///
/// The two really can differ, and when they do the address alone cannot work:
///  * a `@foxmail.com` alias of a QQ account may only authenticate as the QQ number;
///  * VIP and enterprise accounts are often issued a login name distinct from the address.
///
/// The address stays the `From` identity either way — logging in as one account and sending as
/// another is the whole point of the override.
/// As [`account_with_login`], but a provider whose servers are the user's to fill in takes them from
/// `endpoints` instead of the table.
///
/// **Validation happens before anything is built**, and an override for a table provider is refused
/// rather than ignored: a frontend that sent one by mistake would otherwise look like "my custom host
/// was silently dropped", which is a bug report nobody can act on.
pub fn account_with_endpoints(
    provider_id: &str,
    email: &str,
    display_name: &str,
    login: Option<&str>,
    endpoints: Option<&EndpointOverride>,
) -> Result<AccountConfig> {
    let provider = providers::by_id(provider_id)
        .ok_or_else(|| MailError::Config(format!("未知的邮箱服务商：{provider_id}")))?;

    if !provider.supports_imap {
        return Err(MailError::Config(format!(
            "{} 不提供 IMAP/SMTP，无法作为桌面邮件客户端账号使用。",
            provider.display_name
        )));
    }

    let email = email.trim();
    if !email.contains('@') {
        return Err(MailError::Config(format!(
            "「{email}」不是有效的邮箱地址。"
        )));
    }

    let (imap, smtp) = match (provider.manual_endpoints, endpoints) {
        (false, None) => (provider.imap_endpoint(), provider.smtp_endpoint()),
        (false, Some(_)) => {
            return Err(MailError::Config(format!(
                "{} 的服务器地址是内置的，不能手动覆盖。",
                provider.display_name
            )));
        }
        (true, None) => {
            return Err(MailError::Config(
                "自定义服务器需要填写 IMAP 与 SMTP 地址。".into(),
            ));
        }
        (true, Some(o)) => {
            let imap_host =
                normalise_host(&o.imap_host).map_err(|p| MailError::Config(p.message()))?;
            validate_endpoint(&imap_host, o.imap_port, o.imap_security)
                .map_err(|p| MailError::Config(p.message()))?;
            let smtp_host =
                normalise_host(&o.smtp_host).map_err(|p| MailError::Config(p.message()))?;
            validate_endpoint(&smtp_host, o.smtp_port, o.smtp_security)
                .map_err(|p| MailError::Config(p.message()))?;
            (
                Endpoint::new(&imap_host, o.imap_port, o.imap_security),
                Endpoint::new(&smtp_host, o.smtp_port, o.smtp_security),
            )
        }
    };

    let login = login.map(str::trim).filter(|s| !s.is_empty());
    // A login name that is not an address is unusual but legal (some enterprise servers use a
    // bare account id), so it is only required to be non-empty.
    let username = login.unwrap_or(email).to_string();

    Ok(AccountConfig {
        id: email.to_ascii_lowercase(),
        email: email.to_string(),
        display_name: display_name.trim().to_string(),
        username,
        provider_id: provider.id.to_string(),
        imap,
        smtp,
        needs_id_command: provider.quirks.needs_id,
        max_connections: provider.quirks.max_connections,
        // The password path; the wizard switches this to `OAuth2` when the user signs in with a
        // provider that requires it.
        auth: AuthMethod::Password,
        smtp_none: false,
    })
}

/// The same, for the providers whose servers the table already knows.
///
/// A thin wrapper so the existing callers and tests keep exercising **the table-driven path** — they
/// are the coverage for "the built-in rows are right", and routing them through the endpoint-taking
/// version with `None` is what keeps that true without changing any of them.
pub fn account_with_login(
    provider_id: &str,
    email: &str,
    display_name: &str,
    login: Option<&str>,
) -> Result<AccountConfig> {
    account_with_endpoints(provider_id, email, display_name, login, None)
}

/// Probe both endpoints with a real login before the account is saved.
///
/// Never fails: a partial success is useful information, so problems are reported per-endpoint
/// along with the actionable hint from [`MailError::user_hint`].
pub async fn verify_account(cfg: &AccountConfig, password: &str) -> ConnectionReport {
    let mut report = ConnectionReport {
        imap_ok: false,
        smtp_ok: false,
        imap_detail: String::new(),
        smtp_detail: String::new(),
        imap_auth_rejected: false,
        smtp_auth_rejected: false,
        mailboxes: Vec::new(),
    };

    match ImapClient::connect(cfg, password).await {
        Ok(mut client) => {
            report.imap_ok = true;
            report.imap_detail = "IMAP 登录成功".to_string();

            match client.list_mailboxes().await {
                Ok(boxes) => {
                    report.imap_detail = format!("IMAP 登录成功，发现 {} 个文件夹", boxes.len());
                    // Display names: this ends up in front of the user.
                    report.mailboxes = boxes.into_iter().map(|b| b.display_name).collect();
                }
                Err(e) => {
                    report.imap_detail = format!("IMAP 登录成功，但读取文件夹失败：{e}");
                }
            }
            let _ = client.logout().await;
        }
        Err(e) => {
            // A rejection is the server's answer; everything else is the network's silence. The
            // caller decides differently on the two, so the distinction is recorded here, where the
            // raw error is still available.
            report.imap_auth_rejected = matches!(e, MailError::Auth { .. });
            report.imap_detail = describe(&e);
        }
    }

    // Only try SMTP once the credential is known good.
    //
    // The same credential is used for both services, so a failed IMAP login means the SMTP
    // attempt is certain to fail too — and it would still count as a failed authentication
    // against providers that rate-limit logins. QQ says so in its own rejection text
    // ("login frequency limited"), and burning a second attempt per try makes that worse.
    if !report.imap_ok {
        report.smtp_detail =
            "已跳过 SMTP 检查 —— IMAP 认证未通过，同一个凭据去连 SMTP 也一定会失败，\
             而且每次尝试都会计入服务商的登录频率限制。"
                .to_string();
        return report;
    }

    match smtp::test_connection(cfg, password).await {
        Ok(()) => {
            report.smtp_ok = true;
            report.smtp_detail = "SMTP 连接与认证成功".to_string();
        }
        Err(e) => {
            // Same distinction as IMAP above: an answered rejection is the server's verdict and does
            // not repair itself, while silence is usually the network. The wizard says different
            // things about the two (see `src/wizard.js`).
            report.smtp_auth_rejected = matches!(e, MailError::Auth { .. });
            report.smtp_detail = describe(&e);
        }
    }

    report
}

/// Technical cause plus, when we have one, the thing the user should actually do about it.
fn describe(e: &MailError) -> String {
    match e.user_hint() {
        Some(hint) => format!("{e}\n{hint}"),
        None => e.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn login_name_defaults_to_the_address() {
        let cfg = account_from_provider("qq", "a@qq.com", "").expect("preset");
        assert_eq!(cfg.username, "a@qq.com");
        assert_eq!(cfg.email, "a@qq.com");
    }

    #[test]
    fn login_name_can_differ_from_the_address() {
        // The case that motivated this: a @foxmail.com alias of a QQ account that only
        // authenticates as the QQ number. Keep sending as the alias, log in as the number.
        let cfg = account_with_login("qq", "alias@foxmail.com", "张三", Some("12345@qq.com"))
            .expect("preset");

        assert_eq!(cfg.username, "12345@qq.com", "login name must be used");
        assert_eq!(
            cfg.email, "alias@foxmail.com",
            "From identity stays the alias"
        );
        assert_eq!(cfg.from_display(), "张三");
        // Still keyed by the visible address, so it does not collide with the QQ account.
        assert_eq!(cfg.id, "alias@foxmail.com");
    }

    #[test]
    fn blank_login_name_falls_back_to_the_address() {
        for login in [None, Some(""), Some("   ")] {
            let cfg = account_with_login("qq", "a@qq.com", "", login).expect("preset");
            assert_eq!(cfg.username, "a@qq.com", "blank override for {login:?}");
        }
    }

    #[test]
    fn login_name_is_trimmed() {
        let cfg =
            account_with_login("qq", "a@qq.com", "", Some("  12345@qq.com  ")).expect("preset");
        assert_eq!(cfg.username, "12345@qq.com");
    }

    #[test]
    fn builds_a_qq_account_without_the_user_knowing_any_ports() {
        let cfg = account_from_provider("qq", "Someone@QQ.com", "张三").expect("qq is a preset");
        assert_eq!(cfg.imap.host, "imap.qq.com");
        assert_eq!(cfg.imap.port, 993);
        assert_eq!(cfg.smtp.host, "smtp.qq.com");
        assert_eq!(cfg.smtp.port, 465);
        assert_eq!(cfg.display_name, "张三");
        // The id is derived from the address so the same account cannot be added twice.
        assert_eq!(cfg.id, "someone@qq.com");
        assert!(!cfg.needs_id_command);
    }

    #[test]
    fn netease_accounts_carry_the_id_quirk() {
        let cfg = account_from_provider("163", "a@163.com", "").expect("preset");
        assert!(cfg.needs_id_command, "163 requires the ID command");
        assert!(cfg.max_connections <= 2);

        let cfg = account_from_provider("126", "a@126.com", "").expect("preset");
        assert!(cfg.needs_id_command);
    }

    #[test]
    fn display_name_defaults_to_the_address() {
        let cfg = account_from_provider("gmail", "a@gmail.com", "   ").expect("preset");
        assert_eq!(cfg.from_display(), "a@gmail.com");
    }

    #[test]
    fn rejects_providers_without_imap() {
        let err = account_from_provider("tutanota", "a@tuta.com", "").expect_err("must reject");
        assert!(matches!(err, MailError::Config(_)));
        assert!(err.to_string().contains("IMAP"));
    }

    #[test]
    fn rejects_unknown_providers_and_bad_addresses() {
        assert!(account_from_provider("nope", "a@b.com", "").is_err());
        assert!(account_from_provider("qq", "not-an-email", "").is_err());
    }

    // ---------------------------------------------------------------- the manual layer (`custom`)

    fn hand_entered() -> EndpointOverride {
        EndpointOverride {
            imap_host: "  IMAPS://Mail.Example.com/  ".to_string(),
            imap_port: 993,
            imap_security: Security::Ssl,
            smtp_host: "smtp.example.com".to_string(),
            smtp_port: 587,
            smtp_security: Security::StartTls,
        }
    }

    #[test]
    fn a_custom_account_takes_its_servers_from_the_user() {
        let cfg = account_with_endpoints(
            "custom",
            "me@example.com",
            "自建",
            None,
            Some(&hand_entered()),
        )
        .expect("custom account");

        // The pasted scheme and the trailing slash are gone, and the host is lower-cased: what lands
        // in `accounts.json` is a hostname, not the text somebody pasted.
        assert_eq!(cfg.imap.host, "mail.example.com");
        assert_eq!(cfg.imap.port, 993);
        assert_eq!(cfg.imap.security, Security::Ssl);
        assert_eq!(cfg.smtp.host, "smtp.example.com");
        assert_eq!(cfg.smtp.port, 587);
        assert_eq!(cfg.smtp.security, Security::StartTls);
        assert_eq!(cfg.provider_id, "custom");
    }

    #[test]
    fn a_custom_account_without_endpoints_is_refused() {
        let err = account_with_endpoints("custom", "me@example.com", "", None, None)
            .expect_err("no servers to connect to");
        assert!(err.to_string().contains("需要填写"), "got: {err}");
    }

    #[test]
    fn a_table_provider_refuses_an_override_instead_of_ignoring_it() {
        // Ignoring it would look to the user like "my host was silently dropped", which is a bug
        // report nobody can act on.
        let err = account_with_endpoints("qq", "a@qq.com", "", None, Some(&hand_entered()))
            .expect_err("qq's servers are ours to know");
        assert!(err.to_string().contains("不能手动覆盖"), "got: {err}");
    }

    #[test]
    fn a_custom_account_survives_a_round_trip_through_accounts_json() {
        // `accounts.json` is how the account comes back on the next launch, so the hosts must be
        // plain data in it rather than something reconstructed from the provider table.
        let cfg =
            account_with_endpoints("custom", "me@example.com", "", None, Some(&hand_entered()))
                .expect("custom account");
        let json = serde_json::to_string(&cfg).expect("serialize");
        let back: AccountConfig = serde_json::from_str(&json).expect("deserialize");

        assert_eq!(back.imap, cfg.imap);
        assert_eq!(back.smtp, cfg.smtp);
        assert_eq!(back.provider_id, "custom");
    }

    #[test]
    fn a_bad_host_is_refused_before_anything_is_built() {
        let broken = EndpointOverride {
            imap_host: "imap.example.com:993".to_string(),
            ..hand_entered()
        };
        let err = account_with_endpoints("custom", "me@example.com", "", None, Some(&broken))
            .expect_err("a host carrying its own port cannot be used");
        assert!(err.to_string().contains("端口"), "got: {err}");
    }
}
