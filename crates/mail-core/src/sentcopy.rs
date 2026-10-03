//! The Sent copy: make sure a message we just delivered is in the account's Sent folder.
//!
//! `design/07` §5.4. The hard part is that the server may or may not file the copy itself, and the
//! two possible mistakes are asymmetric:
//!
//! * a **missing** copy is one of the failures users take hardest — "已发送里找不到我刚发的信";
//! * a **duplicate** copy is also unacceptable. "The server deduplicates by `Message-ID`" is only
//!   true of Gmail; Dovecot, Exchange, 163 and QQ will happily store a second one (A2 review).
//!
//! So the rule is **search first, append only if the search comes up empty**, and the search is
//! repeated a bounded number of times because a server that files the copy does it slightly after
//! accepting the message. That makes the step idempotent: running it twice cannot produce two copies,
//! which in turn is what lets the outbox treat "spool file still present" as "not archived yet" and
//! resume after a crash with no extra bookkeeping.
//!
//! The design's `provider.auto_files_sent` flag only ever skipped the polling for servers known not
//! to file. It is not needed for correctness — searching first is right either way — so it is not
//! carried: a per-provider switch that can only save a second, and that would create duplicates if
//! it were ever wrong in the "no" direction, is a bad trade.

use std::time::Duration;

use crate::error::{MailError, Result};
use crate::imap::ImapClient;
use crate::model::{AccountConfig, OutgoingAttachment, OutgoingMail};
use crate::userdb::{Draft, DraftMirror};

/// How long to keep looking before deciding the server is not going to file it.
///
/// `design/07` §5.4 allows "3 attempts / ≤30 s cumulative". A server that files does so as part of
/// accepting the message, so those bounds are a ceiling rather than a target — and a user who just
/// pressed 发送 should not wait half a minute for a copy of their own message.
const POLL_LADDER: [Duration; 3] = [
    Duration::ZERO,
    Duration::from_millis(400),
    Duration::from_millis(1200),
];

/// What happened to one message's Sent copy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SentCopy {
    /// The folder it is in, as the wire name.
    pub folder: String,
    /// Its UID, when it could be found. `None` is the documented `APPEND` outcome: the server may
    /// not echo `APPENDUID`, and async-imap discards it when it does (A1-F43) — so this is a lookup
    /// that failed, not a copy that failed. **Nothing may append again because of it.**
    pub uid: Option<u32>,
    /// True when the server had not filed it and we wrote it ourselves.
    pub appended: bool,
}

/// Make sure the account's Sent folder holds this message, and say where it is.
///
/// `None` means **nothing will ever be filed** for this message: either the account has no Sent
/// folder, or the message is larger than the server's documented append limit (`design/07` §5.4's
/// `SkippedTooLarge`). Both are facts rather than failures, and both mean the same thing to the
/// caller — stop, and do not retry.
pub async fn ensure_sent_copy(
    client: &mut ImapClient,
    raw: &[u8],
    message_id: &str,
    append_limit: Option<u64>,
) -> Result<Option<SentCopy>> {
    let Some(folder) = client.sent_mailbox().await? else {
        return Ok(None);
    };

    // `design/07` §5.4 calls this `SkippedTooLarge`: the server has told us (A1-F3) that it will not
    // take a message this big, so attempting the APPEND can only fail — and retrying it can only fail
    // again. `None` is the same answer to the caller as "no Sent folder": nothing is filed, nothing
    // will ever be filed, stop and do not retry. (The user-facing "副本太大，已保存在本机" is a
    // follow-up; today the outbox simply reports it as skipped.)
    if let Some(limit) = append_limit
        && raw.len() as u64 > limit
    {
        return Ok(None);
    }

    for delay in POLL_LADDER {
        if !delay.is_zero() {
            tokio::time::sleep(delay).await;
        }
        if let Some(uid) = client.find_uid_by_message_id(&folder, message_id).await? {
            return Ok(Some(SentCopy {
                folder,
                uid: Some(uid),
                appended: false,
            }));
        }
    }

    // The server did not file it, so we do — `\Seen`, the way every other client writes a sent copy.
    client.append(&folder, raw, true).await?;

    // APPEND does not hand back the UID (A1-F43), so ask for it. Failing to find it is *not* a
    // reason to append again: the copy is in the folder either way, and a second write would be the
    // duplicate this whole function exists to avoid.
    let uid = client.find_uid_by_message_id(&folder, message_id).await?;
    Ok(Some(SentCopy {
        folder,
        uid,
        appended: true,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_poll_ladder_is_bounded_and_short() {
        // Bounded: a server that never files must not turn this into an open-ended wait. Short: the
        // user is watching the outbox row, and the design's 30 s is a ceiling, not a target.
        let total: Duration = POLL_LADDER.iter().sum();
        assert!(
            total <= Duration::from_secs(30),
            "must stay inside design/07 §5.4's ceiling: {total:?}"
        );
        assert!(
            total <= Duration::from_secs(3),
            "and stay well inside it, or every send stalls on it: {total:?}"
        );
        assert_eq!(
            POLL_LADDER[0],
            Duration::ZERO,
            "the first look is immediate"
        );
        assert_eq!(POLL_LADDER.len(), 3, "three attempts, as the design says");
    }

    #[test]
    fn a_draft_becomes_a_message_the_builder_understands() {
        // What a person typed is a comma-separated string; what the builder needs is a list. Getting
        // this wrong is invisible until a draft is mirrored and the recipients are one long address.
        let mut draft = Draft::new("d1", "acc", 1);
        draft.to_addrs = "a@example.com, b@example.com；c@example.com; ".to_string();
        draft.cc_addrs = "d@example.com".to_string();
        draft.subject = "十月报销".to_string();
        draft.body_text = "见附件".to_string();
        draft.attachments_json =
            r#"[{"path":"C:\\x.pdf","name":"x.pdf","mime":"application/pdf","size":10}]"#
                .to_string();
        draft.references = r#"["<m1@example.com>"]"#.to_string();

        let mail = draft_to_mail(&draft).expect("convert");
        assert_eq!(
            mail.to,
            vec!["a@example.com", "b@example.com", "c@example.com"],
            "both comma widths and stray spaces have to be handled"
        );
        assert_eq!(mail.cc, vec!["d@example.com"]);
        assert_eq!(mail.subject, "十月报销");
        assert_eq!(mail.attachments.len(), 1);
        assert_eq!(mail.attachments[0].name, "x.pdf");
        assert_eq!(mail.references, vec!["<m1@example.com>"]);
        assert!(
            mail.attachments[0].sha256.is_none(),
            "a draft that predates hashing keeps working"
        );
    }

    #[test]
    fn a_draft_with_unreadable_attachment_json_is_an_error_not_an_empty_message() {
        // Silently mirroring a draft without the attachment the user added would look like success
        // while the server copy quietly lacks a file.
        let mut draft = Draft::new("d1", "acc", 1);
        draft.attachments_json = "{ not json".to_string();
        assert!(draft_to_mail(&draft).is_err());
    }
}

/// Turn a stored draft into something the SMTP/IMAP message builder can use.
///
/// The draft keeps addresses as the comma-separated strings a person typed, and attachments as a
/// JSON array of paths plus metadata (`design/07` §5.5); building the message needs them split and
/// parsed. Kept here, as a pure function, so "what actually goes to the server" is testable without
/// a server.
pub fn draft_to_mail(draft: &Draft) -> Result<OutgoingMail> {
    let attachments: Vec<OutgoingAttachment> = serde_json::from_str(&draft.attachments_json)
        .map_err(|e| MailError::Parse(format!("草稿里的附件信息读不出来：{e}")))?;
    let references: Vec<String> = serde_json::from_str(&draft.references).unwrap_or_default();

    Ok(OutgoingMail {
        to: split_addresses(&draft.to_addrs),
        cc: split_addresses(&draft.cc_addrs),
        bcc: split_addresses(&draft.bcc_addrs),
        subject: draft.subject.clone(),
        body_text: draft.body_text.clone(),
        body_html: draft.body_html.clone(),
        in_reply_to: draft.in_reply_to.clone(),
        references,
        attachments,
        // The mirror is not the send: lettre generates an id per build, and the draft's identity on
        // the server is "the newest copy we wrote", which is recorded in `draft_mirrors`.
        message_id: None,
    })
}

/// Split the commas a person typed, accepting the full-width ones too.
fn split_addresses(raw: &str) -> Vec<String> {
    raw.split([',', ';', '，', '；'])
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .map(str::to_string)
        .collect()
}

/// What a mirror attempt did.
///
/// `Serialize` because a Tauri command hands it to the webview — and the compiler's complaint about a
/// missing bound reads like a `Send` problem, which is the trap HANDOFF records.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DraftMirrorReport {
    pub folder: String,
    pub uid: Option<u32>,
    pub message_id: String,
    /// The previous copy that was removed, if there was one.
    pub replaced: Option<u32>,
}

/// Copy a draft to the server's Drafts folder, replacing the copy we wrote last time.
///
/// `design/07` §3/§5.1 calls for a **second** copy of the draft: the local one in `user.db` is the
/// truth, and the server one is so the draft is there when the user opens their mail on another
/// device. Two rules follow from that, and both matter:
///
/// * the mirror is written **without `\Seen`** — a draft is not something you have read;
/// * the previous mirror is deleted **after** the new one is safely written. The other order would
///   leave the user with no copy at all if the append failed.
///
/// `previous` is what the caller read out of `user.db`; a failure to delete it is *not* an error,
/// because the new copy is already there and the next mirror will try again. What must never happen
/// is the reverse: reporting success while the copy is missing.
pub async fn mirror_draft(
    client: &mut ImapClient,
    cfg: &AccountConfig,
    draft: &Draft,
    previous: Option<&DraftMirror>,
) -> Result<Option<DraftMirrorReport>> {
    let Some(folder) = client.drafts_mailbox().await? else {
        // No Drafts folder on this account: the local draft is still saved, which is the promise
        // that matters. `None` says "nothing to mirror into", not "the mirror failed".
        return Ok(None);
    };

    let mail = draft_to_mail(draft)?;
    let message = crate::smtp::build_message(cfg, &mail)?;
    let raw = message.formatted();
    let message_id = crate::smtp::message_id_of(&message).unwrap_or_default();

    // Not `\Seen`: a draft has not been read.
    client.append(&folder, &raw, false).await?;

    // APPEND does not hand back the UID (A1-F43), so ask: the unread copy we just wrote is found by
    // its Message-ID, and without a UID the next mirror can still find it the same way.
    let uid = client.find_uid_by_message_id(&folder, &message_id).await?;

    // Only now is it safe to drop the old copy — and its removal is best-effort.
    let mut replaced = None;
    if let Some(old) = previous
        && let Some(old_uid) = old.uid
    {
        // Same folder only: moving a draft between folders is not something this step does.
        if old.folder == folder && client.delete_message(&folder, old_uid).await.is_ok() {
            replaced = Some(old_uid);
        }
    }

    Ok(Some(DraftMirrorReport {
        folder,
        uid,
        message_id,
        replaced,
    }))
}
