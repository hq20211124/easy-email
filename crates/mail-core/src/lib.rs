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
//! - [`smtp`] — compose and send, including threading headers.
//! - [`secret`] — Windows Credential Manager access.

pub mod backfill;
pub mod cache;
pub mod db;
pub mod error;
pub mod folders;
pub mod idle;
pub mod imap;
pub mod model;
pub mod mutf7;
pub mod outbox;
pub mod pool;
pub mod providers;
pub mod secret;
pub mod smtp;
pub mod spool;
pub mod sync;
pub mod userdb;
pub mod worker;

pub use backfill::{BackfillBudget, BackfillReport, BackfillStop, backfill_older};
pub use cache::{Cache, FolderState, SearchHit};
pub use error::{AuthHint, MailError, Result};
pub use idle::{PushEvent, watch, watch_every};
pub use imap::{HeaderPage, ImapClient};
pub use model::{
    AccountConfig, AttachmentMeta, AuthKind, ConnectionReport, Endpoint, InlineImage, MailboxInfo,
    MessageDetail, MessageSummary, OutgoingAttachment, OutgoingMail, Region, Security,
};
pub use outbox::{
    OutboxState, Verdict, is_permanent_failure, needs_user_decision, next_attempt_delay_secs,
    verdict_for_failure,
};
pub use pool::{ConnectionPool, PooledClient};
pub use providers::Provider;
pub use spool::Spool;
pub use sync::{Fetch, SyncDriver, SyncOutcome, SyncWindow};
pub use userdb::{Draft, NewOutbox, OutboxItem, UserStore};
pub use worker::{OutboxRun, RunReport, run_due};

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
pub fn account_with_login(
    provider_id: &str,
    email: &str,
    display_name: &str,
    login: Option<&str>,
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
        imap: provider.imap_endpoint(),
        smtp: provider.smtp_endpoint(),
        needs_id_command: provider.quirks.needs_id,
        max_connections: provider.quirks.max_connections,
    })
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
        Err(e) => report.imap_detail = describe(&e),
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
        Err(e) => report.smtp_detail = describe(&e),
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
}
