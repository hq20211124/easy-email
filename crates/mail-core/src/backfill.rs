//! Walking backwards through a mailbox until the user's sync window is satisfied.
//!
//! The list only ever holds the newest page, so "keep the last two years" has to be reached by paging
//! backwards — which is what this module does, and nothing else. It is deliberately a plain loop over
//! [`ImapClient::fetch_older_headers`] rather than a job: the caller (a command, or the refresh path)
//! owns the pacing, the cancellation and the right to stop early.
//!
//! Three things stop the walk, and all three are normal:
//!
//! * **the window is satisfied** — enough messages, or we crossed the date boundary,
//! * **history is exhausted** — the server had nothing older,
//! * **the budget is spent** — a page or message cap, so one click can never turn into a ten-minute
//!   download. The caller decides whether to ask for more later.
//!
//! The incremental cursor is never touched: everything here is stored *below* `last_uid`, so a
//! backfill can never make the updater skip mail it never stored.

use crate::cache::Cache;
use crate::error::Result;
use crate::imap::ImapClient;
use crate::sync::{INITIAL_SYNC, SyncWindow};

/// How much work one call may do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BackfillBudget {
    /// Pages to fetch at most.
    pub max_pages: u32,
    /// Messages to store at most (counted across pages).
    pub max_messages: u32,
    /// Bytes to fetch at most, as the sum of the sizes the headers report.
    ///
    /// This is the quota bound (`design/04` §5.7): Gmail allows roughly 2.5 GB of IMAP downloads a
    /// day and throttles the account when it is exceeded, so the walk has to stop on its own rather
    /// than wait to be told. `u64::MAX` means "no byte bound" — for an account whose provider has no
    /// documented quota, a made-up number would throttle somebody for no reason.
    pub max_bytes: u64,
}

impl Default for BackfillBudget {
    fn default() -> Self {
        // One page: enough to make "the mail just below the list" appear without turning a click into
        // a long download. Callers that want more ask again.
        Self {
            max_pages: 1,
            max_messages: INITIAL_SYNC as u32,
            max_bytes: u64::MAX,
        }
    }
}

impl BackfillBudget {
    /// A budget for "walk to the window edge", still bounded so it cannot run away.
    pub fn generous() -> Self {
        Self {
            max_pages: 40,
            max_messages: 20_000,
            max_bytes: u64::MAX,
        }
    }
}

/// Why the walk stopped.
///
/// `Serialize` because a Tauri command hands this to the webview: a command's return type must be
/// serialisable, and the compiler's complaint about a missing bound reads like a `Send` problem.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BackfillStop {
    /// The window is satisfied (or the date boundary has been crossed).
    WindowSatisfied,
    /// The server had nothing older.
    HistoryExhausted,
    /// The budget ran out; asking again continues from here.
    BudgetSpent,
    /// The account's daily download quota is used up (`design/04` §5.7). Distinct from
    /// [`BackfillStop::BudgetSpent`] because the user has to be told *why* to stop and when it
    /// resumes — and because nothing may quietly continue past it.
    QuotaSpent,
}

/// What one walk did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub struct BackfillReport {
    pub pages: u32,
    pub fetched: u32,
    /// Sum of the message sizes the walk stored, which is what the quota ledger is debited by.
    pub bytes: u64,
    pub stop: BackfillStop,
}

/// Re-store the newest `pages` pages of headers, so cached rows get their conversation keys.
///
/// The `thread_key` column arrived after those rows were written, and no future sync will fill them:
/// an incremental sync asks only for UIDs *above* the cursor, and the rows below it were stored
/// without their `References` chain. Left alone, old mail keeps the subject-based fallback forever
/// while new mail groups exactly — two behaviours in one list, which is worse than either.
///
/// Bounded on purpose: each page is a round trip, and the rows the user actually looks at are the
/// newest ones. Returns how many messages came back with a key.
pub async fn fill_thread_keys(
    client: &mut ImapClient,
    cache: &Cache,
    account_id: &str,
    folder: &str,
    pages: u32,
    page_size: usize,
) -> Result<u32> {
    let mut filled = 0u32;

    // The first page is the *newest* one, and it is asked for by count rather than by a UID range:
    // starting from `u32::MAX` asks the server for `1:4294967294`, a range no real mailbox has — and
    // which made this walk's own fixture try to allocate for it. Everything after that walks strictly
    // downwards from the oldest UID in hand.
    let newest = client.fetch_newest_headers(folder, page_size).await?;
    filled += newest
        .summaries
        .iter()
        .filter(|summary| summary.thread_key.is_some())
        .count() as u32;
    if !newest.summaries.is_empty() {
        cache.upsert_summaries(account_id, folder, &newest.summaries)?;
    }
    let mut before = newest
        .summaries
        .iter()
        .map(|summary| summary.uid)
        .min()
        .unwrap_or(0);

    for _ in 1..pages.max(1) {
        if before <= 1 {
            break;
        }
        let page = client
            .fetch_older_headers(folder, before, page_size)
            .await?;
        if page.is_empty() {
            break;
        }

        filled += page
            .iter()
            .filter(|summary| summary.thread_key.is_some())
            .count() as u32;

        // `fetch_older_headers` returns ascending UIDs; the next page starts below the oldest one.
        let oldest = page
            .iter()
            .map(|summary| summary.uid)
            .min()
            .unwrap_or(before);
        cache.upsert_summaries(account_id, folder, &page)?;
        before = oldest.saturating_sub(1);
    }

    Ok(filled)
}

/// Page backwards until the window is satisfied, history ends, or the budget is spent.
///
/// `now` is a parameter rather than a clock read so a test can pin the date boundary.
pub async fn backfill_older(
    client: &mut ImapClient,
    cache: &Cache,
    account_id: &str,
    folder: &str,
    window: SyncWindow,
    budget: BackfillBudget,
    now: i64,
) -> Result<BackfillReport> {
    let page_size = INITIAL_SYNC;
    let mut pages = 0u32;
    let mut fetched = 0u32;
    let mut bytes = 0u64;

    loop {
        if pages >= budget.max_pages || fetched >= budget.max_messages {
            return Ok(BackfillReport {
                pages,
                fetched,
                bytes,
                stop: BackfillStop::BudgetSpent,
            });
        }

        let Some(oldest) = cache.oldest_uid(account_id, folder)? else {
            // Nothing cached at all: there is no "older than" to ask about. The sync path owns filling
            // the first page.
            return Ok(BackfillReport {
                pages,
                fetched,
                bytes,
                stop: BackfillStop::HistoryExhausted,
            });
        };

        let remaining = budget.max_messages.saturating_sub(fetched) as usize;
        // How much of a page the window still wants. Without this the walk fetched a full
        // `INITIAL_SYNC` page to satisfy a window that needed sixteen messages — which is what the
        // first version did, and what the loopback test for the window caught.
        let wanted = match window {
            SyncWindow::Count(target) => {
                let have = cache.summary_count(account_id, folder)?;
                page_size.min(target.saturating_sub(have).max(1) as usize)
            }
            _ => page_size,
        };
        let want = wanted.min(remaining.max(1));
        let page = client.fetch_older_headers(folder, oldest, want).await?;
        if page.is_empty() {
            return Ok(BackfillReport {
                pages,
                fetched,
                bytes,
                stop: BackfillStop::HistoryExhausted,
            });
        }

        // Store **before** deciding: the page is what the user is about to see, and a stop decision
        // that discarded it would re-fetch the same page next time.
        let newest_in_page = page.iter().filter_map(|m| m.date).max();
        let page_bytes: u64 = page.iter().map(|m| u64::from(m.size.unwrap_or(0))).sum();
        cache.upsert_summaries(account_id, folder, &page)?;
        pages += 1;
        fetched += page.len() as u32;
        bytes += page_bytes;

        // The daily quota is the one limit that must never be crossed quietly: exceeding it gets the
        // account throttled by the provider, which costs the user far more than stopping here
        // (`design/04` §5.7). Checked before the window rules, because "the window is satisfied" is
        // not a reason to keep going if the quota is gone *and* not a reason to hide it either.
        if bytes >= budget.max_bytes {
            return Ok(BackfillReport {
                pages,
                fetched,
                bytes,
                stop: BackfillStop::QuotaSpent,
            });
        }

        match window {
            SyncWindow::Count(target) => {
                if cache.summary_count(account_id, folder)? >= target {
                    return Ok(BackfillReport {
                        pages,
                        fetched,
                        bytes,
                        stop: BackfillStop::WindowSatisfied,
                    });
                }
            }
            SyncWindow::Years(_) | SyncWindow::All => {
                // We walk backwards, so the first page whose newest message is already outside the
                // window is the page that crossed the boundary. `All` never crosses it.
                if let Some(newest) =
                    newest_in_page.filter(|date| window.is_outside(Some(*date), now))
                {
                    let _ = newest;
                    return Ok(BackfillReport {
                        pages,
                        fetched,
                        bytes,
                        stop: BackfillStop::WindowSatisfied,
                    });
                }
            }
            SyncWindow::OnDemand => {
                // "Only what the user opens": one page of context is the most this window ever
                // means by prefetching.
                return Ok(BackfillReport {
                    pages,
                    fetched,
                    bytes,
                    stop: BackfillStop::WindowSatisfied,
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_budget_is_one_page() {
        // A click must not become a ten-minute download.
        assert_eq!(BackfillBudget::default().max_pages, 1);
        assert_eq!(BackfillBudget::default().max_messages, INITIAL_SYNC as u32);
    }

    #[test]
    fn the_generous_budget_is_still_bounded() {
        let budget = BackfillBudget::generous();
        assert!(
            budget.max_pages >= 10,
            "walking to a window edge needs pages"
        );
        assert!(
            budget.max_messages <= 50_000,
            "an unbounded walk would fight the daily byte budget (design/04 §5.7)"
        );
    }
}
