//! IMAP IDLE: let the server tell us instead of asking it every minute.
//!
//! Without this, "new mail" means polling, which costs a round trip per mailbox per interval and
//! gets accounts rate-limited. IDLE is the server pushing an unsolicited `EXISTS` down a connection
//! that is otherwise silent.
//!
//! The shape of a watcher is: connect → `SELECT` → `IDLE` → wait → `DONE` → report → `IDLE` again.
//! Two things make it survive real networks:
//!
//! * **A finite idle window.** RFC 2177 lets a server drop an idle connection after 30 minutes and
//!   asks clients to re-issue IDLE well before that, so every wait is bounded.
//! * **Reconnect with exponential backoff**, reset as soon as a session manages a full idle window.
//!   A laptop that sleeps, a Wi-Fi change, or a server restart all end the connection; the watcher
//!   has to come back on its own without hammering the server while it is down.
//!
//! The watcher only *reports*. It deliberately does no fetching on its own connection — the whole
//! point of IDLE is that this connection is otherwise idle, and doing work here would delay the next
//! notification. Fetching is the caller's job, over a pooled connection.

use std::time::Duration;

use tokio::sync::mpsc;

use crate::imap::ImapClient;
use crate::model::AccountConfig;

/// How long to sit in IDLE before asking the server for the mailbox size again.
///
/// This is the notification latency, and it is a real limitation rather than a tuning choice: see
/// the note on `ImapClient::idle_then_count` — async-imap does not surface a pushed `EXISTS`, so the
/// change can only be noticed when the idle window ends. Fifty seconds keeps the promise of "within
/// a minute" while costing one `IDLE` + one `SELECT` per cycle.
const IDLE_CHECK_AFTER: Duration = Duration::from_secs(50);

const BACKOFF_START: Duration = Duration::from_secs(1);
const BACKOFF_MAX: Duration = Duration::from_secs(60);

/// Something a watcher wants the app to know about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PushEvent {
    /// The server volunteered something for this mailbox: `EXISTS`, `EXPUNGE`, or a flag change.
    ///
    /// The caller re-syncs and learns how many messages are genuinely new from that sync — which is
    /// the number worth showing, and is more reliable than the mailbox size the server reported,
    /// because it also covers a folder the client is seeing for the first time.
    Changed { mailbox: String },
    /// A connection was lost and a new one is up.
    Reconnected { mailbox: String, attempt: u32 },
    /// An attempt failed. The watcher keeps retrying; this is for logging and for the UI.
    Failed { reason: String },
}

/// Watch `mailbox` and report changes until the receiver is dropped.
///
/// Never returns an error: a failure is reported as [`PushEvent::Failed`] and retried, because a
/// watcher that gives up quietly is indistinguishable from an account that has no new mail.
pub async fn watch(
    cfg: AccountConfig,
    password: String,
    mailbox: String,
    events: mpsc::Sender<PushEvent>,
) {
    watch_every(cfg, password, mailbox, IDLE_CHECK_AFTER, events).await
}

/// [`watch`] with an explicit idle window, so tests do not have to wait fifty seconds per cycle.
pub async fn watch_every(
    cfg: AccountConfig,
    password: String,
    mailbox: String,
    window: Duration,
    events: mpsc::Sender<PushEvent>,
) {
    let mut backoff = BACKOFF_START;
    let mut attempt = 0u32;
    // The size last reported by the server, carried across reconnects: without that, a message that
    // arrived while the connection was down would become the new baseline and never be announced.
    let mut known: Option<u32> = None;

    loop {
        if events.is_closed() {
            return;
        }

        match session_loop(
            &cfg, &password, &mailbox, window, &events, attempt, &mut known,
        )
        .await
        {
            // The receiver went away, i.e. the app is shutting this account down.
            Outcome::Stopped => return,
            Outcome::Lost { reason, cycles } => {
                attempt += 1;
                let _ = events.send(PushEvent::Failed { reason }).await;

                // A session that idled successfully before dying is an ordinary dropped connection,
                // not a server that is down: start the backoff over.
                if cycles > 0 {
                    backoff = BACKOFF_START;
                    attempt = 1;
                }

                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(BACKOFF_MAX);
            }
        }
    }
}

enum Outcome {
    /// The receiver closed; stop entirely.
    Stopped,
    /// The connection was lost, after `cycles` completed idle windows.
    Lost { reason: String, cycles: u32 },
}

async fn session_loop(
    cfg: &AccountConfig,
    password: &str,
    mailbox: &str,
    window: Duration,
    events: &mpsc::Sender<PushEvent>,
    attempt: u32,
    known: &mut Option<u32>,
) -> Outcome {
    let mut client = match ImapClient::connect(cfg, password).await {
        Ok(client) => client,
        Err(error) => {
            return Outcome::Lost {
                reason: error.to_string(),
                cycles: 0,
            };
        }
    };

    let selected = match client.select(mailbox).await {
        Ok(selected) => selected,
        Err(error) => {
            return Outcome::Lost {
                reason: error.to_string(),
                cycles: 0,
            };
        }
    };

    // Announced only once the connection has actually selected the mailbox, so a reconnect is never
    // reported for a connection that then turns out to be useless.
    if attempt > 0
        && events
            .send(PushEvent::Reconnected {
                mailbox: mailbox.to_string(),
                attempt,
            })
            .await
            .is_err()
    {
        return Outcome::Stopped;
    }

    // The first sighting is the baseline, not a change: mail that was already there when the watcher
    // started is not news. `known` survives reconnects, so a message that arrived while the
    // connection was down is still reported once it comes back.
    if !announce_if_changed(known, selected.exists, mailbox, events).await {
        return Outcome::Stopped;
    }

    let mut cycles = 0u32;
    loop {
        match client.idle_then_count(window, mailbox).await {
            Ok((returned, exists)) => {
                client = returned;
                cycles += 1;

                if !announce_if_changed(known, exists, mailbox, events).await {
                    return Outcome::Stopped;
                }
            }
            Err(error) => {
                return Outcome::Lost {
                    reason: error.to_string(),
                    cycles,
                };
            }
        }
    }
}

/// Report a size change, returning false when the receiver has gone away.
async fn announce_if_changed(
    known: &mut Option<u32>,
    exists: u32,
    mailbox: &str,
    events: &mpsc::Sender<PushEvent>,
) -> bool {
    let previous = known.replace(exists);
    if previous.is_none_or(|count| count == exists) {
        return true;
    }
    events
        .send(PushEvent::Changed {
            mailbox: mailbox.to_string(),
        })
        .await
        .is_ok()
}
