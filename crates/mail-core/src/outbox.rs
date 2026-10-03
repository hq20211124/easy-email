//! Outbox decisions that are worth testing on their own.
//!
//! The send worker (`design/07` §5.3) is I/O — SMTP, spool files, timers — and I/O is hard to test.
//! What it *decides* is small and pure, and getting it wrong is expensive in both directions: retry
//! too eagerly and a provider starts throttling us; give up too early and somebody's mail never
//! leaves. So the decisions live here, as functions with no side effects, and the worker just calls
//! them.
//!
//! Nothing in here talks to a server, and none of it needs a database either.

use serde::{Deserialize, Serialize};

/// Where one outbox item stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutboxState {
    /// Waiting for its turn (or for its undo window / scheduled time to pass).
    Queued,
    /// Being delivered right now. A crash leaves rows in this state, which is why startup looks for a
    /// Sent copy before requeueing (`design/07` §5.3.1).
    Sending,
    Sent,
    /// Delivery failed for good. Terminal: the worker will not pick it up again, and the UI offers
    /// "重试" (which requeues it) or "删除". A failure worth retrying goes back to
    /// [`OutboxState::Queued`] with `next_attempt_at` set — which is also how the UI can say
    /// "12 秒后重试" without a fourth state.
    Failed,
    /// The user cancelled it inside the undo window.
    Cancelled,
    /// Waiting for the user to decide — an attachment changed under us, or the server refused the
    /// message size. Retrying automatically would either send the wrong bytes or loop forever.
    NeedsAttention,
}

impl OutboxState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Sending => "sending",
            Self::Sent => "sent",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::NeedsAttention => "needs_attention",
        }
    }

    /// Parse the stored form.
    ///
    /// Not `FromStr`: that trait is expected to fail on bad input, whereas an unreadable state must
    /// land on something safe. The name says "parse" so the difference is visible at the call site.
    ///
    /// An unknown value becomes [`OutboxState::NeedsAttention`] rather than
    /// `Queued`: a row we cannot interpret must not be sent automatically.
    pub fn parse_state(raw: &str) -> Self {
        match raw {
            "queued" => Self::Queued,
            "sending" => Self::Sending,
            "sent" => Self::Sent,
            "failed" => Self::Failed,
            "cancelled" => Self::Cancelled,
            _ => Self::NeedsAttention,
        }
    }

    /// Whether this item still needs work: queued for later, or in flight right now.
    pub fn is_open(self) -> bool {
        matches!(self, Self::Queued | Self::Sending)
    }
}

/// How long to wait before attempt number `attempts + 1`, in seconds.
///
/// Doubling with a floor and a ceiling: short enough that a flaky network recovers inside a minute,
/// long enough that a provider which is throttling us sees a quiet client. Attempt counts are zero
/// based (0 = the first failure).
pub fn next_attempt_delay_secs(attempts: u32) -> i64 {
    const BASE: i64 = 30;
    const MAX: i64 = 6 * 60 * 60; // six hours
    let shift = attempts.min(10); // cap the shift before it overflows
    (BASE << shift).min(MAX)
}

/// Whether a failure means "stop trying".
///
/// The taxonomy that matters (`design/07` §6): 4xx is the server asking us to come back later, so it
/// is retryable. 5xx is a decision — except 421 (service closing) and 450/451/452 (mailbox busy or
/// over quota *right now*), which are transient in practice despite their class. 552 is "message too
/// large", and retrying it can only fail again.
pub fn is_permanent_failure(code: u16) -> bool {
    match code {
        421 | 450 | 451 | 452 => false,
        // Authentication and policy failures: retrying without the user changing something is a loop.
        530..=539 | 550..=559 => true,
        _ if (400..500).contains(&code) => false,
        _ if (500..600).contains(&code) => true,
        // Anything outside the SMTP range is not an SMTP status; treat it as transient so a parsing
        // glitch cannot silently drop queued mail.
        _ => false,
    }
}

/// Whether the item should wait for the user instead of being retried.
///
/// Two cases, both from `design/07` §5.3: the bytes we froze at enqueue time no longer match the file
/// on disk, or the server refused the size. Sending anyway would deliver something the user did not
/// write; retrying is a loop.
pub fn needs_user_decision(reason: Option<&str>) -> bool {
    matches!(
        reason,
        Some("attachment_changed") | Some("attachment_missing") | Some("too_large")
    )
}

/// The SMTP status code inside a failure message, when the server sent one.
///
/// lettre hands back the server's sentence as a string (`"552 5.3.4 message too big"`), so the code
/// has to be read back out of it. Only a standalone three-digit run in the 4xx/5xx range counts:
/// matching any digits would find `1025` in a connect error and read it as a status.
pub fn status_code(message: &str) -> Option<u16> {
    let bytes = message.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if !bytes[index].is_ascii_digit() {
            index += 1;
            continue;
        }
        let start = index;
        while index < bytes.len() && bytes[index].is_ascii_digit() {
            index += 1;
        }
        if index - start == 3
            && let Ok(code) = message[start..index].parse::<u16>()
            && (400..600).contains(&code)
        {
            return Some(code);
        }
    }
    None
}

/// What the worker should do after an attempt failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Come back later: the failure is either the network or the server asking us to wait.
    Retry,
    /// Stop trying. `needs_attention` separates "the attempt failed for good" from "only the user can
    /// decide what to do next", which is what the outbox UI offers different buttons for.
    GiveUp { needs_attention: bool },
}

/// Read a failure message and decide.
///
/// The default is [`Verdict::Retry`], deliberately: anything we cannot classify is *not* proof that
/// the message is undeliverable, and dropping queued mail is the one failure this feature exists to
/// prevent. The cost of being wrong in that direction is a message that keeps trying every six
/// hours, which the user can cancel.
pub fn verdict_for_failure(message: &str) -> Verdict {
    if let Some(code) = status_code(message) {
        // "Message too large" can only fail again, and the fix is a decision (compress, send a
        // link, drop the attachment), which is exactly what needs_attention is for.
        if code == 552 {
            return Verdict::GiveUp {
                needs_attention: needs_user_decision(Some("too_large")),
            };
        }
        if is_permanent_failure(code) {
            return Verdict::GiveUp {
                needs_attention: false,
            };
        }
    }
    Verdict::Retry
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_state_survives_its_stored_form() {
        for state in [
            OutboxState::Queued,
            OutboxState::Sending,
            OutboxState::Sent,
            OutboxState::Failed,
            OutboxState::Cancelled,
            OutboxState::NeedsAttention,
        ] {
            assert_eq!(OutboxState::parse_state(state.as_str()), state);
        }
    }

    #[test]
    fn an_unreadable_state_never_means_send_it() {
        // A row we cannot interpret must not be picked up by the worker.
        for bad in ["", "QUEUED", "weird", "0"] {
            assert_eq!(
                OutboxState::parse_state(bad),
                OutboxState::NeedsAttention,
                "{bad:?} must not become Queued"
            );
        }
        assert!(!OutboxState::NeedsAttention.is_open());
        assert!(!OutboxState::Sent.is_open());
        // A permanently failed row is done as far as the worker is concerned: leaving it "open" is
        // how a give-up turns into a retry loop.
        assert!(!OutboxState::Failed.is_open());
        assert!(OutboxState::Queued.is_open());
        assert!(OutboxState::Sending.is_open());
    }

    #[test]
    fn backoff_grows_then_stops_growing() {
        // Enough attempts to reach the ceiling: 30 s doubling takes ten steps to pass six hours.
        let delays: Vec<i64> = (0..12).map(next_attempt_delay_secs).collect();
        assert_eq!(
            delays[0], 30,
            "the first retry is quick: flaky networks recover"
        );
        assert!(
            delays.windows(2).all(|w| w[0] <= w[1]),
            "the delay must never shrink: {delays:?}"
        );
        assert_eq!(
            *delays.last().expect("last"),
            6 * 60 * 60,
            "it must stop at the ceiling, six hours"
        );
        assert_eq!(
            next_attempt_delay_secs(u32::MAX),
            6 * 60 * 60,
            "a corrupted attempt count must not overflow into a negative delay"
        );
    }

    #[test]
    fn transient_and_permanent_failures_are_told_apart() {
        // Come back later.
        for code in [421u16, 450, 451, 452, 400, 499] {
            assert!(!is_permanent_failure(code), "{code} is transient");
        }
        // Stop and tell the user.
        for code in [530u16, 535, 550, 552, 554, 500, 599] {
            assert!(is_permanent_failure(code), "{code} is permanent");
        }
    }

    #[test]
    fn a_552_is_not_retried_forever() {
        // "Message too large" can only fail again; the design shows it to the user with sizes instead
        // (design/07 §6).
        assert!(is_permanent_failure(552));
    }

    #[test]
    fn an_out_of_range_status_does_not_drop_mail() {
        // Not an SMTP status at all: treating it as permanent would silently discard a queued message.
        assert!(!is_permanent_failure(0));
        assert!(!is_permanent_failure(999));
    }

    #[test]
    fn the_two_cases_that_need_a_human_are_recognised() {
        for reason in ["attachment_changed", "attachment_missing", "too_large"] {
            assert!(needs_user_decision(Some(reason)), "{reason} needs the user");
        }
        for reason in [None, Some("timeout"), Some("connection_reset")] {
            assert!(
                !needs_user_decision(reason),
                "{reason:?} must be handled automatically"
            );
        }
    }

    #[test]
    fn a_status_code_is_read_out_of_the_servers_sentence() {
        assert_eq!(status_code("552 5.3.4 message too big"), Some(552));
        assert_eq!(
            status_code("permanent error: 550 5.1.1 recipient rejected"),
            Some(550)
        );
        // A connect error mentions a port, which is not a status.
        assert_eq!(status_code("connection refused (127.0.0.1:1025)"), None);
        // "5.7.8" is a status code, not an SMTP reply code: only 3-digit runs count.
        assert_eq!(status_code("invalid credentials 5.7.8"), None);
        assert_eq!(status_code("timeout while sending the message"), None);
    }

    #[test]
    fn only_permanent_failures_give_up_and_only_size_asks_the_user() {
        assert_eq!(verdict_for_failure("421 4.3.2 try later"), Verdict::Retry);
        assert_eq!(
            verdict_for_failure("452 4.2.2 mailbox full"),
            Verdict::Retry,
            "a full mailbox is a wait, not a verdict"
        );
        assert_eq!(
            verdict_for_failure("550 5.1.1 no such user"),
            Verdict::GiveUp {
                needs_attention: false
            }
        );
        assert_eq!(
            verdict_for_failure("552 5.3.4 message too big"),
            Verdict::GiveUp {
                needs_attention: true
            },
            "the size fix is a decision, not a retry"
        );
        // Nothing recognisable: never decide that somebody's mail is undeliverable.
        assert_eq!(
            verdict_for_failure("connection reset by peer"),
            Verdict::Retry
        );
    }
}
