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
    /// Delivery failed in a way worth retrying; `next_attempt_at` says when.
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

    /// Whether this item still needs work.
    pub fn is_open(self) -> bool {
        matches!(self, Self::Queued | Self::Sending | Self::Failed)
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
        assert!(OutboxState::Queued.is_open());
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
}
