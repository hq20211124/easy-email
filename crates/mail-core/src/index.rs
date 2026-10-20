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

/// How many messages one pass may fetch.
///
/// Twenty, on a 60 s tick: the per-message cost equals opening the message, so this is deliberately
/// slow and interruptible rather than a burst that starves the user's own refresh.
pub const DEFAULT_PASS_LIMIT: usize = 20;

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
/// A server that dislikes long sequence sets is the reason (`design/19`'s politeness rule), and the
/// per-pass limit is 20 today, so this only ever binds once the tick changes in step 3.
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
        let mut remote: Vec<&PendingBody> = Vec::new();
        for item in &group {
            if item.has_body {
                match run.cache.reindex_row(&run.cfg.id, &item.folder, item.uid) {
                    Ok(()) => report.indexed += 1,
                    Err(_) => report.failed += 1,
                }
            } else {
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
