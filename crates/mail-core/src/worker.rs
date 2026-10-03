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
use crate::sentcopy::SentCopy;
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

    let mut mail: OutgoingMail = serde_json::from_str(&item.payload_json)
        .map_err(|e| MailError::Parse(format!("发件内容无法解析：{e}")))?;
    // Rebuilding is the only path where the file on disk can still be compared against what the user
    // approved — the frozen bytes are gone, so this is the last chance to tell them the file changed.
    // A file that was edited or deleted stops the send and asks; it is never quietly rebuilt from
    // whatever happens to be at that path now (`design/07` §5.3/§5.5).
    crate::attach::prepare_attachments(&mut mail)?;
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
                // The spool is **kept**: the Sent copy is filed from these bytes afterwards, and its
                // presence is what marks the row as "delivered but not archived yet" (`design/07`
                // §5.4). [`file_sent_copies`] removes it.
                //
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

/// How many bytes a message may have and still be re-sent unattended after a crash.
///
/// `design/07` §5.3.1: a large attachment is where the exposure window is genuinely long (TLS plus a
/// 25 MB upload on a slow line is minutes, and that is exactly when a Windows update reboots the
/// machine). Below the limit the check is cheap and the answer is trustworthy; above it the user
/// decides.
pub const DEFAULT_CRASH_RESEND_LIMIT: i64 = 5 * 1024 * 1024;

/// What a previous run left behind, and what to do about it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Recovery {
    /// The Sent folder already holds this `Message-ID`: it went out before the crash. **Never send it
    /// again** — a second SMTP submission is a second message in the recipient's mailbox.
    AlreadySent,
    /// Nothing left this machine, so the message is still owed to the user. Send it.
    Resend,
    /// A human has to look. Either we cannot tell whether it went out, or re-sending it unattended is
    /// not a decision this program should make on its own.
    AskUser(RecoveryQuestion),
    /// The server could not be asked (offline, or the folder failed to open). Leave the row exactly
    /// as it is and try again later: guessing "not sent" here is how the same message goes out twice.
    RetryLater,
}

/// Why [`Recovery::AskUser`] needs the user, as a value rather than a sentence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecoveryQuestion {
    /// The frozen bytes (and therefore the `Message-ID`) are gone, so there is nothing to search for.
    NoMessageId,
    /// Big enough that re-sending it silently is the wrong default.
    LargeAttachment,
}

impl RecoveryQuestion {
    /// The sentence the outbox shows. Stored as `last_error`, so it has to read like an explanation.
    pub fn message(self) -> &'static str {
        match self {
            Self::NoMessageId => {
                "无法确认这封信是否已经发出（冻结的信件内容不见了，查不到 Message-ID）。\
                 请先确认对方有没有收到，再决定重试还是删除。"
            }
            Self::LargeAttachment => {
                "这封信带了大附件，崩溃恢复时默认不自动重发（避免收件人收到两封）。\
                 确认没有发出后，点「重试」即可。"
            }
        }
    }
}

/// Decide what to do with a row that a crash left in `sending`.
///
/// The order matters. Knowing it *was* sent settles the question whatever else is true — including
/// the size pause, which exists only to avoid a guess. After that the size check comes before the
/// "not found" answer, because "not found" is the case where a wrong guess duplicates somebody's
/// mail. An unanswered question is never a licence to send.
pub fn recovery_verdict(
    message_id: Option<&str>,
    spool_bytes: i64,
    max_auto_bytes: i64,
    found_in_sent: Option<bool>,
) -> Recovery {
    if message_id.is_none() {
        return Recovery::AskUser(RecoveryQuestion::NoMessageId);
    }
    match found_in_sent {
        Some(true) => Recovery::AlreadySent,
        Some(false) if spool_bytes > max_auto_bytes => {
            Recovery::AskUser(RecoveryQuestion::LargeAttachment)
        }
        Some(false) => Recovery::Resend,
        None => Recovery::RetryLater,
    }
}

/// Everything the recovery pass needs.
pub struct RecoveryRun<'a> {
    pub store: &'a UserStore,
    pub spool: &'a Spool,
    pub now: i64,
    /// Messages larger than this wait for the user; see [`DEFAULT_CRASH_RESEND_LIMIT`].
    pub max_auto_bytes: i64,
}

/// What one recovery pass did.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct RecoveryReport {
    /// Rows that turned out to have been delivered already.
    pub already_sent: usize,
    /// Rows put back in the queue.
    pub requeued: usize,
    /// Rows taken out of the worker's hands for a human to look at.
    pub asked_user: usize,
    /// Rows left untouched because the server could not be asked.
    pub unresolved: usize,
}

/// Resolve the rows a crash left in `sending`.
///
/// `find_in_sent` is given a `Message-ID` and answers whether the Sent folder already holds it — the
/// same primitive the archive step uses, injected so the decision can be tested without a server.
///
/// This runs before the worker's first send, and only ever on rows that are already `sending`: those
/// are exactly the ones where "did it go out?" is unknown, and getting the answer wrong in the
/// optimistic direction sends a duplicate.
pub async fn recover_in_flight<F, Fut>(
    run: RecoveryRun<'_>,
    find_in_sent: F,
) -> Result<RecoveryReport>
where
    F: Fn(String) -> Fut,
    Fut: Future<Output = Result<bool>>,
{
    let mut report = RecoveryReport::default();

    for item in run.store.outbox_in_flight()? {
        // The frozen bytes are the only place the Message-ID is recorded before a send succeeds, and
        // reading it out of them is what ties this row to the message that may already be at the
        // recipient.
        let message_id = item
            .spool_path
            .as_deref()
            .and_then(|path| run.spool.read(Path::new(path)).ok())
            .and_then(|bytes| frozen_message_id(&bytes));

        let found = match message_id.as_deref() {
            // `None` when the server could not be asked: offline, or the folder would not open. Say
            // nothing and leave the row alone — a later attempt can still answer, and a wrong
            // "not sent" is the expensive mistake.
            Some(id) => find_in_sent(id.to_string()).await.ok(),
            None => None,
        };

        match recovery_verdict(
            message_id.as_deref(),
            item.spool_bytes,
            run.max_auto_bytes,
            found,
        ) {
            Recovery::AlreadySent => {
                run.store
                    .mark_outbox_sent(item.id, message_id.as_deref(), run.now)?;
                report.already_sent += 1;
            }
            Recovery::Resend => {
                // Straight back to the queue, eligible now: the user is waiting for this one.
                if run.store.requeue_in_flight(item.id)? {
                    report.requeued += 1;
                }
            }
            Recovery::AskUser(question) => {
                run.store
                    .mark_outbox_failed(item.id, question.message(), true)?;
                report.asked_user += 1;
            }
            Recovery::RetryLater => {
                report.unresolved += 1;
            }
        }
    }

    Ok(report)
}

/// Everything the filing pass needs.
pub struct FilingRun<'a> {
    pub store: &'a UserStore,
    pub spool: &'a Spool,
}

/// What one filing pass did.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct FilingReport {
    /// Messages the Sent folder turned out to have (or now has) a copy of.
    pub filed: usize,
    /// Messages this pass will never file: no Sent folder, or the frozen bytes are gone.
    pub skipped: usize,
    /// Messages to try again later — the account could not be reached, so nothing was decided.
    pub deferred: usize,
}

/// Put the Sent copy of every delivered message where it belongs (`design/07` §5.4).
///
/// `file_one` is handed the frozen `Message-ID` and the frozen bytes and answers where the copy
/// ended up, or `None` when the account has no Sent folder. It is injected for the same reason
/// [`run_due`]'s transport is: the interesting cases are the ones a live server rarely produces.
///
/// A pass that cannot reach the server decides nothing and leaves the spool in place, so the next
/// pass tries again — the spool's existence is the whole state machine.
pub async fn file_sent_copies<F, Fut>(run: FilingRun<'_>, file_one: F) -> Result<FilingReport>
where
    F: Fn(String, Vec<u8>) -> Fut,
    Fut: Future<Output = Result<Option<SentCopy>>>,
{
    let mut report = FilingReport::default();

    for item in run.store.sent_awaiting_copy()? {
        let Some(path) = item.spool_path.clone() else {
            continue;
        };
        let bytes = match run.spool.read(Path::new(&path)) {
            Ok(bytes) => bytes,
            Err(_) => {
                // The bytes are gone, so there is nothing to file and nothing to retry. Clear the
                // marker rather than looping over it at every pass.
                run.store.clear_outbox_spool(item.id)?;
                report.skipped += 1;
                continue;
            }
        };
        let Some(message_id) = frozen_message_id(&bytes) else {
            run.store.clear_outbox_spool(item.id)?;
            report.skipped += 1;
            continue;
        };

        match file_one(message_id, bytes).await {
            Ok(Some(_copy)) => {
                run.spool.remove(Path::new(&path));
                run.store.clear_outbox_spool(item.id)?;
                report.filed += 1;
            }
            Ok(None) => {
                // No Sent folder on this account: there is nowhere a copy could go, so this is
                // settled rather than pending.
                run.spool.remove(Path::new(&path));
                run.store.clear_outbox_spool(item.id)?;
                report.skipped += 1;
            }
            Err(_) => report.deferred += 1,
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

    /// A queue with one frozen message in it, plus a draft behind it.
    fn queued(spool: &Spool, undo_until: i64, now: i64) -> (UserStore, i64) {
        let store = UserStore::open_in_memory().expect("open");

        let mut draft = crate::userdb::Draft::new("d1", "t", now);
        draft.subject = "你好".into();
        store.save_draft(&draft).expect("draft");

        let id = enqueue_into(&store, spool, Some("d1"), undo_until, now);
        (store, id)
    }

    /// Add one frozen message to an existing queue.
    fn enqueue_into(
        store: &UserStore,
        spool: &Spool,
        draft_id: Option<&str>,
        undo_until: i64,
        now: i64,
    ) -> i64 {
        let mail = outgoing();
        let cfg = account();
        let message = smtp::build_message(&cfg, &mail).expect("build");
        let bytes = message.formatted();

        let item = NewOutbox {
            account_id: "t".into(),
            draft_id: draft_id.map(str::to_string),
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
        id
    }

    /// The `Message-ID` frozen inside a row's spool file.
    fn frozen_id_of(store: &UserStore, spool: &Spool, id: i64) -> String {
        let item = store.outbox_item(id).expect("item").expect("present");
        let path = item.spool_path.expect("path");
        frozen_message_id(&spool.read(std::path::Path::new(&path)).expect("read"))
            .expect("a built message carries a Message-ID")
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
        // The bytes stay: they are what the Sent copy is written from, and their presence is what
        // marks this row as "delivered but not archived yet" (design/07 §5.4).
        let path = item.spool_path.expect("path");
        assert!(Path::new(&path).exists(), "the spool is kept for the copy");
        assert_eq!(
            store.sent_awaiting_copy().expect("awaiting").len(),
            1,
            "and the row is queued up for the filing pass"
        );
        assert!(
            store.load_draft("d1").expect("draft").is_none(),
            "the draft is gone: leaving it would offer to send a second copy"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn filing_a_copy_clears_the_marker_and_the_bytes() {
        let dir = temp_dir("file");
        let spool = Spool::open(&dir).expect("spool");
        let (store, id) = queued(&spool, 0, 1_000);
        let cfg = account();

        // Delivered first: the filing pass only ever looks at rows that are already sent.
        run_due(run(&store, &spool, &cfg, 1_000), |_, _, _, _| async {
            Ok(())
        })
        .await
        .expect("run");
        let path = store
            .outbox_item(id)
            .expect("item")
            .expect("present")
            .spool_path
            .expect("path");

        let asked = Arc::new(Mutex::new(Vec::new()));
        let log = asked.clone();
        let report = file_sent_copies(
            FilingRun {
                store: &store,
                spool: &spool,
            },
            move |message_id, _bytes| {
                log.lock().expect("lock").push(message_id);
                async {
                    Ok(Some(crate::sentcopy::SentCopy {
                        folder: "Sent".into(),
                        uid: Some(4242),
                        appended: false,
                    }))
                }
            },
        )
        .await
        .expect("file");

        assert_eq!(report.filed, 1, "{report:?}");
        assert_eq!(
            asked.lock().expect("lock").len(),
            1,
            "the copy is looked up by the frozen Message-ID"
        );
        assert!(!Path::new(&path).exists(), "the bytes are gone once filed");
        assert!(
            store.sent_awaiting_copy().expect("awaiting").is_empty(),
            "and the row is not picked up again"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn filing_defers_when_the_server_cannot_be_reached() {
        // Offline is not a decision. Keeping the bytes is what lets the next pass finish the job.
        let dir = temp_dir("file-offline");
        let spool = Spool::open(&dir).expect("spool");
        let (store, _id) = queued(&spool, 0, 1_000);
        let cfg = account();
        run_due(run(&store, &spool, &cfg, 1_000), |_, _, _, _| async {
            Ok(())
        })
        .await
        .expect("run");

        let report = file_sent_copies(
            FilingRun {
                store: &store,
                spool: &spool,
            },
            |_, _| async { Err(MailError::Smtp("connection refused".into())) },
        )
        .await
        .expect("file");

        assert_eq!(report.deferred, 1, "{report:?}");
        assert_eq!(store.sent_awaiting_copy().expect("awaiting").len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn an_account_with_no_sent_folder_is_settled_not_retried() {
        let dir = temp_dir("file-no-folder");
        let spool = Spool::open(&dir).expect("spool");
        let (store, _id) = queued(&spool, 0, 1_000);
        let cfg = account();
        run_due(run(&store, &spool, &cfg, 1_000), |_, _, _, _| async {
            Ok(())
        })
        .await
        .expect("run");

        let report = file_sent_copies(
            FilingRun {
                store: &store,
                spool: &spool,
            },
            |_, _| async { Ok(None) },
        )
        .await
        .expect("file");

        assert_eq!(report.skipped, 1, "{report:?}");
        assert!(
            store.sent_awaiting_copy().expect("awaiting").is_empty(),
            "there is nowhere to file it, so retrying forever would be a loop with nothing in it"
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

    #[tokio::test]
    async fn recovery_settles_each_in_flight_row_using_the_sent_folder() {
        let dir = temp_dir("recover");
        let spool = Spool::open(&dir).expect("spool");
        let store = UserStore::open_in_memory().expect("open");
        let delivered = enqueue_into(&store, &spool, None, 0, 1_000);
        let owed = enqueue_into(&store, &spool, None, 0, 1_001);
        // Both are mid-send: that is what a crash leaves behind.
        assert!(store.claim_outbox(delivered, 1_000).expect("claim"));
        assert!(store.claim_outbox(owed, 1_001).expect("claim"));

        let delivered_id = frozen_id_of(&store, &spool, delivered);
        let asked = Arc::new(Mutex::new(Vec::new()));
        let log = asked.clone();
        let probe = delivered_id.clone();

        let report = recover_in_flight(
            RecoveryRun {
                store: &store,
                spool: &spool,
                now: 2_000,
                max_auto_bytes: DEFAULT_CRASH_RESEND_LIMIT,
            },
            move |message_id| {
                log.lock().expect("lock").push(message_id.clone());
                let probe = probe.clone();
                async move { Ok(message_id == probe) }
            },
        )
        .await
        .expect("recovery");

        assert_eq!(report.already_sent, 1);
        assert_eq!(report.requeued, 1);
        assert_eq!(report.asked_user, 0);
        assert_eq!(report.unresolved, 0);
        assert_eq!(
            asked.lock().expect("lock").len(),
            2,
            "both rows had to be looked up: guessing is what duplicates mail"
        );

        let done = store
            .outbox_item(delivered)
            .expect("item")
            .expect("present");
        assert_eq!(done.parsed_state(), crate::outbox::OutboxState::Sent);
        assert_eq!(
            done.sent_message_id.as_deref(),
            Some(delivered_id.as_str()),
            "the id it was found by is the one recorded"
        );

        let back = store.outbox_item(owed).expect("item").expect("present");
        assert_eq!(back.parsed_state(), crate::outbox::OutboxState::Queued);
        assert!(
            store
                .due_outbox(2_000, 10)
                .expect("due")
                .iter()
                .any(|i| i.id == owed),
            "a message the server never saw is owed to the user now"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn recovery_leaves_a_row_alone_when_the_server_cannot_be_asked() {
        // Offline at startup is the normal case, not an error. Requeueing here would be a guess, and
        // a wrong guess sends the message a second time.
        let dir = temp_dir("recover-offline");
        let spool = Spool::open(&dir).expect("spool");
        let store = UserStore::open_in_memory().expect("open");
        let id = enqueue_into(&store, &spool, None, 0, 1_000);
        assert!(store.claim_outbox(id, 1_000).expect("claim"));

        let report = recover_in_flight(
            RecoveryRun {
                store: &store,
                spool: &spool,
                now: 2_000,
                max_auto_bytes: DEFAULT_CRASH_RESEND_LIMIT,
            },
            |_| async { Err(MailError::Smtp("connection refused".into())) },
        )
        .await
        .expect("recovery");

        assert_eq!(report.unresolved, 1);
        let item = store.outbox_item(id).expect("item").expect("present");
        assert_eq!(
            item.parsed_state(),
            crate::outbox::OutboxState::Sending,
            "still in flight: a later attempt can still answer the question"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn recovery_asks_the_user_when_the_frozen_bytes_are_gone() {
        let dir = temp_dir("recover-lost-spool");
        let spool = Spool::open(&dir).expect("spool");
        let store = UserStore::open_in_memory().expect("open");
        let id = enqueue_into(&store, &spool, None, 0, 1_000);
        assert!(store.claim_outbox(id, 1_000).expect("claim"));

        let item = store.outbox_item(id).expect("item").expect("present");
        spool.remove(std::path::Path::new(
            item.spool_path.as_deref().expect("path"),
        ));

        let report = recover_in_flight(
            RecoveryRun {
                store: &store,
                spool: &spool,
                now: 2_000,
                max_auto_bytes: DEFAULT_CRASH_RESEND_LIMIT,
            },
            |_| async { Ok(false) },
        )
        .await
        .expect("recovery");

        assert_eq!(report.asked_user, 1);
        let item = store.outbox_item(id).expect("item").expect("present");
        assert_eq!(
            item.parsed_state(),
            crate::outbox::OutboxState::NeedsAttention
        );
        assert!(
            item.last_error
                .as_deref()
                .is_some_and(|reason| reason.contains("Message-ID")),
            "the reason has to tell the user what to check: {:?}",
            item.last_error
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_message_the_server_already_has_is_never_sent_again() {
        // The whole point of §5.3.1: the second submission is a second message in the recipient's
        // mailbox, so "found in Sent" must win over every other consideration — including the size
        // pause, which exists only to avoid a guess.
        assert_eq!(
            recovery_verdict(Some("<id@x>"), 99_999_999, 5 * 1024 * 1024, Some(true)),
            Recovery::AlreadySent
        );
    }

    #[test]
    fn an_uncertain_message_is_never_sent_unattended() {
        // Nothing to search for.
        assert_eq!(
            recovery_verdict(None, 10, i64::MAX, Some(false)),
            Recovery::AskUser(RecoveryQuestion::NoMessageId)
        );
        // Big, and not in Sent: the exposure window was long, so the user decides.
        assert_eq!(
            recovery_verdict(
                Some("<id@x>"),
                6 * 1024 * 1024,
                5 * 1024 * 1024,
                Some(false)
            ),
            Recovery::AskUser(RecoveryQuestion::LargeAttachment)
        );
        // The server could not be asked at all — offline at startup is the normal case.
        assert_eq!(
            recovery_verdict(Some("<id@x>"), 10, 5 * 1024 * 1024, None),
            Recovery::RetryLater
        );
    }

    #[test]
    fn a_small_message_the_server_does_not_have_is_sent() {
        assert_eq!(
            recovery_verdict(Some("<id@x>"), 10, 5 * 1024 * 1024, Some(false)),
            Recovery::Resend
        );
        // Exactly at the limit is fine: the constant is "above this, ask".
        assert_eq!(
            recovery_verdict(
                Some("<id@x>"),
                5 * 1024 * 1024,
                5 * 1024 * 1024,
                Some(false)
            ),
            Recovery::Resend
        );
    }

    #[tokio::test]
    async fn a_rebuild_refuses_an_attachment_that_changed() {
        // The frozen bytes are gone, so the file on disk is the only source left — and it is no longer
        // what the user approved. This is the one moment the check can still be made, and sending the
        // new contents instead would deliver something they never agreed to.
        let dir = temp_dir("rebuild-changed");
        let spool = Spool::open(&dir).expect("spool");
        let store = UserStore::open_in_memory().expect("open");
        let cfg = account();

        let file = dir.join("report.pdf");
        std::fs::write(&file, b"first version").expect("fixture");
        let recorded = crate::attach::file_sha256(&file).expect("hash");
        std::fs::write(&file, b"second version").expect("edit fixture");

        let mut mail = outgoing();
        mail.attachments.push(crate::model::OutgoingAttachment {
            name: "report.pdf".into(),
            mime: "application/pdf".into(),
            path: file.to_string_lossy().to_string(),
            size: 14,
            sha256: Some(recorded),
        });

        let id = store
            .enqueue_outbox(&NewOutbox {
                account_id: "t".into(),
                draft_id: None,
                payload_json: serde_json::to_string(&mail).expect("payload"),
                // No spool file: this row can only be sent by rebuilding from the payload.
                spool_path: None,
                spool_bytes: 0,
                undo_until: 0,
                scheduled_at: 0,
                created_at: 1_000,
            })
            .expect("enqueue");

        let calls = Arc::new(Mutex::new(0usize));
        let counter = calls.clone();
        let report = run_due(run(&store, &spool, &cfg, 1_000), move |_, _, _, _| {
            *counter.lock().expect("lock") += 1;
            async { Ok(()) }
        })
        .await
        .expect("run");

        assert_eq!(report.sent, 0, "it must not go out with the new contents");
        assert_eq!(report.needs_attention, 1, "{report:?}");
        assert_eq!(*calls.lock().expect("lock"), 0, "SMTP was never reached");

        let item = store.outbox_item(id).expect("item").expect("present");
        assert_eq!(
            item.parsed_state(),
            crate::outbox::OutboxState::NeedsAttention
        );
        let reason = item.last_error.expect("reason");
        assert!(
            reason.contains("report.pdf") && reason.contains("改动"),
            "the user has to be told which file and how to proceed: {reason}"
        );
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
