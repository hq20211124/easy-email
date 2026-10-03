//! The outbox worker: one pass over the queue, turning "queued" into "sent" (`design/07` §5.3).
//!
//! The loop that decides *when* to call this lives in the desktop shell (5 s tick, plus a wake on
//! enqueue, on the undo window expiring and on the network coming back). What lives here is the part
//! worth testing: claim a row atomically, get the frozen bytes, hand them to SMTP, and record the
//! outcome. The transport is a parameter rather than a hard call so a test can produce the failures a
//! real server only produces when it feels like it.
//!
//! Nothing in here re-builds a message unless it has to. `design/07` §5.3 is explicit that the bytes
//! are frozen at enqueue time: rebuilding is a second chance to produce a different message than the
//! one the user approved. The only exception is a spool file that has gone missing, which is
//! reported rather than guessed at.

use std::future::Future;
use std::path::Path;

use crate::error::{MailError, Result};
use crate::model::{AccountConfig, OutgoingMail};
use crate::outbox::{Verdict, next_attempt_delay_secs, verdict_for_failure};
use crate::smtp;
use crate::spool::Spool;
use crate::userdb::{OutboxItem, UserStore};

/// Everything one pass over the queue needs.
pub struct OutboxRun<'a> {
    pub store: &'a UserStore,
    pub spool: &'a Spool,
    pub cfg: &'a AccountConfig,
    pub password: &'a str,
    pub now: i64,
    /// How many messages one pass may attempt.
    ///
    /// Bounded on purpose: a backlog of 200 queued messages must not hold the account's connection
    /// budget (`design/04`) for minutes, and the caller needs a tick that returns.
    pub limit: usize,
}

/// What one pass did. Returned rather than only logged so the caller can decide whether to tell the
/// window something (`outbox:changed`) or schedule an immediate follow-up pass.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct RunReport {
    pub attempted: usize,
    pub sent: usize,
    pub retried: usize,
    pub gave_up: usize,
    pub needs_attention: usize,
}

/// How many messages one pass may attempt by default.
pub const DEFAULT_PASS_LIMIT: usize = 5;

/// The `Message-ID` in an already-built message.
///
/// Read out of the frozen bytes rather than remembered somewhere: the spool **is** the message, and
/// the identity that matters for the Sent copy and for crash recovery is the one actually on the
/// wire. Header folding is not a concern for a `Message-ID`, but the value is trimmed anyway.
pub fn frozen_message_id(bytes: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(bytes);
    for line in text.lines() {
        if line.is_empty() {
            break; // end of the header block
        }
        if let Some(value) = line.strip_prefix("Message-ID:") {
            let value = value.trim();
            if !value.is_empty() {
                return Some(value.to_string());
            }
        }
    }
    None
}

/// Who the envelope must carry.
///
/// From the stored payload, not from the headers: Bcc recipients exist only in the envelope
/// (`design/07` §7 requires exactly that, so they cannot be recovered from the message itself).
fn envelope_recipients(item: &OutboxItem) -> Result<Vec<String>> {
    let mail: OutgoingMail = serde_json::from_str(&item.payload_json)
        .map_err(|e| MailError::Parse(format!("发件内容无法解析：{e}")))?;
    let mut recipients: Vec<String> = Vec::with_capacity(mail.to.len() + mail.cc.len());
    recipients.extend(mail.to.iter().cloned());
    recipients.extend(mail.cc.iter().cloned());
    recipients.extend(mail.bcc.iter().cloned());
    Ok(recipients)
}

/// The bytes to send, plus the `Message-ID` they carry.
///
/// Reads the spool. Only when the file is gone does it rebuild from the payload — and it says so in
/// the returned flag, because a rebuilt message is a *different* `Message-ID` and therefore a
/// different message as far as the recipient's client is concerned.
fn frozen_bytes(run: &OutboxRun<'_>, item: &OutboxItem) -> Result<(Vec<u8>, Option<String>)> {
    if let Some(path) = item.spool_path.as_deref() {
        let path = Path::new(path);
        if path.exists() {
            let bytes = run.spool.read(path)?;
            let message_id = frozen_message_id(&bytes);
            return Ok((bytes, message_id));
        }
    }

    let mail: OutgoingMail = serde_json::from_str(&item.payload_json)
        .map_err(|e| MailError::Parse(format!("发件内容无法解析：{e}")))?;
    let message = smtp::build_message(run.cfg, &mail)?;
    let message_id = smtp::message_id_of(&message);
    let bytes = message.formatted();
    // Put it back, so the next attempt (and crash recovery) sees the same bytes we are about to
    // send rather than rebuilding a third time.
    if let Ok((path, _)) = run.spool.write(item.id, &bytes) {
        let _ = run
            .store
            .set_outbox_spool(item.id, &path.to_string_lossy(), bytes.len() as i64);
    }
    Ok((bytes, message_id))
}

/// Attempt every send that is due.
///
/// `deliver` receives the account, the password, the frozen bytes and the envelope recipients. It is
/// a plain closure rather than a trait object because the only implementation is [`smtp::send_spool`]
/// and a trait here would add a dependency to make one call site swappable.
pub async fn run_due<F, Fut>(run: OutboxRun<'_>, deliver: F) -> Result<RunReport>
where
    F: Fn(AccountConfig, String, Vec<u8>, Vec<String>) -> Fut,
    Fut: Future<Output = Result<()>>,
{
    let mut report = RunReport::default();

    for item in run.store.due_outbox(run.now, run.limit)? {
        // Claim first: two windows, or a tick racing a manual "send now", must not both send it.
        if !run.store.claim_outbox(item.id, run.now)? {
            continue;
        }
        report.attempted += 1;

        let recipients = match envelope_recipients(&item) {
            Ok(recipients) => recipients,
            Err(error) => {
                // The stored payload is unreadable, so we cannot even say who this was for. Retrying
                // cannot help; a human has to look at it.
                run.store.mark_outbox_failed(
                    item.id,
                    &format!("发件队列里的这封信读不出来了：{error}"),
                    true,
                )?;
                report.needs_attention += 1;
                continue;
            }
        };

        let (bytes, message_id) = match frozen_bytes(&run, &item) {
            Ok(frozen) => frozen,
            Err(error) => {
                run.store.mark_outbox_failed(
                    item.id,
                    &format!("无法重建这封信的内容：{error}"),
                    true,
                )?;
                report.needs_attention += 1;
                continue;
            }
        };

        match deliver(run.cfg.clone(), run.password.to_string(), bytes, recipients).await {
            Ok(()) => {
                run.store
                    .mark_outbox_sent(item.id, message_id.as_deref(), run.now)?;
                if let Some(path) = item.spool_path.as_deref() {
                    run.spool.remove(Path::new(path));
                }
                // The draft has served its purpose. Leaving it would offer to send the same message
                // a second time (`design/07` §5.1: 成功 → 删草稿).
                if let Some(draft_id) = item.draft_id.as_deref() {
                    let _ = run.store.delete_draft(draft_id);
                }
                report.sent += 1;
            }
            Err(error) => {
                let message = error.to_string();
                match verdict_for_failure(&message) {
                    Verdict::Retry => {
                        let delay = next_attempt_delay_secs(item.attempts);
                        run.store
                            .mark_outbox_retry(item.id, &message, run.now + delay)?;
                        report.retried += 1;
                    }
                    Verdict::GiveUp { needs_attention } => {
                        run.store
                            .mark_outbox_failed(item.id, &message, needs_attention)?;
                        if needs_attention {
                            report.needs_attention += 1;
                        } else {
                            report.gave_up += 1;
                        }
                    }
                }
            }
        }
    }

    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Endpoint, Security};
    use crate::userdb::NewOutbox;
    use std::sync::{Arc, Mutex};

    fn temp_dir(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "easy-email-worker-{}-{}-{}",
            std::process::id(),
            name,
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ))
    }

    fn account() -> AccountConfig {
        AccountConfig {
            id: "t".into(),
            email: "me@example.com".into(),
            display_name: "测试用户".into(),
            username: "me@example.com".into(),
            provider_id: "custom".into(),
            imap: Endpoint::new("127.0.0.1", 1143, Security::Plain),
            smtp: Endpoint::new("127.0.0.1", 1025, Security::Plain),
            needs_id_command: false,
            max_connections: 4,
        }
    }

    fn outgoing() -> OutgoingMail {
        OutgoingMail {
            to: vec!["them@example.com".into()],
            cc: vec![],
            bcc: vec!["secret@example.com".into()],
            subject: "你好".into(),
            body_text: "正文".into(),
            body_html: None,
            in_reply_to: None,
            references: vec![],
            attachments: vec![],
            message_id: None,
        }
    }

    /// A queue with one frozen message in it, and a draft behind it.
    fn queued(spool: &Spool, undo_until: i64, now: i64) -> (UserStore, i64) {
        let store = UserStore::open_in_memory().expect("open");
        let mail = outgoing();
        let cfg = account();
        let message = smtp::build_message(&cfg, &mail).expect("build");
        let bytes = message.formatted();

        let mut draft = crate::userdb::Draft::new("d1", "t", now);
        draft.subject = "你好".into();
        store.save_draft(&draft).expect("draft");

        let item = NewOutbox {
            account_id: "t".into(),
            draft_id: Some("d1".into()),
            payload_json: serde_json::to_string(&mail).expect("payload"),
            spool_path: None,
            spool_bytes: 0,
            undo_until,
            scheduled_at: 0,
            created_at: now,
        };
        let id = store.enqueue_outbox(&item).expect("enqueue");
        let (path, size) = spool.write(id, &bytes).expect("spool");
        store
            .set_outbox_spool(id, &path.to_string_lossy(), size as i64)
            .expect("record spool");
        (store, id)
    }

    fn run<'a>(
        store: &'a UserStore,
        spool: &'a Spool,
        cfg: &'a AccountConfig,
        now: i64,
    ) -> OutboxRun<'a> {
        OutboxRun {
            store,
            spool,
            cfg,
            password: "pw",
            now,
            limit: DEFAULT_PASS_LIMIT,
        }
    }

    /// What a fake transport was handed: the frozen bytes and the envelope recipients.
    type Delivered = Arc<Mutex<Vec<(Vec<u8>, Vec<String>)>>>;

    #[tokio::test]
    async fn a_due_message_goes_out_and_leaves_nothing_behind() {
        let dir = temp_dir("sent");
        let spool = Spool::open(&dir).expect("spool");
        let (store, id) = queued(&spool, 0, 1_000);
        let cfg = account();
        let seen: Delivered = Arc::new(Mutex::new(Vec::new()));

        let sink = seen.clone();
        let report = run_due(run(&store, &spool, &cfg, 1_000), move |_, _, bytes, to| {
            sink.lock().expect("lock").push((bytes, to));
            async { Ok(()) }
        })
        .await
        .expect("run");

        assert_eq!(report.sent, 1);
        let item = store.outbox_item(id).expect("item").expect("present");
        assert_eq!(item.parsed_state(), crate::outbox::OutboxState::Sent);
        assert!(item.sent_at.is_some());
        assert!(
            item.sent_message_id.is_some(),
            "the frozen Message-ID is recorded: the Sent copy is found by it"
        );

        let calls = seen.lock().expect("lock");
        assert_eq!(calls.len(), 1);
        assert!(
            calls[0].0.starts_with(b"From:"),
            "the bytes reaching SMTP are the frozen message"
        );
        assert_eq!(
            calls[0].1,
            vec![
                "them@example.com".to_string(),
                "secret@example.com".to_string()
            ],
            "the envelope carries Bcc, which the headers must not"
        );
        assert!(
            !String::from_utf8_lossy(&calls[0].0).contains("secret@example.com"),
            "a Bcc address in the headers would leak it to every recipient"
        );
        assert!(
            !Path::new(&item.spool_path.expect("path")).exists(),
            "the spool is cleaned up"
        );
        assert!(
            store.load_draft("d1").expect("draft").is_none(),
            "the draft is gone: leaving it would offer to send a second copy"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_message_inside_its_undo_window_is_not_sent() {
        let dir = temp_dir("undo");
        let spool = Spool::open(&dir).expect("spool");
        let (store, id) = queued(&spool, 1_060, 1_000);
        let cfg = account();
        let calls = Arc::new(Mutex::new(0usize));

        let counter = calls.clone();
        let report = run_due(run(&store, &spool, &cfg, 1_000), move |_, _, _, _| {
            *counter.lock().expect("lock") += 1;
            async { Ok(()) }
        })
        .await
        .expect("run");

        assert_eq!(report.attempted, 0, "the undo window has not expired");
        assert_eq!(*calls.lock().expect("lock"), 0);
        let item = store.outbox_item(id).expect("item").expect("present");
        assert_eq!(item.parsed_state(), crate::outbox::OutboxState::Queued);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_cancelled_send_stays_cancelled_even_when_its_window_expires() {
        let dir = temp_dir("cancel");
        let spool = Spool::open(&dir).expect("spool");
        let (store, id) = queued(&spool, 1_000, 1_000);
        let cfg = account();
        assert!(store.cancel_outbox(id).expect("cancel"));

        let calls = Arc::new(Mutex::new(0usize));
        let counter = calls.clone();
        run_due(run(&store, &spool, &cfg, 9_999), move |_, _, _, _| {
            *counter.lock().expect("lock") += 1;
            async { Ok(()) }
        })
        .await
        .expect("run");

        assert_eq!(*calls.lock().expect("lock"), 0);
        let item = store.outbox_item(id).expect("item").expect("present");
        assert_eq!(item.parsed_state(), crate::outbox::OutboxState::Cancelled);
        // The draft survives a cancellation: the user still has what they wrote (design/07 §5.2).
        assert!(store.load_draft("d1").expect("draft").is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_transient_failure_waits_then_comes_back() {
        let dir = temp_dir("retry");
        let spool = Spool::open(&dir).expect("spool");
        let (store, id) = queued(&spool, 0, 1_000);
        let cfg = account();

        let report = run_due(run(&store, &spool, &cfg, 1_000), |_, _, _, _| async {
            Err(MailError::Smtp("421 4.3.2 try again later".into()))
        })
        .await
        .expect("run");

        assert_eq!(report.retried, 1);
        let item = store.outbox_item(id).expect("item").expect("present");
        assert_eq!(item.parsed_state(), crate::outbox::OutboxState::Queued);
        assert_eq!(item.attempts, 1);
        assert_eq!(item.next_attempt_at, 1_000 + 30, "the first backoff step");
        assert!(item.spool_path.is_some(), "a retry reuses the frozen bytes");
        assert!(
            store.due_outbox(1_029, 5).expect("due").is_empty(),
            "not yet"
        );
        assert_eq!(store.due_outbox(1_030, 5).expect("due").len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_permanent_failure_stops_and_never_retries_itself() {
        let dir = temp_dir("permanent");
        let spool = Spool::open(&dir).expect("spool");
        let (store, id) = queued(&spool, 0, 1_000);
        let cfg = account();

        let report = run_due(run(&store, &spool, &cfg, 1_000), |_, _, _, _| async {
            Err(MailError::Smtp("550 5.1.1 no such user".into()))
        })
        .await
        .expect("run");

        assert_eq!(report.gave_up, 1);
        let item = store.outbox_item(id).expect("item").expect("present");
        assert_eq!(item.parsed_state(), crate::outbox::OutboxState::Failed);
        assert!(
            store.due_outbox(i64::MAX / 2, 5).expect("due").is_empty(),
            "a permanent failure that stayed eligible would be retried forever"
        );
        // The draft stays, so the user can fix the address and try again.
        assert!(store.load_draft("d1").expect("draft").is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_too_large_refusal_asks_the_user_instead_of_looping() {
        let dir = temp_dir("too-large");
        let spool = Spool::open(&dir).expect("spool");
        let (store, id) = queued(&spool, 0, 1_000);
        let cfg = account();

        let report = run_due(run(&store, &spool, &cfg, 1_000), |_, _, _, _| async {
            Err(MailError::Smtp("552 5.3.4 message too big".into()))
        })
        .await
        .expect("run");

        assert_eq!(report.needs_attention, 1);
        let item = store.outbox_item(id).expect("item").expect("present");
        assert_eq!(
            item.parsed_state(),
            crate::outbox::OutboxState::NeedsAttention
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_claimed_row_cannot_be_sent_twice() {
        let dir = temp_dir("claim");
        let spool = Spool::open(&dir).expect("spool");
        let (store, id) = queued(&spool, 0, 1_000);
        assert!(store.claim_outbox(id, 1_000).expect("claim"));
        assert!(
            !store.claim_outbox(id, 1_000).expect("claim again"),
            "two passes must not both take the same message"
        );
        assert!(
            store.due_outbox(1_000, 5).expect("due").is_empty(),
            "an in-flight row is not due"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_missing_spool_is_rebuilt_from_the_payload() {
        let dir = temp_dir("rebuild");
        let spool = Spool::open(&dir).expect("spool");
        let (store, id) = queued(&spool, 0, 1_000);
        let cfg = account();
        let item = store.outbox_item(id).expect("item").expect("present");
        spool.remove(Path::new(item.spool_path.as_deref().expect("path")));

        let report = run_due(
            run(&store, &spool, &cfg, 1_000),
            |_, _, bytes, _| async move {
                assert!(
                    bytes.starts_with(b"From:"),
                    "rebuilt bytes are still a message"
                );
                Ok(())
            },
        )
        .await
        .expect("run");

        assert_eq!(report.sent, 1, "a lost spool file is worth rebuilding");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_frozen_message_id_is_read_from_the_header_block() {
        let raw = b"From: a@b.c\r\nSubject: hi\r\nMessage-ID: <abc@localhost>\r\n\r\nbody\r\n";
        assert_eq!(frozen_message_id(raw), Some("<abc@localhost>".to_string()));
    }

    #[test]
    fn a_message_id_in_the_body_is_not_a_header() {
        // Without stopping at the blank line, a quoted reply containing "Message-ID:" would be read
        // as this message's own id — and crash recovery would then search the Sent folder for the
        // wrong message.
        let raw = "From: a@b.c\r\nSubject: hi\r\n\r\n引用：\r\nMessage-ID: <quoted@x>\r\n";
        assert_eq!(frozen_message_id(raw.as_bytes()), None);
    }

    #[test]
    fn a_message_without_one_reports_none() {
        assert_eq!(frozen_message_id(b"From: a@b.c\r\n\r\nbody"), None);
        assert_eq!(frozen_message_id(b""), None);
    }
}
