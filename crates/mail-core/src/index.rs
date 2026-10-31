//! One bounded pass of "fetch the bodies we have not read yet, so search can see them".
//!
//! Why a pass and not part of the sync path: `imap.rs` has no body-only primitive, so indexing a
//! message costs exactly what opening it costs. Doing that inside the sync path would turn a
//! 500-header refresh into hundreds of full fetches, competing with IDLE and the manual refresh for
//! the same connection pool (`design/20-v0.2-plan.md` C2). A bounded pass, on its own tick, with the
//! transport injected by the caller, is the shape [`crate::worker`] already uses for the outbox.

use crate::cache::Cache;
use crate::error::Result;
use crate::model::MessageDetail;
use std::time::Duration;

/// How many messages one pass may fetch.
///
/// Equal to [`MAX_BATCH_MESSAGES`] on purpose: a pass now costs one round trip per folder, so the
/// cheapest shape is "one command, one batch". Before 2026-10-20 this was 20 because **each message**
/// cost its own round trip and a bigger number meant a longer uninterrupted hold on the connection;
/// with the batch, `reports/performance.md` §16.7 measured that the pass itself takes ~150 ms on a
/// typical link, so the limit is free and the *pace* is what has to be chosen — see
/// [`BackfillCadence`].
pub const DEFAULT_PASS_LIMIT: usize = MAX_BATCH_MESSAGES;

/// How much raw mail text one batched `UID FETCH` may ask for, summed over its UIDs.
///
/// The reason this is a **byte** ceiling and not a count: the server sends the whole response before
/// the client is done with the first message, so a batch's peak memory is proportional to everything
/// it asked for. [`crate::imap`]'s single-message ceiling is 128 MB, so a count-based batch could
/// multiply that — ten fat mailing-list digests would take the process out, and a mail client that
/// dies mid-index shows the user nothing. 8 MB is deliberately below what one message may already be,
/// which makes this ceiling strictly safer than the code it replaces.
pub const MAX_BATCH_BYTES: u64 = 8 * 1024 * 1024;

/// The other half of the ceiling: however small the mail is, one command asks for at most this many.
///
/// The reason is the **command line**, not memory: UIDs are written out comma-separated, so 50 of them
/// is ~400–500 characters before the `(UID FLAGS … BODY.PEEK[])` item list, and many servers cap an
/// IMAP command line around 1000 characters. That is also why this is not simply raised whenever a
/// bigger batch would be faster — the byte ceiling ([`MAX_BATCH_BYTES`]) bounds what one command may
/// *return*, this one bounds what it may *say*.
///
/// [`DEFAULT_PASS_LIMIT`] equals it, which makes a pass exactly one command per folder.
pub const MAX_BATCH_MESSAGES: usize = 50;

/// How much of a body goes into the search index, in characters.
///
/// `design/09` §11's default of 64 KB, counted in characters rather than bytes because the mail it
/// matters for is Chinese — a byte cap would cut the last character of nearly every message.
pub const DEFAULT_BODY_INDEX_CHARS: usize = 64 * 1024;

/// One message whose body is not searchable yet — for one of two reasons, told apart by
/// [`Self::has_body`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingBody {
    pub folder: String,
    pub uid: u32,
    /// The body **is** in the cache; only its index row predates the current shape
    /// ([`crate::cache::INDEX_VERSION`]). Such a row is re-indexed locally — `bodies.detail` holds
    /// the whole message, so the pass must not spend a connection (or a quota) fetching it again.
    /// `false` means there is no body at all and the message has to be opened, which costs what
    /// opening costs and is the reason the pass is bounded.
    pub has_body: bool,
    /// `RFC822.SIZE` from the header sync, when the row has one.
    ///
    /// Batched fetches are bounded by bytes (see [`MAX_BATCH_BYTES`]), and the only cheap way to know
    /// a message's size before asking for it is the header the sync already stored. `None` — a row
    /// written by a version that did not fetch the size — is treated as "as big as a message can be",
    /// so such a row is still fetched on its own, exactly as it was before batching existed.
    pub size: Option<u32>,
}

/// Which messages the next pass should work on.
///
/// Pure so the cap and the ordering can be pinned without a database: the caller hands over what is
/// pending (newest first, from `Cache::bodies_pending`) and this decides how much of it one pass may
/// take. It must not reorder — reordering would make the newest mail the last to be indexed.
pub fn pick_pending(pending: Vec<PendingBody>, limit: usize) -> Vec<PendingBody> {
    pending.into_iter().take(limit).collect()
}

/// Group one folder's UIDs into as few fetch commands as [`MAX_BATCH_BYTES`] allows.
///
/// Pure, and asked for one folder at a time, because `UID FETCH` is scoped to the current selection:
/// mixing folders in one set is precisely the bug fixed on 2026-10-20. Order is preserved
/// (newest first), and each batch's UIDs stay in that order — the caller must not turn this into a
/// range, because UIDs are not dense and a range would fetch mail nobody asked for.
///
/// A single message bigger than the ceiling becomes a batch of one, which is what it cost before
/// batching existed; it is never dropped, and never merged with a neighbour.
pub fn plan_batches(items: &[&PendingBody]) -> Vec<Vec<u32>> {
    let mut batches: Vec<Vec<u32>> = Vec::new();
    let mut current: Vec<u32> = Vec::new();
    let mut current_bytes: u64 = 0;
    for item in items {
        let cost = item.size.unwrap_or(u32::MAX) as u64;
        let fits = current.len() < MAX_BATCH_MESSAGES && current_bytes + cost <= MAX_BATCH_BYTES;
        if !current.is_empty() && !fits {
            batches.push(std::mem::take(&mut current));
            current_bytes = 0;
        }
        current.push(item.uid);
        current_bytes += cost;
    }
    if !current.is_empty() {
        batches.push(current);
    }
    batches
}

/// What one pass did.
///
/// Serialisable because it crosses the Tauri IPC boundary: a command whose return type is not
/// `IpcResponse` does not compile, and that needs `Serialize` on everything inside.
///
/// Returned rather than only logged so the caller can decide whether another pass is worth
/// scheduling — and so the UI never has to guess "done": `remaining` is asked for again after the
/// pass instead of being subtracted, so failures and mail that arrived mid-pass are both counted.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct IndexReport {
    /// Bodies fetched and stored (and therefore indexed) by this pass.
    pub indexed: u32,
    /// Messages whose fetch failed. Still pending; a later pass will try them again.
    pub failed: u32,
    /// How much is still un-indexed after the pass.
    pub remaining: u32,
    /// The pass stopped early because the **user** showed up ([`BodySource::should_yield`]).
    ///
    /// Separate from `remaining > 0` because the two call for different waits: an unfinished pass that
    /// ran out of mail can pace itself by duty cycle, while one that gave way should re-check soon
    /// and, until the connection frees up, do nothing. Without this flag the cadence would read a
    /// near-zero `elapsed` and schedule the next pass almost immediately — hammering the very moment
    /// the user is trying to work.
    pub yielded: bool,
}

/// How much of one account's connection the background backfill may occupy, in permille.
///
/// 250 ‰ = a quarter, and the number comes from arithmetic, not taste. With [`MAX_BATCH_MESSAGES`]
/// messages per round trip, a duty cycle `d` buys a sustained rate of `d × 50 / 往返延迟`:
///
/// | 往返 | d = 15 %（2026-10-20 §16.4 用的数） | d = 25 % |
/// | --- | --- | --- |
/// | 150 ms | 50 封/秒 | **83 封/秒** |
/// | 400 ms | 18.8 封/秒 ❌ | **31 封/秒** ✅ |
///
/// The 20 封/秒 budget therefore needs `d ≥ 20 × 0.4 / 50 = 16 %` on the slowest arm that has been
/// measured. 15 % cannot reach it; 25 % clears it with margin and still leaves three quarters of every
/// second for IDLE, a manual refresh and opening a message. Raising it further buys nothing the user
/// can see — the interactive path is P8a, which is already green — it only competes.
///
/// **The table is the connection's own arithmetic** — one round trip per pass. The ruler in the unit
/// test `the_budget_clears_on_every_link_latency_that_has_been_measured` adds the app's work on top
/// (≈1 ms/封, the §16.3 figure rounded up), which is why its numbers read lower for the same duty
/// cycle: 16.7 rather than 18.8 封/秒 at 15 %. Both are true of what they count, and the stricter one
/// is the one that fails a build.
pub const BACKFILL_DUTY_PERMILLE: u16 = 250;

/// How long to wait when a pass found nothing left to index.
///
/// Sixty seconds is the tick the worker has used since it was written, and it is the *right* number
/// for an empty queue: the interesting events (a manual refresh, an IDLE push, the search box's own
/// button) all run a pass on demand, so this is only the catch-up for mail that arrived while nobody
/// was looking.
pub const BACKFILL_QUIET: Duration = Duration::from_secs(60);

/// How long to wait after a pass gave way to the user before asking whether the coast is clear.
pub const BACKFILL_RECHECK: Duration = Duration::from_millis(500);

/// What the backfill worker waits on. Split out from the worker loop so the pacing is a value that
/// can be asserted — a `sleep` written inline in a `loop` is the one part of this design that cannot
/// be reviewed by reading it, and it is the part that decides whether the user ever gets their mail.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BackfillCadence {
    /// See [`BACKFILL_DUTY_PERMILLE`].
    pub duty_permille: u16,
    /// See [`BACKFILL_QUIET`].
    pub quiet: Duration,
    /// See [`BACKFILL_RECHECK`].
    pub recheck: Duration,
}

impl Default for BackfillCadence {
    fn default() -> Self {
        Self {
            duty_permille: BACKFILL_DUTY_PERMILLE,
            quiet: BACKFILL_QUIET,
            recheck: BACKFILL_RECHECK,
        }
    }
}

/// What to do after one pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackfillWait {
    /// Nothing pending: this long before the cache is asked again.
    Idle(Duration),
    /// Work remains and nobody was in the way: the duty-cycle gap, derived from how long the pass
    /// actually took, so a slow link waits longer in absolute terms and every link keeps the same
    /// share of the connection.
    Pace(Duration),
    /// The pass gave way to the user: ask again this soon, and keep giving way until it frees up.
    Recheck(Duration),
}

/// The gap that holds `duty_permille` given a pass that took `elapsed`.
///
/// Pure, and the reason it is written in nanoseconds rather than with a float is that the caller is
/// comparing durations in tests: `elapsed × (1000 − d) / d` at `d = 250` is exactly `3 × elapsed`, and
/// an off-by-a-rounding-step there would make the budget assertion in
/// `tests/loopback.rs` hinge on floating point.
pub fn duty_gap(elapsed: Duration, duty_permille: u16) -> Duration {
    let duty = u128::from(duty_permille.clamp(1, 1000));
    let gap = elapsed
        .as_nanos()
        .saturating_mul(1000 - duty)
        .checked_div(duty)
        .unwrap_or(u128::MAX);
    Duration::from_nanos(u64::try_from(gap).unwrap_or(u64::MAX))
}

/// Decide what the worker waits for next, given what the last pass did.
pub fn backfill_wait(
    cadence: &BackfillCadence,
    report: &IndexReport,
    elapsed: Duration,
) -> BackfillWait {
    if report.yielded {
        return BackfillWait::Recheck(cadence.recheck);
    }
    if report.remaining == 0 {
        return BackfillWait::Idle(cadence.quiet);
    }
    if report.indexed == 0 {
        // Nothing that this pass could do, did. Pacing this off `elapsed` would be a retry storm —
        // a failing pass is *fast*, so a 25 % duty cycle on a pass that took 5 ms means "again in
        // 15 ms", forever. A server hiccup, an unreadable folder and an account whose mail all fails
        // to parse are all cases where waiting a minute is the only useful answer.
        return BackfillWait::Idle(cadence.quiet);
    }
    // Capped at `quiet`: a pass that took longer than the whole quiet tick — an offline account
    // retrying a handshake, a VPN reconnecting — must not turn into a multi-minute silence while mail
    // is still waiting.
    BackfillWait::Pace(duty_gap(elapsed, cadence.duty_permille).min(cadence.quiet))
}

impl BackfillWait {
    /// How long the caller should actually sleep. A method rather than a match at every call site
    /// because the three variants mean different things to a reader and the same thing to a timer.
    pub fn after(self) -> Duration {
        match self {
            BackfillWait::Idle(d) | BackfillWait::Pace(d) | BackfillWait::Recheck(d) => d,
        }
    }
}

/// Everything one pass needs.
pub struct BodyIndexRun<'a> {
    pub cache: &'a Cache,
    /// Which account. Only the id is used; kept as the config so the caller's shape matches
    /// [`crate::worker::OutboxRun`].
    pub cfg: &'a crate::model::AccountConfig,
    /// How many messages this pass may fetch.
    pub limit: usize,
}

/// Where one pass gets message bodies from.
///
/// A trait rather than a closure because a closure that borrows the connection and returns a future
/// cannot satisfy [`FnMut`]: the returned future has to outlive a lifetime the caller chose, and
/// there is no way to write that bound with a plain type parameter. An `async fn` in a trait says it
/// exactly, and it lets a test supply a source that fails on demand — which is how the "one message
/// is unreadable" case is covered.
#[allow(async_fn_in_trait)]
pub trait BodySource {
    async fn fetch_body(&mut self, folder: &str, uid: u32) -> Result<MessageDetail>;

    /// Should this pass give way and stop?
    ///
    /// A source that owns a **shared** connection answers it from the pool's contention count
    /// ([`crate::pool::ConnectionPool::busy_count`]); the default is `false`, because a caller that
    /// owns its connection exclusively — a test, a one-off client — has nothing to yield to.
    ///
    /// It is a sync method on purpose: the pass asks between batches, and an `await` there would be
    /// another place where a background task can hold the connection after the user arrived.
    fn should_yield(&self) -> bool {
        false
    }

    /// Fetch several bodies from **one folder** in as few round trips as the source can manage.
    ///
    /// The return is one entry per requested UID, attributed by the UID the server echoed rather than
    /// by the order of the response — a real server may answer a set in any order, and the pass stores
    /// by `(folder, uid)`, so a response matched by position would put one message's body behind
    /// another message's key. Entries are therefore `(uid, Result<…>)`: a message the server skipped
    /// is an `Err` entry, still pending, and the rest of the batch is kept.
    ///
    /// The outer `Result` is for **the batch as a whole** — a server that rejects a multi-UID set, or
    /// a connection that died. The caller then falls back to `fetch_body` per message, so a server
    /// with an opinion about set sizes costs round trips rather than mail.
    ///
    /// The default implementation is the per-message loop: it exists so a test source (and any future
    /// non-IMAP one, like the Gmail API) does not have to implement batching to be a valid source.
    async fn fetch_bodies(
        &mut self,
        folder: &str,
        uids: &[u32],
    ) -> Result<Vec<(u32, Result<MessageDetail>)>> {
        let mut out = Vec::with_capacity(uids.len());
        for uid in uids {
            out.push((*uid, self.fetch_body(folder, *uid).await));
        }
        Ok(out)
    }
}

impl BodySource for crate::imap::ImapClient {
    async fn fetch_body(&mut self, folder: &str, uid: u32) -> Result<MessageDetail> {
        // `UID FETCH` is only meaningful **inside a selection**, and a UID identifies a different
        // message in every folder. Until 2026-10-20 this line did not exist — the folder argument was
        // ignored on the assumption that "the caller already selected it", which the background pass
        // never does. It therefore read UIDs out of whichever folder the pooled connection happened
        // to be sitting in, and `put_detail` stored that body under the pending row's folder: either
        // a hard failure (nothing indexed, and the log line only prints when it indexed something) or
        // the wrong message's body behind the right key.
        self.use_mailbox(folder).await?;
        self.fetch_message(uid).await
    }

    /// One `UID FETCH a,b,c (…)` for a whole batch: the round trip is what this pass pays for, and
    /// [`crate::imap`] measures 98 % of its cost in nothing else.
    async fn fetch_bodies(
        &mut self,
        folder: &str,
        uids: &[u32],
    ) -> Result<Vec<(u32, Result<MessageDetail>)>> {
        self.use_mailbox(folder).await?;
        let mut fetched = self.fetch_messages(uids).await?;
        if fetched.is_empty() && uids.len() > 1 {
            // **A set that answers nothing is a refusal, not a mailbox full of unreadable mail.**
            // async-imap 0.12 ends a fetch stream at its tagged response and drops the status on the
            // floor — `parse::filter_sync` keeps reading until `Response::Done` carries *its* tag, and
            // a `BAD` in that Done never reaches the caller (measured 2026-10-20: a refused batch and
            // a successful one both surface as an empty stream). So this is the only signal there is,
            // and a refusal is about the command: hand it up as a command-level error and let the
            // caller retry the same UIDs one at a time.
            return Err(crate::error::MailError::Imap(format!(
                "批量 FETCH（{} 封）没有任何响应 —— 按整批失败处理，逐封重试",
                uids.len()
            )));
        }
        Ok(uids
            .iter()
            .map(|uid| {
                let one = fetched.remove(uid).unwrap_or_else(|| {
                    Err(crate::error::MailError::Imap(format!(
                        "服务器没有返回 UID {uid} 的内容"
                    )))
                });
                (*uid, one)
            })
            .collect())
    }
}

/// Wraps a source for a pass the **user asked for by name**, so it never steps aside.
///
/// [`BodySource::should_yield`] exists to keep background work from competing with the interactive
/// path. A pass started by the search box's button *is* the interactive path: yielding because a sync
/// happens to hold a connection would make the button appear broken, which is worse than being
/// slightly impolite for one round trip.
pub struct OnDemand<'a, S>(pub &'a mut S);

#[allow(async_fn_in_trait)]
impl<S: BodySource> BodySource for OnDemand<'_, S> {
    async fn fetch_body(&mut self, folder: &str, uid: u32) -> Result<MessageDetail> {
        self.0.fetch_body(folder, uid).await
    }

    async fn fetch_bodies(
        &mut self,
        folder: &str,
        uids: &[u32],
    ) -> Result<Vec<(u32, Result<MessageDetail>)>> {
        self.0.fetch_bodies(folder, uids).await
    }

    fn should_yield(&self) -> bool {
        false
    }
}

/// Fetch bodies for up to `limit` pending messages and store them.
///
/// `source` lives for the whole pass — in the desktop shell, the pooled IMAP connection. A failure on
/// one message is recorded and the pass continues: stopping the whole pass because the 7th message is
/// malformed would leave the 8th through 20th un-indexed for no reason, and the failures are exactly
/// what the next pass retries.
pub async fn index_bodies_pass(
    run: BodyIndexRun<'_>,
    source: &mut impl BodySource,
) -> Result<IndexReport> {
    let pending = run.cache.bodies_pending(&run.cfg.id, run.limit)?;
    let picked = pick_pending(pending, run.limit);

    let mut report = IndexReport::default();
    // **Folder by folder.** `UID FETCH` is scoped to a selection (see the `BodySource` impl above) and
    // `bodies_pending` mixes the account's folders, so working in the raw order would pay a `SELECT`
    // every time the folder alternates. Grouping happens *after* `pick_pending` has decided which
    // messages this pass takes, so it cannot change what gets indexed — only how few selections it
    // costs. The newest-first order still holds inside each group.
    let mut folders: Vec<&str> = Vec::new();
    for item in &picked {
        if !folders.contains(&item.folder.as_str()) {
            folders.push(item.folder.as_str());
        }
    }

    for &folder in &folders {
        let group: Vec<&PendingBody> = picked
            .iter()
            .filter(|item| item.folder.as_str() == folder)
            .collect();

        // Two kinds of pending work. A body that is already stored only needs its index row
        // rebuilt — one write transaction, no connection — and that is the upgrade path for mail
        // indexed before the current shape existed. Those go first: they are free, and a pass that
        // is mostly re-indexing should not open a connection at all.
        //
        // In [`crate::cache::BodyStorage::OnlineOnly`] the second kind does not exist: fetching a
        // body that may not be stored would cost the whole round trip for nothing, and it would do it
        // for every message in the account, forever, because the pending list can never empty. The
        // local half still runs — it is the only work that is still possible.
        let store_bodies = matches!(run.cache.body_storage(), crate::cache::BodyStorage::Cached);
        let mut remote: Vec<&PendingBody> = Vec::new();
        for item in &group {
            if item.has_body {
                match run.cache.reindex_row(&run.cfg.id, &item.folder, item.uid) {
                    Ok(()) => report.indexed += 1,
                    Err(_) => report.failed += 1,
                }
            } else if store_bodies {
                remote.push(item);
            }
        }
        if remote.is_empty() {
            continue;
        }

        // The expensive half, in as few commands as the byte ceiling allows. A whole batch failing at
        // once is the server saying something about the set rather than about the mail, so it costs a
        // retry one by one — never a hole in the index.
        for uids in plan_batches(&remote) {
            // Asked again for every batch, not once per pass: with the pass limit now a whole
            // command's worth of mail, "the user arrived halfway through" is the common case, and the
            // polite thing is to stop *this* pass rather than finish 50 fetches they are waiting behind.
            if source.should_yield() {
                report.yielded = true;
                break;
            }
            let results = match source.fetch_bodies(folder, &uids).await {
                Ok(results) => results,
                Err(_) => {
                    let mut one_by_one = Vec::with_capacity(uids.len());
                    for item in remote.iter().filter(|i| uids.contains(&i.uid)) {
                        one_by_one.push((item.uid, source.fetch_body(folder, item.uid).await));
                    }
                    one_by_one
                }
            };
            for (_uid, outcome) in results {
                match outcome {
                    Ok(detail) => {
                        run.cache.put_detail(&run.cfg.id, folder, &detail)?;
                        report.indexed += 1;
                    }
                    Err(_) => {
                        // Counted per message, not per batch: the one the server omitted is still
                        // pending and the next pass retries exactly it.
                        report.failed += 1;
                    }
                }
            }
        }
        if report.yielded {
            // Not this folder only: the next folder would pay another `SELECT` for the same user.
            break;
        }
    }
    // Asked rather than computed: `failed` messages are still pending, and so is anything that
    // arrived between the two queries. Reporting the smaller number would be a lie the UI repeats.
    report.remaining = run.cache.bodies_pending_count(&run.cfg.id)?;
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pending(count: u32) -> Vec<PendingBody> {
        (0..count)
            .map(|i| PendingBody {
                folder: "INBOX".to_string(),
                uid: 100 - i,
                has_body: false,
                size: Some(1024),
            })
            .collect()
    }

    #[test]
    fn a_pass_never_takes_more_than_the_limit() {
        assert_eq!(pick_pending(pending(5), 2).len(), 2);
        assert_eq!(pick_pending(pending(2), 5).len(), 2);
    }

    #[test]
    fn pending_order_is_preserved() {
        // Newest first is what the caller handed over; reordering here would index the mail the
        // user is least likely to open first, and the test would not notice.
        let picked = pick_pending(pending(3), 3);
        assert_eq!(
            picked.iter().map(|p| p.uid).collect::<Vec<_>>(),
            vec![100, 99, 98]
        );
    }

    #[test]
    fn an_empty_pending_list_is_not_an_error() {
        assert!(pick_pending(Vec::new(), 20).is_empty());
    }

    // ---------------------------------------------------------- the two kinds of pending work

    fn summary(
        uid: u32,
        subject: &str,
        from_addr: &str,
        date: i64,
    ) -> crate::model::MessageSummary {
        crate::model::MessageSummary {
            uid,
            subject: subject.to_string(),
            from_name: Some("发件人".to_string()),
            from_addr: Some(from_addr.to_string()),
            date: Some(date),
            size: Some(1024),
            seen: false,
            flagged: false,
            thread_key: None,
            subject_key: None,
            to_addrs: Vec::new(),
        }
    }

    fn detail(uid: u32, subject: &str, body_text: &str) -> MessageDetail {
        MessageDetail {
            uid,
            subject: subject.to_string(),
            from_name: Some("发件人".to_string()),
            from_addr: Some("shop@example.com".to_string()),
            to: vec!["me@example.com".to_string()],
            cc: Vec::new(),
            reply_to: None,
            date: Some(1_700_000_000),
            message_id: Some("m1@example.com".to_string()),
            in_reply_to: None,
            references: Vec::new(),
            body_text: Some(body_text.to_string()),
            body_html: Some("<p>html</p>".to_string()),
            attachments: Vec::new(),
            inline_images: Vec::new(),
        }
    }

    fn test_config() -> crate::model::AccountConfig {
        crate::model::AccountConfig {
            id: "acc".to_string(),
            email: "me@example.com".to_string(),
            display_name: "我".to_string(),
            username: "me@example.com".to_string(),
            provider_id: "custom".to_string(),
            imap: crate::model::Endpoint::new("127.0.0.1", 143, crate::model::Security::Plain),
            smtp: crate::model::Endpoint::new("127.0.0.1", 25, crate::model::Security::Plain),
            needs_id_command: false,
            max_connections: 2,
            auth: crate::model::AuthMethod::Password,
            smtp_none: false,
        }
    }

    /// A source that hands over one fixed message and counts the calls, so the two branches of the
    /// pass can be told apart without a server: what matters is not *what* it would have returned,
    /// but that it is never asked for a row we already hold.
    struct OneMessage {
        detail: MessageDetail,
        fetched: std::cell::Cell<u32>,
    }

    impl BodySource for OneMessage {
        async fn fetch_body(&mut self, _folder: &str, _uid: u32) -> Result<MessageDetail> {
            self.fetched.set(self.fetched.get() + 1);
            Ok(self.detail.clone())
        }
    }

    #[tokio::test]
    async fn a_row_indexed_before_attachment_names_existed_is_reindexed_locally_and_never_fetched()
    {
        // The upgrade path, end to end through the pass: the body is in the cache, only its index
        // row is behind. Fetching it again would spend a connection (and quota) on data already
        // stored — on a big mailbox that is the difference between an upgrade and a re-download.
        let cache = crate::cache::Cache::open_in_memory().unwrap();
        cache
            .upsert_summaries(
                "acc",
                "INBOX",
                &[summary(1, "发票", "shop@example.com", 100)],
            )
            .unwrap();
        cache
            .put_detail("acc", "INBOX", &detail(1, "发票", "正文"))
            .unwrap();
        cache
            .force_index_version("acc", "INBOX", 1, 0)
            .expect("rewind the row to the pre-attachment shape");

        let run = BodyIndexRun {
            cache: &cache,
            cfg: &test_config(),
            limit: 20,
        };
        let mut source = OneMessage {
            // Whatever the source would have produced must be irrelevant: it must not be called.
            detail: detail(1, "不该被取", "不该被取"),
            fetched: std::cell::Cell::new(0),
        };
        let report = index_bodies_pass(run, &mut source).await.expect("pass");

        assert_eq!(report.indexed, 1);
        assert_eq!(report.failed, 0);
        assert_eq!(
            source.fetched.get(),
            0,
            "a stale row is already stored — spending a connection on it is the bug"
        );
        assert_eq!(report.remaining, 0);
        assert_eq!(cache.bodies_pending_count("acc").unwrap(), 0);
    }

    /// 正文不落盘 mode, seen from the background: a body that may not be stored must not be fetched.
    ///
    /// The pass has no reason to ask in this mode — it could not keep the answer — and the cost of
    /// asking is not small: every message in the account stays permanently pending, so a pass that
    /// ignored the flag would re-download the mailbox once a minute, forever, and pay for a body it
    /// then threw away.
    #[tokio::test]
    async fn the_background_pass_fetches_nothing_when_bodies_may_not_be_stored() {
        let cache = crate::cache::Cache::open_in_memory().unwrap();
        let summaries: Vec<crate::model::MessageSummary> = (1..=3u32)
            .map(|uid| {
                summary(
                    uid,
                    &format!("主题 {uid}"),
                    "shop@example.com",
                    100 + i64::from(uid),
                )
            })
            .collect();
        cache.upsert_summaries("acc", "INBOX", &summaries).unwrap();
        cache.set_body_storage(crate::cache::BodyStorage::OnlineOnly);

        let run = BodyIndexRun {
            cache: &cache,
            cfg: &test_config(),
            limit: 20,
        };
        let mut source = OneMessage {
            detail: detail(1, "不该被取", "正文"),
            fetched: std::cell::Cell::new(0),
        };
        let report = index_bodies_pass(run, &mut source).await.expect("pass");

        assert_eq!(
            source.fetched.get(),
            0,
            "the mode says bodies may not be stored; a fetch is pure loss"
        );
        assert_eq!(report.indexed, 0);
        // Honest, not optimistic: the bodies really are missing, so `remaining` says so. The next
        // assertion is what makes that number safe — a report that indexed nothing must be paced as
        // idle, or the mode turns into a retry storm.
        assert_eq!(report.remaining, 3);
        assert_eq!(
            backfill_wait(
                &BackfillCadence::default(),
                &report,
                std::time::Duration::from_millis(5)
            ),
            BackfillWait::Idle(BACKFILL_QUIET),
            "no work done ⇒ wait the quiet length, not the duty-cycle gap"
        );
    }

    #[tokio::test]
    async fn a_message_with_no_body_is_fetched_and_then_indexed() {
        // The other branch, and the reason the pass is bounded at all: no body stored means the
        // message has to be opened, which costs what opening costs.
        let cache = crate::cache::Cache::open_in_memory().unwrap();
        cache
            .upsert_summaries(
                "acc",
                "INBOX",
                &[summary(1, "发票", "shop@example.com", 100)],
            )
            .unwrap();

        let run = BodyIndexRun {
            cache: &cache,
            cfg: &test_config(),
            limit: 20,
        };
        let mut source = OneMessage {
            detail: detail(1, "发票", "正文里的词"),
            fetched: std::cell::Cell::new(0),
        };
        let report = index_bodies_pass(run, &mut source).await.expect("pass");

        assert_eq!(report.indexed, 1);
        assert_eq!(source.fetched.get(), 1, "a missing body has to be fetched");
        assert_eq!(report.remaining, 0);
        assert_eq!(cache.search("acc", "正文里的词", 5).unwrap().len(), 1);
    }

    // ------------------------------------------------------- batching: how many commands a pass pays

    fn refs(items: &[PendingBody]) -> Vec<&PendingBody> {
        items.iter().collect()
    }

    fn sized(uids: &[u32], size: Option<u32>) -> Vec<PendingBody> {
        uids.iter()
            .map(|uid| PendingBody {
                folder: "INBOX".to_string(),
                uid: *uid,
                has_body: false,
                size,
            })
            .collect()
    }

    #[test]
    fn small_messages_travel_in_one_batch() {
        let items = sized(&[9, 8, 7, 6], Some(1024));
        assert_eq!(plan_batches(&refs(&items)), vec![vec![9, 8, 7, 6]]);
    }

    #[test]
    fn a_batch_stops_at_the_byte_ceiling_not_at_the_count() {
        // 3 MB each: two fit under 8 MB, three would not. The ceiling is about the bytes the server
        // will put on the wire, because that — not the number of UIDs — is what the process holds.
        let items = sized(&[5, 4, 3, 2, 1], Some(3 * 1024 * 1024));
        assert_eq!(
            plan_batches(&refs(&items)),
            vec![vec![5, 4], vec![3, 2], vec![1]]
        );
    }

    #[test]
    fn a_message_bigger_than_the_ceiling_is_a_batch_of_one_and_is_never_dropped() {
        let items = sized(&[3, 2, 1], Some(20 * 1024 * 1024));
        assert_eq!(plan_batches(&refs(&items)), vec![vec![3], vec![2], vec![1]]);
    }

    #[test]
    fn an_unknown_size_is_fetched_alone() {
        // A row written before the header sync recorded `RFC822.SIZE`: batching it with neighbours
        // would be a guess about memory, so it keeps the cost it had before batching existed.
        let mut items = sized(&[2, 1], Some(1024));
        items[0].size = None;
        assert_eq!(plan_batches(&refs(&items)), vec![vec![2], vec![1]]);
    }

    #[test]
    fn the_batch_count_is_capped_even_when_the_bytes_would_allow_more() {
        let many: Vec<u32> = (0..120).map(|i| 200 - i).collect();
        let items = sized(&many, Some(1));
        let batches = plan_batches(&refs(&items));
        assert_eq!(batches.len(), 3);
        assert!(batches.iter().all(|b| b.len() <= MAX_BATCH_MESSAGES));
        // Nothing is reordered or lost: the union of the batches is the input, in order.
        assert_eq!(
            batches.concat(),
            many,
            "flattening the batches must give back exactly what was asked for"
        );
    }

    #[test]
    fn the_duty_gap_is_a_share_of_the_connection_not_a_frequency() {
        // 250 ‰ means the gap is three times the pass: pass ÷ (pass + gap) = 1/4.
        assert_eq!(
            duty_gap(Duration::from_millis(150), 250),
            Duration::from_millis(450)
        );
        assert_eq!(
            duty_gap(Duration::from_millis(400), 250),
            Duration::from_millis(1_200)
        );
        // The absolute wait shrinks on a fast link while the *share* stays put — which is the whole
        // reason the policy is written as a duty cycle instead of a tick.
        assert!(
            duty_gap(Duration::from_millis(10), 250) < duty_gap(Duration::from_millis(150), 250)
        );
    }

    #[test]
    fn the_budget_clears_on_every_link_latency_that_has_been_measured() {
        // P8b's budget is 20 封/秒, and this is the arithmetic that chose `BACKFILL_DUTY_PERMILLE`.
        // A pass is `DEFAULT_PASS_LIMIT` messages ≈ one round trip, plus the app's own work at the
        // 0.5 ms/封 measured in §16.3 (rounded up to 1 ms so the assertion is not flattering itself).
        for rtt_ms in [150u64, 400, 500] {
            let pass = Duration::from_millis(rtt_ms + DEFAULT_PASS_LIMIT as u64);
            let gap = duty_gap(pass, BACKFILL_DUTY_PERMILLE);
            let per_second = DEFAULT_PASS_LIMIT as f64 / (pass + gap).as_secs_f64();
            assert!(
                per_second >= 20.0,
                "at {rtt_ms} ms round trip the backfill does {per_second:.1} 封/秒, under the 20 封/秒 budget"
            );
        }
    }

    #[test]
    fn the_old_fifteen_percent_politeness_cannot_meet_the_budget_on_a_slow_link() {
        // The counterfactual, kept as a test so the constant is not quietly tuned back down.
        // 15 % was the number `reports/performance.md` §16.4 worked out when it was only about the
        // connection's duty; on a 400 ms link it lands under the 20 封/秒 budget.
        let pass = Duration::from_millis(450);
        let gap = duty_gap(pass, 150);
        let per_second = DEFAULT_PASS_LIMIT as f64 / (pass + gap).as_secs_f64();
        assert!(
            per_second < 20.0,
            "15 % duty was supposed to miss the budget at 400 ms, got {per_second:.1} 封/秒"
        );
    }

    #[test]
    fn an_empty_queue_waits_the_quiet_tick_and_work_waits_the_duty_gap() {
        let cadence = BackfillCadence::default();
        let drained = IndexReport {
            indexed: 50,
            failed: 0,
            remaining: 0,
            yielded: false,
        };
        assert_eq!(
            backfill_wait(&cadence, &drained, Duration::from_millis(150)),
            BackfillWait::Idle(Duration::from_secs(60))
        );

        let more = IndexReport {
            remaining: 4_000,
            ..drained
        };
        assert_eq!(
            backfill_wait(&cadence, &more, Duration::from_millis(150)),
            BackfillWait::Pace(Duration::from_millis(450))
        );
    }

    #[test]
    fn a_pass_that_indexed_nothing_waits_the_quiet_tick_instead_of_retrying_at_full_duty() {
        // The storm this rule closes: a *failing* pass is a fast one, so a duty-cycle gap measured from
        // 5 ms says "again in 15 ms" — forever, on an account whose mail nobody can read.
        let cadence = BackfillCadence::default();
        let stuck = IndexReport {
            indexed: 0,
            failed: 50,
            remaining: 4_000,
            yielded: false,
        };
        assert_eq!(
            backfill_wait(&cadence, &stuck, Duration::from_millis(5)),
            BackfillWait::Idle(cadence.quiet)
        );
    }

    #[test]
    fn giving_way_rechecks_rather_than_pacing_from_a_pass_that_barely_ran() {
        // The trap this exists to avoid: a pass that stopped after one `SELECT` has an `elapsed` near
        // zero, so pacing off it would schedule the next pass almost immediately — hammering the exact
        // moment the user is working. `yielded` has to outrank both `remaining` and the arithmetic.
        let cadence = BackfillCadence::default();
        let report = IndexReport {
            indexed: 0,
            remaining: 4_000,
            yielded: true,
            ..IndexReport::default()
        };
        assert_eq!(
            backfill_wait(&cadence, &report, Duration::from_millis(2)),
            BackfillWait::Recheck(cadence.recheck)
        );
        // Even when giving way happened to drain the queue: the re-check is harmless, the 60 s silence
        // would hide a sync that arrives a second later.
        let yielded_empty = IndexReport {
            remaining: 0,
            ..report
        };
        assert_eq!(
            backfill_wait(&cadence, &yielded_empty, Duration::from_millis(2)),
            BackfillWait::Recheck(cadence.recheck)
        );
    }

    #[test]
    fn a_pass_that_overruns_the_whole_tick_never_silences_the_backfill() {
        let cadence = BackfillCadence::default();
        let report = IndexReport {
            indexed: 50,
            remaining: 4_000,
            ..IndexReport::default()
        };
        // A VPN reconnecting made the pass take five minutes. The gap must not become fifteen.
        assert_eq!(
            backfill_wait(&cadence, &report, Duration::from_secs(300)),
            BackfillWait::Pace(cadence.quiet)
        );
    }

    #[tokio::test]
    async fn a_source_that_says_the_user_arrived_stops_before_fetching_anything() {
        let cache = crate::cache::Cache::open_in_memory().unwrap();
        seed(&cache, "INBOX", &[5, 4, 3, 2, 1]);
        let run = BodyIndexRun {
            cache: &cache,
            cfg: &test_config(),
            limit: 20,
        };
        let mut inner = Batches {
            per_folder: std::cell::RefCell::new(Vec::new()),
            per_message: std::cell::Cell::new(0),
            omit: None,
            reject_sets: false,
        };
        let report = {
            let mut source = Yielding {
                inner: &mut inner,
                batches: std::cell::Cell::new(0),
                after: 0,
            };
            index_bodies_pass(run, &mut source).await.expect("pass")
        };

        assert!(report.yielded, "the pass must say it gave way");
        assert_eq!(report.indexed, 0);
        assert!(
            inner.per_folder.borrow().is_empty(),
            "giving way means not asking: {:?}",
            inner.per_folder.borrow()
        );
        assert_eq!(report.remaining, 5, "and nothing is lost by doing so");
    }

    #[tokio::test]
    async fn a_pass_that_gives_way_mid_flight_keeps_what_it_already_indexed() {
        // The reason the question is asked per batch rather than per pass: with the limit now a whole
        // command's worth of mail, "the user arrived halfway through" is the ordinary case.
        let cache = crate::cache::Cache::open_in_memory().unwrap();
        let uids: Vec<u32> = (1..=120u32).collect();
        seed(&cache, "INBOX", &uids);
        let run = BodyIndexRun {
            cache: &cache,
            cfg: &test_config(),
            // Three commands' worth, so there is a "halfway through" to stop at.
            limit: 120,
        };
        let mut inner = Batches {
            per_folder: std::cell::RefCell::new(Vec::new()),
            per_message: std::cell::Cell::new(0),
            omit: None,
            reject_sets: false,
        };
        let report = {
            let mut source = Yielding {
                inner: &mut inner,
                batches: std::cell::Cell::new(0),
                after: 1,
            };
            index_bodies_pass(run, &mut source).await.expect("pass")
        };

        assert!(report.yielded);
        assert_eq!(
            report.indexed, MAX_BATCH_MESSAGES as u32,
            "the batch it managed to fetch must be counted"
        );
        assert_eq!(
            inner.per_folder.borrow().len(),
            1,
            "and the second command must not have been sent"
        );
        assert_eq!(report.remaining, 70, "120 pending minus the 50 indexed");
    }

    /// Wraps a source and answers "the user is here" once `after` batches have gone through.
    struct Yielding<'a, S> {
        inner: &'a mut S,
        /// Counted here, not in the inner source: this is the number of times the *pass* asked, which
        /// is what `should_yield` is allowed to know.
        batches: std::cell::Cell<u32>,
        after: u32,
    }

    #[allow(async_fn_in_trait)]
    impl<S: BodySource> BodySource for Yielding<'_, S> {
        async fn fetch_body(&mut self, folder: &str, uid: u32) -> Result<MessageDetail> {
            self.inner.fetch_body(folder, uid).await
        }

        async fn fetch_bodies(
            &mut self,
            folder: &str,
            uids: &[u32],
        ) -> Result<Vec<(u32, Result<MessageDetail>)>> {
            let n = self.batches.get();
            self.batches.set(n + 1);
            self.inner.fetch_bodies(folder, uids).await
        }

        fn should_yield(&self) -> bool {
            self.batches.get() >= self.after
        }
    }

    /// A source that batches like the IMAP one and records every command it was asked to make.
    struct Batches {
        per_folder: std::cell::RefCell<Vec<(String, Vec<u32>)>>,
        per_message: std::cell::Cell<u32>,
        /// `Some(n)` answers only the first n of the set and omits the rest, like a server that
        /// trimmed a message it could not read.
        omit: Option<usize>,
        reject_sets: bool,
    }

    impl BodySource for Batches {
        async fn fetch_body(&mut self, _folder: &str, uid: u32) -> Result<MessageDetail> {
            self.per_message.set(self.per_message.get() + 1);
            Ok(detail(uid, "主题", "正文里的词"))
        }

        async fn fetch_bodies(
            &mut self,
            folder: &str,
            uids: &[u32],
        ) -> Result<Vec<(u32, Result<MessageDetail>)>> {
            self.per_folder
                .borrow_mut()
                .push((folder.to_string(), uids.to_vec()));
            if self.reject_sets && uids.len() > 1 {
                return Err(crate::error::MailError::Imap(
                    "this server refuses multi-UID sets".into(),
                ));
            }
            let keep = self.omit.unwrap_or(uids.len());
            Ok(uids
                .iter()
                .enumerate()
                .map(|(i, uid)| {
                    (
                        *uid,
                        if i < keep {
                            Ok(detail(*uid, "主题", "正文里的词"))
                        } else {
                            Err(crate::error::MailError::Imap(format!(
                                "服务器没有返回 UID {uid} 的内容"
                            )))
                        },
                    )
                })
                .collect())
        }
    }

    fn seed(cache: &crate::cache::Cache, folder: &str, uids: &[u32]) {
        let summaries: Vec<crate::model::MessageSummary> = uids
            .iter()
            .enumerate()
            .map(|(i, uid)| {
                summary(
                    *uid,
                    &format!("主题 {uid}"),
                    "shop@example.com",
                    100 - i as i64,
                )
            })
            .collect();
        cache.upsert_summaries("acc", folder, &summaries).unwrap();
    }

    #[tokio::test]
    async fn a_pass_pays_one_command_per_folder_not_one_per_message() {
        // The whole point of 2b: `reports/performance.md` §16.3 measured 98 % of this pass as round
        // trips, so the only thing that moves the number is how many of them a pass makes.
        let cache = crate::cache::Cache::open_in_memory().unwrap();
        seed(&cache, "INBOX", &[9, 8, 7, 6]);
        seed(&cache, "已发送", &[5, 4]);

        let run = BodyIndexRun {
            cache: &cache,
            cfg: &test_config(),
            limit: 20,
        };
        let mut source = Batches {
            per_folder: std::cell::RefCell::new(Vec::new()),
            per_message: std::cell::Cell::new(0),
            omit: None,
            reject_sets: false,
        };
        let report = index_bodies_pass(run, &mut source).await.expect("pass");

        let asked = source.per_folder.borrow().clone();
        assert_eq!(
            asked.len(),
            2,
            "two folders with work is two commands, not six — and never one mixed set"
        );
        let folder_of: std::collections::HashMap<u32, &str> = [
            (9, "INBOX"),
            (8, "INBOX"),
            (7, "INBOX"),
            (6, "INBOX"),
            (5, "已发送"),
            (4, "已发送"),
        ]
        .into_iter()
        .collect();
        for (folder, uids) in &asked {
            assert!(
                uids.iter().all(|u| folder_of[u] == folder.as_str()),
                "a batch must not contain a uid belonging to another folder: {folder} ← {uids:?}"
            );
        }
        assert_eq!(
            source.per_message.get(),
            0,
            "the batched path must not also fetch singly"
        );
        assert_eq!(report.indexed, 6);
        assert_eq!(report.failed, 0);
        assert_eq!(report.remaining, 0);
    }

    #[tokio::test]
    async fn a_server_that_refuses_a_set_still_gets_every_message() {
        // The fallback is the reason batching is allowed to exist at all: a server that answers BAD to
        // a multi-UID set must cost the user round trips, not mail.
        let cache = crate::cache::Cache::open_in_memory().unwrap();
        seed(&cache, "INBOX", &[9, 8, 7, 6]);

        let run = BodyIndexRun {
            cache: &cache,
            cfg: &test_config(),
            limit: 20,
        };
        let mut source = Batches {
            per_folder: std::cell::RefCell::new(Vec::new()),
            per_message: std::cell::Cell::new(0),
            omit: None,
            reject_sets: true,
        };
        let report = index_bodies_pass(run, &mut source).await.expect("pass");

        assert_eq!(
            source.per_message.get(),
            4,
            "each refused message retried once"
        );
        assert_eq!(report.indexed, 4);
        assert_eq!(report.failed, 0);
        assert_eq!(report.remaining, 0);
    }

    #[tokio::test]
    async fn a_message_the_server_omitted_is_one_failure_not_a_lost_batch() {
        let cache = crate::cache::Cache::open_in_memory().unwrap();
        seed(&cache, "INBOX", &[9, 8, 7, 6]);

        let run = BodyIndexRun {
            cache: &cache,
            cfg: &test_config(),
            limit: 20,
        };
        let mut source = Batches {
            per_folder: std::cell::RefCell::new(Vec::new()),
            per_message: std::cell::Cell::new(0),
            omit: Some(3),
            reject_sets: false,
        };
        let report = index_bodies_pass(run, &mut source).await.expect("pass");

        assert_eq!(report.indexed, 3);
        assert_eq!(
            report.failed, 1,
            "the omitted uid is a failure of that message only"
        );
        assert_eq!(
            report.remaining, 1,
            "and it is still pending, so the next pass retries exactly it"
        );
        let pending = cache.bodies_pending("acc", 20).unwrap();
        assert_eq!(pending.len(), 1);
    }
}
