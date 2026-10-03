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

use crate::error::Result;
use crate::imap::ImapClient;

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
/// `None` means the account has no Sent folder to file into — a fact about the account, not a
/// failure of this message, and one the caller must not retry forever.
pub async fn ensure_sent_copy(
    client: &mut ImapClient,
    raw: &[u8],
    message_id: &str,
) -> Result<Option<SentCopy>> {
    let Some(folder) = client.sent_mailbox().await? else {
        return Ok(None);
    };

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
}
