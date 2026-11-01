//! Replay of the offline intent queue (`design/04` §5.5 / SYN-14).
//!
//! Storage for the queue lives in `userdb.rs`; this is the half that consumes it, and it is shaped
//! after `index.rs` for the same reason: the transport is injected, so a fake applier can drive the
//! claim/apply/record logic without a server — which is where all the interesting decisions are.
//!
//! **Why every one of these intents is safe to replay.** Each is idempotent on the server:
//! `STORE +FLAGS (\Seen)` twice is one flag, `MOVE` of an already-moved message is a no-op or an
//! error we treat as done, and `EXPUNGE` of an already-expunged message does not exist any more. That
//! is what makes the crash path (`requeue_pending_ops` at startup) honest: an intent that may or may
//! not have been applied is applied again, and the final state is the same either way. **Do not
//! "improve" this into a check-then-act** — a read followed by an action is exactly the race that
//! needs a second connection to resolve, and the queue exists to avoid spending one.

use crate::error::Result;
use crate::userdb::{PendingOp, PendingOpKind, UserStore};

/// How far one pass may get. Twenty, on the same slow tick as the body index and the outbox: this is
/// background compensation, not a race.
pub const DEFAULT_REPLAY_LIMIT: usize = 20;

/// What one pass did.
///
/// `remaining` is asked again after the pass rather than computed, so the number the UI shows never
/// disagrees with the database (the discipline `IndexReport` already follows).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ReplayReport {
    /// Intents applied to the server in this pass.
    pub applied: u32,
    /// Intents that failed and went back on the queue with a longer backoff.
    pub failed: u32,
    /// How much is still outstanding after the pass.
    pub remaining: u32,
}

/// Everything one pass needs.
pub struct ReplayRun<'a> {
    pub store: &'a UserStore,
    /// "Now", so the backoff arithmetic is testable without a clock.
    pub now: i64,
    pub limit: usize,
}

/// Where a replay applies an intent.
///
/// A trait rather than a closure for the reason `index.rs` gives: the future has to outlive a
/// lifetime the caller chose, and an `async fn` in a trait says exactly that. It also lets a test
/// supply an applier that fails on demand.
#[allow(async_fn_in_trait)]
pub trait OpApplier {
    async fn apply(&mut self, op: &PendingOp) -> Result<()>;
}

/// One bounded pass over the offline intent queue.
///
/// The claim is a compare-and-set in the store, so a tick racing a manual retry cannot apply the
/// same intent twice. A failure is **not** dropped: the row goes back to `queued` with the shared
/// backoff curve, so a server that is down for an hour costs one attempt per interval instead of a
/// burst per tick.
pub async fn replay_pending_ops(
    run: ReplayRun<'_>,
    applier: &mut impl OpApplier,
) -> Result<ReplayReport> {
    let mut report = ReplayReport::default();
    for op in run.store.due_pending_ops(run.now, run.limit)? {
        if !run.store.claim_pending_op(op.id, run.now)? {
            continue;
        }
        if op.kind.is_none() {
            // A row from a newer version, or one this build cannot read. Applying it would be
            // guessing at what the user meant, so it stays queued with a reason attached: the row is
            // their data, and "this version cannot do it" is a fact about this version. The backoff
            // keeps it from spinning, and the outstanding count keeps it visible.
            run.store.retry_pending_op(
                op.id,
                "这个版本不认识这种离线操作，保留原状等新版处理",
                run.now,
            )?;
            report.failed += 1;
            continue;
        }
        match applier.apply(&op).await {
            Ok(()) => {
                run.store.finish_pending_op(op.id, run.now)?;
                report.applied += 1;
            }
            Err(e) => {
                run.store.retry_pending_op(op.id, &e.to_string(), run.now)?;
                report.failed += 1;
            }
        }
    }
    report.remaining = run.store.pending_ops_outstanding()?;
    Ok(report)
}

/// What one intent means for the applier, in words the desktop shell can act on.
///
/// Kept next to the replay so the translation from "a row in the queue" to "a command" has one home;
/// the shell's `OpApplier` matches on it and calls the right `ImapClient` method.
pub fn describe(op: &PendingOp) -> Option<PendingOpKind> {
    op.kind
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::userdb::{PendingOp, PendingOpKind, PendingPayload, UserStore};

    /// An applier that fails the first `failures` calls and then succeeds.
    struct FlakyApplier {
        failures_left: u32,
        calls: u32,
    }

    impl FlakyApplier {
        fn new(failures: u32) -> Self {
            Self {
                failures_left: failures,
                calls: 0,
            }
        }
    }

    #[allow(async_fn_in_trait)]
    impl OpApplier for FlakyApplier {
        async fn apply(&mut self, _op: &PendingOp) -> Result<()> {
            self.calls += 1;
            if self.failures_left > 0 {
                self.failures_left -= 1;
                return Err(crate::error::MailError::Cache("server said no".into()));
            }
            Ok(())
        }
    }

    fn store() -> UserStore {
        UserStore::open_in_memory().expect("store")
    }

    fn seed(store: &UserStore, kind: PendingOpKind) {
        store
            .enqueue_pending_op("acc", "INBOX", Some(5), kind, &PendingPayload::default(), 1)
            .expect("enqueue");
    }

    #[tokio::test]
    async fn a_failed_intent_comes_back_and_then_succeeds() {
        let store = store();
        seed(&store, PendingOpKind::MarkRead);
        let mut applier = FlakyApplier::new(1);

        let first = replay_pending_ops(
            ReplayRun {
                store: &store,
                now: 10,
                limit: 20,
            },
            &mut applier,
        )
        .await
        .expect("first pass");
        assert_eq!(first.applied, 0);
        assert_eq!(first.failed, 1);
        assert_eq!(first.remaining, 1, "the intent is still owed");

        // Not due yet: the backoff moved it into the future.
        let early = replay_pending_ops(
            ReplayRun {
                store: &store,
                now: 11,
                limit: 20,
            },
            &mut applier,
        )
        .await
        .expect("early pass");
        assert_eq!(
            early.applied, 0,
            "retrying immediately is what got the server annoyed"
        );

        let delay = crate::outbox::next_attempt_delay_secs(1);
        let second = replay_pending_ops(
            ReplayRun {
                store: &store,
                now: 10 + delay,
                limit: 20,
            },
            &mut applier,
        )
        .await
        .expect("second pass");
        assert_eq!(second.applied, 1);
        assert_eq!(
            second.remaining, 0,
            "and the count says so, because it is asked for"
        );
        assert_eq!(applier.calls, 2);
    }

    #[tokio::test]
    async fn a_server_that_stays_down_costs_no_more_than_one_attempt_per_interval() {
        let store = store();
        seed(&store, PendingOpKind::Delete);
        let mut applier = FlakyApplier::new(99);

        let first = replay_pending_ops(
            ReplayRun {
                store: &store,
                now: 1,
                limit: 20,
            },
            &mut applier,
        )
        .await
        .expect("pass");
        assert_eq!((first.applied, first.failed, first.remaining), (0, 1, 1));

        // Ten ticks in the same second: the queue is not due again, so nothing is retried.
        for _ in 0..10 {
            let report = replay_pending_ops(
                ReplayRun {
                    store: &store,
                    now: 1,
                    limit: 20,
                },
                &mut applier,
            )
            .await
            .expect("pass");
            assert_eq!(report.applied, 0);
            assert_eq!(
                report.failed, 0,
                "a queue that is not due must not be touched"
            );
        }
        assert_eq!(applier.calls, 1, "one attempt, not eleven");
    }

    #[tokio::test]
    async fn an_unreadable_intent_is_kept_and_reported_not_guessed_at() {
        let store = store();
        seed(&store, PendingOpKind::MarkRead);
        // A row written by a newer build: real intent, unreadable here.
        store
            .force_pending_kind(1, "teleport")
            .expect("plant the future");
        let mut applier = FlakyApplier::new(0);

        let report = replay_pending_ops(
            ReplayRun {
                store: &store,
                now: 5,
                limit: 20,
            },
            &mut applier,
        )
        .await
        .expect("pass");
        assert_eq!(report.failed, 1, "reported as a failure — it is one");
        assert_eq!(
            report.remaining, 1,
            "and it stays owed: it is the user's intent, not junk"
        );
        assert_eq!(applier.calls, 0, "never applied — that would be guessing");
        let row = store.pending_op(1).expect("read").expect("row");
        assert!(
            row.last_error
                .as_deref()
                .is_some_and(|e| e.contains("不认识")),
            "the reason must be on the row, where a human will look for it"
        );
    }

    #[tokio::test]
    async fn an_empty_queue_costs_nothing() {
        let store = store();
        let mut applier = FlakyApplier::new(0);
        let report = replay_pending_ops(
            ReplayRun {
                store: &store,
                now: 1,
                limit: 20,
            },
            &mut applier,
        )
        .await
        .expect("pass");
        assert_eq!((report.applied, report.failed, report.remaining), (0, 0, 0));
        assert_eq!(applier.calls, 0);
    }

    #[tokio::test]
    async fn a_pass_takes_at_most_the_limit() {
        let store = store();
        for uid in 1..=5 {
            store
                .enqueue_pending_op(
                    "acc",
                    "INBOX",
                    Some(uid),
                    PendingOpKind::MarkRead,
                    &PendingPayload::default(),
                    1,
                )
                .expect("enqueue");
        }
        let mut applier = FlakyApplier::new(0);
        let report = replay_pending_ops(
            ReplayRun {
                store: &store,
                now: 2,
                limit: 2,
            },
            &mut applier,
        )
        .await
        .expect("pass");
        assert_eq!(
            report.applied, 2,
            "bounded, like every other background pass here"
        );
        assert_eq!(report.remaining, 3);
        assert_eq!(applier.calls, 2);
    }
}
