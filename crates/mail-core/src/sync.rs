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
//! let mut driver = SyncDriver::start(&open_cache()?, &account_id, &folder, &marks)?;
//! let outcome = loop {
//!     let page = driver.execute(&mut client).await?;    // the only await
//!     if let Some(outcome) = driver.absorb(&open_cache()?, &page)? {
//!         break outcome;
//!     }
//! };
//! ```
//!
//! `src-tauri` and the loopback tests both drive it this way, so the tested path is the shipped one.

use serde::{Deserialize, Serialize};

use crate::cache::Cache;
use crate::error::Result;
use crate::imap::{HeaderPage, ImapClient};
use crate::model::MailboxInfo;
use crate::userdb::Tombstones;

/// How many headers a folder gets on its very first sync.
///
/// Headers only, so this is on the order of a few hundred kilobytes. History beyond this is not
/// cached yet — see the note in README about what search can and cannot see.
pub const INITIAL_SYNC: usize = 500;

/// How much of a mailbox the user asked us to keep.
///
/// This is the answer to the honest question "how far back should this client go?", and it is the
/// user's to make: a 15-year archive is gigabytes and days of downloading, while somebody who only
/// wants this year's mail should not pay for either. The design defaults to
/// [`SyncWindow::Years(2)`] (`design/04` §3, decision P9) because it fits in a reasonable download
/// budget while still covering "I need that invoice from last spring".
///
/// `Count` and `Years` bound how far back a *backfill* walks; `All` is the same walk with no bound,
/// so it is only as fast as the server and the daily byte budget allow — which is why the quota
/// ledger exists (`design/04` §5.7).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
// `tag` + `content`, not `tag` alone: a newtype variant (`Years(u16)`) cannot be serialised with
// internal tagging — serde rejects it at *run time*, not at compile time. The JSON is
// `{"kind":"years","value":2}`, which is also what the frontend sends back.
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum SyncWindow {
    /// Everything the server has.
    All,
    /// The last `n` years, judged by each message's own date.
    Years(u16),
    /// The newest `n` messages per folder.
    Count(u32),
    /// Nothing beyond what the user opens; bodies and older headers arrive on demand only.
    OnDemand,
}

impl Default for SyncWindow {
    fn default() -> Self {
        // P9: the balance between "usable on day one" and "does not download my life".
        Self::Years(2)
    }
}

impl SyncWindow {
    /// How many years back this window reaches, as a timestamp, if it is year-bounded.
    ///
    /// `now` is passed in rather than read from the clock so the behaviour is testable.
    pub fn since_timestamp(self, now: i64) -> Option<i64> {
        match self {
            Self::Years(years) => Some(now - i64::from(years) * 365 * 24 * 60 * 60),
            _ => None,
        }
    }

    /// How many messages per folder this window keeps, if it is count-bounded.
    pub fn max_messages(self) -> Option<u32> {
        match self {
            Self::Count(n) => Some(n),
            // `OnDemand` keeps nothing unopened: the list still needs its first page, and one page is
            // what the user will actually look at.
            Self::OnDemand => Some(INITIAL_SYNC as u32),
            _ => None,
        }
    }

    /// Whether this message is old enough to fall outside the window.
    ///
    /// A message with no date is **kept**: treating "unknown" as "too old" would hide real mail, and
    /// the only cost of keeping it is one header row.
    pub fn is_outside(self, date: Option<i64>, now: i64) -> bool {
        match (self.since_timestamp(now), date) {
            (Some(since), Some(date)) => date < since,
            _ => false,
        }
    }

    /// The stored form: one short string, because it lives in a `kv` row a human may inspect.
    pub fn as_key(self) -> String {
        match self {
            Self::All => "all".to_string(),
            Self::Years(years) => format!("years:{years}"),
            Self::Count(n) => format!("count:{n}"),
            Self::OnDemand => "on_demand".to_string(),
        }
    }

    /// Parse [`SyncWindow::as_key`]. An unreadable value falls back to the default rather than
    /// failing: a corrupt preference must not stop the app from syncing.
    pub fn from_key(raw: &str) -> Self {
        let raw = raw.trim();
        let (head, tail) = raw.split_once(':').unwrap_or((raw, ""));
        let number = tail.trim().parse::<u32>().ok();
        match (head, number) {
            ("all", _) => Self::All,
            ("years", Some(n)) if (1..=100).contains(&n) => Self::Years(n as u16),
            ("count", Some(n)) if n > 0 => Self::Count(n),
            ("on_demand", _) => Self::OnDemand,
            _ => Self::default(),
        }
    }
}

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
    /// Headers the server sent that were **not** stored, because the user deleted them here.
    ///
    /// Reported rather than swallowed: "the server says 17, the list grew by 16" is exactly the kind
    /// of silent disagreement this repository has been bitten by, and one integer in the log line
    /// makes it visible without a debugger.
    pub hidden_by_tombstone: usize,
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
    /// Messages the user deleted on this machine (`design/07` §14.9).
    ///
    /// Carried by the driver rather than looked up inside [`SyncDriver::absorb`] because the two
    /// live in different database files: the driver only ever touches `cache.db`, and reaching into
    /// `user.db` from here would make this type hold two connections across an `await`.
    tombstones: Tombstones,
}

impl SyncDriver {
    /// Read the cache and decide what to fetch next.
    ///
    /// Purely synchronous — the caller is expected to drop the connection before awaiting.
    ///
    /// `tombstones` is a **parameter, not an option**: a sync that does not know what the user
    /// deleted is the bug this argument exists to make impossible to write by accident. Callers with
    /// no `user.db` (the loopback fixtures) pass [`Tombstones::empty`] and say so out loud.
    pub fn start(
        cache: &Cache,
        account_id: &str,
        folder: &MailboxInfo,
        tombstones: &Tombstones,
    ) -> Result<Self> {
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
            tombstones: tombstones.clone(),
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

        // **Check the tombstones before writing, not after.** A message the user deleted is still on
        // the server until the delete reaches it (offline, a queued intent, a server that only
        // flagged `\Deleted`), so every pass over it would otherwise re-create the row — the exact
        // "删了又同步回来" bug, and it survived two rounds of storage work because nothing read the
        // table (`design/07` §14.9).
        //
        // Two things must happen for a hidden message, and only one of them is obvious:
        //   1. it is not stored;
        //   2. any row that is *still there* (a crash between the local delete and this write, or a
        //      delete that only cleared the list) is removed, so the tombstone is self-healing.
        // The cursor is advanced from the **whole page**, hidden messages included — otherwise a
        // deleted message that happens to be the newest one would pin the cursor below itself and
        // every subsequent pass would re-fetch and re-hide it for ever.
        let mut stored = Vec::with_capacity(page.summaries.len());
        let mut hidden = 0usize;
        for summary in &page.summaries {
            if self.tombstones.hides(summary.uid, page.uidvalidity) {
                hidden += 1;
                cache.delete_message(&self.account_id, &self.folder.name, summary.uid)?;
                continue;
            }
            stored.push(summary.clone());
        }
        cache.upsert_summaries(&self.account_id, &self.folder.name, &stored)?;

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
            hidden_by_tombstone: hidden,
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

    // ============================================================== sync window (M1a-3)

    #[test]
    fn the_default_window_is_two_years() {
        // P9. If this ever changes, the README's "what does it download on day one" answer changes too.
        assert_eq!(SyncWindow::default(), SyncWindow::Years(2));
    }

    #[test]
    fn a_year_window_turns_into_a_timestamp() {
        let now = 1_700_000_000i64;
        assert_eq!(
            SyncWindow::Years(2).since_timestamp(now),
            Some(now - 2 * 365 * 24 * 60 * 60)
        );
        assert_eq!(SyncWindow::All.since_timestamp(now), None);
        assert_eq!(SyncWindow::Count(10).since_timestamp(now), None);
    }

    #[test]
    fn outside_the_window_means_older_than_the_cutoff_only() {
        let now = 1_700_000_000i64;
        let window = SyncWindow::Years(2);
        let cutoff = window.since_timestamp(now).expect("cutoff");

        assert!(
            window.is_outside(Some(cutoff - 1), now),
            "a day older is outside"
        );
        assert!(
            !window.is_outside(Some(cutoff + 1), now),
            "a day newer is inside"
        );
        assert!(
            !window.is_outside(None, now),
            "an unknown date must be kept: calling it 'too old' would hide real mail"
        );
    }

    #[test]
    fn every_window_survives_a_round_trip_through_its_stored_form() {
        for window in [
            SyncWindow::All,
            SyncWindow::Years(2),
            SyncWindow::Years(15),
            SyncWindow::Count(5_000),
            SyncWindow::OnDemand,
        ] {
            let key = window.as_key();
            assert_eq!(
                SyncWindow::from_key(&key),
                window,
                "round trip failed for {key}"
            );
        }
    }

    #[test]
    fn every_window_survives_json_in_both_directions() {
        // The commands hand these to the webview as JSON. A newtype variant with internal tagging
        // compiles fine and then fails at runtime, so the shape is pinned here.
        for window in [
            SyncWindow::All,
            SyncWindow::Years(2),
            SyncWindow::Count(5_000),
            SyncWindow::OnDemand,
        ] {
            let json = serde_json::to_string(&window).expect("serialise");
            let back: SyncWindow = serde_json::from_str(&json).expect("deserialise");
            assert_eq!(back, window, "round trip failed for {json}");
        }
        assert_eq!(
            serde_json::to_string(&SyncWindow::Years(2)).expect("serialise"),
            r#"{"kind":"years","value":2}"#,
            "the wire shape is part of the frontend contract"
        );
    }

    #[test]
    fn a_corrupt_stored_window_falls_back_to_the_default() {
        // A preference is not worth refusing to sync over.
        for bad in [
            "",
            "years:",
            "years:0",
            "years:9999",
            "count:0",
            "wat",
            "навсегда",
        ] {
            assert_eq!(
                SyncWindow::from_key(bad),
                SyncWindow::default(),
                "{bad:?} must fall back to the default"
            );
        }
    }

    #[test]
    fn on_demand_still_keeps_one_page() {
        assert_eq!(
            SyncWindow::OnDemand.max_messages(),
            Some(INITIAL_SYNC as u32),
            "the list needs something to show even when nothing is prefetched"
        );
        assert_eq!(SyncWindow::Years(2).max_messages(), None);
        assert_eq!(SyncWindow::Count(7).max_messages(), Some(7));
    }

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
            thread_key: None,
            subject_key: None,
            to_addrs: Vec::new(),
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
        let driver = SyncDriver::start(&cache, "acc", &folder(), &Tombstones::empty()).unwrap();
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
        let mut driver = SyncDriver::start(&cache, "acc", &folder(), &Tombstones::empty()).unwrap();
        let page = HeaderPage {
            uidvalidity: 5,
            summaries: vec![summary(1), summary(2)],
        };
        let outcome = driver.absorb(&cache, &page).unwrap().expect("done");
        assert_eq!(outcome.last_uid, 2);

        let driver = SyncDriver::start(&cache, "acc", &folder(), &Tombstones::empty()).unwrap();
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
        let mut driver = SyncDriver::start(&cache, "acc", &folder(), &Tombstones::empty()).unwrap();
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
        let mut driver = SyncDriver::start(&cache, "acc", &folder(), &Tombstones::empty()).unwrap();
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
        let mut driver = SyncDriver::start(&cache, "acc", &folder(), &Tombstones::empty()).unwrap();
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

    // ============================================================== tombstones (round 113)

    /// The whole bug in one test: delete locally, sync, and the message must **not** come back.
    ///
    /// The real path is `delete_message` (writes a mark) → a sync pass (reads it). Both halves are
    /// exercised here against real SQLite files — `user.db` for the mark, `cache.db` for the rows —
    /// because the interesting failure was never in either half alone: it was that nothing joined
    /// them (`design/07` §14.9).
    #[test]
    fn a_message_deleted_here_does_not_come_back_when_the_server_still_lists_it() {
        let store = crate::userdb::UserStore::open_in_memory().unwrap();
        store
            .mark_deleted("acc", "INBOX", 2, 5, None, 1_000)
            .unwrap();
        let marks = store.tombstones("acc", "INBOX").unwrap();
        assert_eq!(marks.len(), 1);

        let cache = Cache::open_in_memory().unwrap();
        let mut driver = SyncDriver::start(&cache, "acc", &folder(), &marks).unwrap();
        let outcome = driver
            .absorb(
                &cache,
                &HeaderPage {
                    uidvalidity: 5,
                    summaries: vec![summary(1), summary(2), summary(3)],
                },
            )
            .unwrap()
            .expect("done");

        assert_eq!(
            cache.summary_count("acc", "INBOX").unwrap(),
            2,
            "the deleted message must not be stored again"
        );
        assert_eq!(outcome.hidden_by_tombstone, 1, "and the log says so");
        assert_eq!(
            outcome.fetched, 3,
            "fetched counts what the server sent; hiding it is a separate number"
        );
        assert_eq!(
            outcome.last_uid, 3,
            "**the cursor must move past a hidden message**, or every pass re-fetches it for ever"
        );
    }

    /// A row that survived a crash between the local delete and the write is removed, not just
    /// skipped: the tombstone has to be able to clean up after the path that wrote it.
    #[test]
    fn a_row_that_outlived_its_tombstone_is_removed_by_the_next_sync() {
        let cache = Cache::open_in_memory().unwrap();
        let mut driver = SyncDriver::start(&cache, "acc", &folder(), &Tombstones::empty()).unwrap();
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
        assert_eq!(cache.summary_count("acc", "INBOX").unwrap(), 2);

        let marks = Tombstones::from_marks(vec![(2, 5)]);
        let mut driver = SyncDriver::start(&cache, "acc", &folder(), &marks).unwrap();
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
        assert_eq!(
            cache.summary_count("acc", "INBOX").unwrap(),
            1,
            "a row the user deleted must not survive because the write path crashed half way"
        );
    }

    /// `UIDVALIDITY` is half of the identity: a folder the server renumbered hands the same UID to
    /// different mail, and a mark keyed on the number alone would hide a message nobody ever saw.
    #[test]
    fn a_mark_stops_matching_when_the_folder_is_renumbered() {
        let marks = Tombstones::from_marks(vec![(2, 5)]);
        assert!(marks.hides(2, 5));
        assert!(
            !marks.hides(2, 6),
            "a different generation is a different message"
        );
        assert!(
            !marks.hides(2, 0),
            "an unknown generation matches nothing — the same rule sent_index.uidvalidity follows"
        );
        assert!(!marks.hides(3, 5));
        assert!(!Tombstones::empty().hides(2, 5));
    }
}
