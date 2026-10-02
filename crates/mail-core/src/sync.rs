//! Incremental folder synchronisation: keep the local cache current without re-reading the folder.
//!
//! The shape of a sync depends on whether the folder has been seen before:
//!
//! * **First time** — fetch the newest [`INITIAL_SYNC`] headers. Deliberately *not* everything from
//!   UID 1: a folder with twenty thousand messages would make the first launch unusable, and the UI
//!   only ever shows one page at a time anyway.
//! * **After that** — `UID FETCH <last_uid+1>:*`, so a sync costs one round trip regardless of how
//!   big the folder is.
//! * **When the server changes `UIDVALIDITY`** — every cached UID may now refer to a different
//!   message, so the folder is discarded and re-filled rather than shown with mismatched headers.
//!
//! # Why this is a driver rather than one `async fn`
//!
//! `rusqlite::Connection` is `!Sync` (it holds a `RefCell` internally), so **any future that holds a
//! `&Cache` across an `.await` is not `Send`** — and a Tauri command's future must be. The same is
//! true of a `tokio::sync::MutexGuard<Cache>`, whose `Send`ness requires `Cache: Sync`. Sharing one
//! connection behind a mutex therefore does not solve it.
//!
//! So the cache is only ever touched *between* awaits. [`SyncDriver`] carries the decision as owned
//! data, which is `Send`, and callers open a connection for each synchronous phase:
//!
//! ```ignore
//! let mut driver = SyncDriver::start(&open_cache()?, &account_id, &folder)?;
//! let outcome = loop {
//!     let page = driver.execute(&mut client).await?;    // the only await
//!     if let Some(outcome) = driver.absorb(&open_cache()?, &page)? {
//!         break outcome;
//!     }
//! };
//! ```
//!
//! `src-tauri` and the loopback tests both drive it this way, so the tested path is the shipped one.

use serde::Serialize;

use crate::cache::Cache;
use crate::error::Result;
use crate::imap::{HeaderPage, ImapClient};
use crate::model::MailboxInfo;

/// How many headers a folder gets on its very first sync.
///
/// Headers only, so this is on the order of a few hundred kilobytes. History beyond this is not
/// cached yet — see the note in README about what search can and cannot see.
pub const INITIAL_SYNC: usize = 500;

/// Upper bound on how many new headers one incremental pass will store.
const SYNC_BATCH: usize = 500;

/// What one sync did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SyncOutcome {
    /// Headers newly stored by this pass.
    pub fetched: usize,
    /// How many messages the folder now has cached in total.
    pub total_cached: u32,
    /// The server had changed `UIDVALIDITY`: the folder was wiped and re-filled.
    pub uidvalidity_changed: bool,
    /// Where the incremental cursor stands after this pass.
    pub last_uid: u32,
}

/// The one fetch a pass needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fetch {
    /// A cold folder, or one whose generation just changed: take the newest window.
    Newest { limit: usize },
    /// Everything above a UID.
    Since { after_uid: u32, limit: usize },
}

/// Carries a sync across the one `await` it needs.
///
/// Holds only owned data on purpose: it must stay `Send` so a Tauri command can hold it while the
/// network call is in flight, without also holding the `!Sync` SQLite connection.
#[derive(Debug, Clone)]
pub struct SyncDriver {
    account_id: String,
    folder: MailboxInfo,
    fetch: Fetch,
    /// Generation the cached rows belong to; 0 means "unknown or just invalidated".
    cached_uidvalidity: u32,
    /// Set once a generation change has been handled, so the restart cannot loop.
    restarted: bool,
}

impl SyncDriver {
    /// Read the cache and decide what to fetch next.
    ///
    /// Purely synchronous — the caller is expected to drop the connection before awaiting.
    pub fn start(cache: &Cache, account_id: &str, folder: &MailboxInfo) -> Result<Self> {
        let state = cache.folder_state(account_id, &folder.name)?;
        let (fetch, cached_uidvalidity) = match state {
            // Nothing cached, or the folder was invalidated and its generation zeroed.
            Some(state) if state.uidvalidity != 0 => (
                Fetch::Since {
                    after_uid: state.last_uid,
                    limit: SYNC_BATCH,
                },
                state.uidvalidity,
            ),
            _ => (
                Fetch::Newest {
                    limit: INITIAL_SYNC,
                },
                0,
            ),
        };
        Ok(Self {
            account_id: account_id.to_string(),
            folder: folder.clone(),
            fetch,
            cached_uidvalidity,
            restarted: false,
        })
    }

    pub fn fetch(&self) -> Fetch {
        self.fetch
    }

    /// Perform this pass's fetch. The only network call in a sync.
    pub async fn execute(&self, client: &mut ImapClient) -> Result<HeaderPage> {
        match self.fetch {
            Fetch::Newest { limit } => client.fetch_newest_headers(&self.folder.name, limit).await,
            Fetch::Since { after_uid, limit } => {
                client
                    .fetch_headers_since(&self.folder.name, after_uid, limit)
                    .await
            }
        }
    }

    /// Store what came back.
    ///
    /// Returns `None` when the server renumbered the folder: the stale rows have been discarded and
    /// the driver now asks for a fresh window, so the caller should loop and call
    /// [`SyncDriver::execute`] again. That second pass always completes — `invalidate_folder` zeroes
    /// the stored generation, so no further change can be detected on it.
    pub fn absorb(&mut self, cache: &Cache, page: &HeaderPage) -> Result<Option<SyncOutcome>> {
        if !self.restarted
            && self.cached_uidvalidity != 0
            && page.uidvalidity != self.cached_uidvalidity
        {
            cache.invalidate_folder(&self.account_id, &self.folder.name)?;
            self.fetch = Fetch::Newest {
                limit: INITIAL_SYNC,
            };
            self.cached_uidvalidity = 0;
            self.restarted = true;
            return Ok(None);
        }

        let previous = cache.folder_state(&self.account_id, &self.folder.name)?;
        cache.upsert_folder(&self.account_id, &self.folder, page.uidvalidity)?;
        cache.upsert_summaries(&self.account_id, &self.folder.name, &page.summaries)?;

        let last_uid = advance_cursor(previous, &page.summaries);
        cache.set_cursor(
            &self.account_id,
            &self.folder.name,
            page.uidvalidity,
            last_uid,
        )?;

        Ok(Some(SyncOutcome {
            fetched: page.summaries.len(),
            total_cached: cache.summary_count(&self.account_id, &self.folder.name)?,
            uidvalidity_changed: self.restarted,
            last_uid,
        }))
    }
}

/// Where the cursor ends up.
///
/// Advancing to the highest stored UID is what makes the next sync incremental. When nothing new
/// arrived the previous value is kept, because re-fetching the same range next time is the point of
/// keeping it.
fn advance_cursor(
    previous: Option<crate::cache::FolderState>,
    page: &[crate::model::MessageSummary],
) -> u32 {
    if let Some(highest) = page.iter().map(|m| m.uid).max() {
        return highest;
    }
    previous.map(|state| state.last_uid).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::MessageSummary;

    fn summary(uid: u32) -> MessageSummary {
        MessageSummary {
            uid,
            subject: format!("主题 {uid}"),
            from_name: None,
            from_addr: Some("a@b.c".to_string()),
            date: Some(i64::from(uid)),
            size: None,
            seen: false,
            flagged: false,
        }
    }

    fn folder() -> MailboxInfo {
        MailboxInfo {
            name: "INBOX".to_string(),
            display_name: "收件箱".to_string(),
            delimiter: Some("/".to_string()),
            selectable: true,
        }
    }

    #[test]
    fn the_cursor_advances_to_the_highest_stored_uid() {
        assert_eq!(
            advance_cursor(None, &[summary(3), summary(9), summary(5)]),
            9
        );
    }

    #[test]
    fn an_empty_page_keeps_the_previous_cursor() {
        let state = crate::cache::FolderState {
            uidvalidity: 7,
            last_uid: 42,
        };
        assert_eq!(advance_cursor(Some(state), &[]), 42);
    }

    #[test]
    fn a_cold_folder_asks_for_the_newest_window() {
        let cache = Cache::open_in_memory().unwrap();
        let driver = SyncDriver::start(&cache, "acc", &folder()).unwrap();
        assert_eq!(
            driver.fetch(),
            Fetch::Newest {
                limit: INITIAL_SYNC
            }
        );
    }

    #[test]
    fn a_known_folder_asks_for_everything_above_the_cursor() {
        let cache = Cache::open_in_memory().unwrap();
        let mut driver = SyncDriver::start(&cache, "acc", &folder()).unwrap();
        let page = HeaderPage {
            uidvalidity: 5,
            summaries: vec![summary(1), summary(2)],
        };
        let outcome = driver.absorb(&cache, &page).unwrap().expect("done");
        assert_eq!(outcome.last_uid, 2);

        let driver = SyncDriver::start(&cache, "acc", &folder()).unwrap();
        assert_eq!(
            driver.fetch(),
            Fetch::Since {
                after_uid: 2,
                limit: SYNC_BATCH
            }
        );
    }

    #[test]
    fn a_changed_generation_discards_the_folder_and_asks_for_a_fresh_window() {
        let cache = Cache::open_in_memory().unwrap();
        let mut driver = SyncDriver::start(&cache, "acc", &folder()).unwrap();
        driver
            .absorb(
                &cache,
                &HeaderPage {
                    uidvalidity: 5,
                    summaries: vec![summary(1), summary(2)],
                },
            )
            .unwrap()
            .expect("done");

        // The server renumbers the folder.
        let mut driver = SyncDriver::start(&cache, "acc", &folder()).unwrap();
        assert!(
            driver
                .absorb(
                    &cache,
                    &HeaderPage {
                        uidvalidity: 6,
                        summaries: vec![summary(9)],
                    },
                )
                .unwrap()
                .is_none(),
            "a generation change must ask the caller to fetch again"
        );
        assert_eq!(
            cache.summary_count("acc", "INBOX").unwrap(),
            0,
            "the stale rows must be gone"
        );
        assert_eq!(
            driver.fetch(),
            Fetch::Newest {
                limit: INITIAL_SYNC
            },
            "and the next pass must take a fresh window rather than trusting the dead cursor"
        );

        // The restart terminates, and reports that it happened.
        let outcome = driver
            .absorb(
                &cache,
                &HeaderPage {
                    uidvalidity: 6,
                    summaries: vec![summary(9)],
                },
            )
            .unwrap()
            .expect("done");
        assert_eq!(outcome.total_cached, 1);
        assert_eq!(outcome.last_uid, 9);
        assert!(
            outcome.uidvalidity_changed,
            "the caller should be able to say so"
        );
    }

    #[test]
    fn a_server_that_reports_no_generation_at_all_never_looks_changed() {
        // Some servers omit UIDVALIDITY. Treating that as a change would wipe the folder on every
        // single sync, so 0 must mean "cannot tell".
        let cache = Cache::open_in_memory().unwrap();
        let mut driver = SyncDriver::start(&cache, "acc", &folder()).unwrap();
        driver
            .absorb(
                &cache,
                &HeaderPage {
                    uidvalidity: 0,
                    summaries: vec![summary(1)],
                },
            )
            .unwrap()
            .expect("done");

        assert_eq!(
            cache.summary_count("acc", "INBOX").unwrap(),
            1,
            "a folder whose server reports no generation must not be wiped"
        );
    }
}
