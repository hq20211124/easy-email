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

/// How much of a body goes into the search index, in characters.
///
/// `design/09` §11's default of 64 KB, counted in characters rather than bytes because the mail it
/// matters for is Chinese — a byte cap would cut the last character of nearly every message.
pub const DEFAULT_BODY_INDEX_CHARS: usize = 64 * 1024;

/// One message that has no body in the cache yet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingBody {
    pub folder: String,
    pub uid: u32,
    /// The body **is** in the cache; only its index row predates the current shape
    /// ([`crate::cache::INDEX_VERSION`]). Such a row is re-indexed locally — `bodies.detail` holds
    /// the whole message, so the pass must not spend a connection (or a quota) fetching it again.
    pub has_body: bool,
}

/// Which messages the next pass should fetch.
///
/// Pure so the cap and the ordering can be pinned without a database: the caller hands over what is
/// pending (newest first, from `Cache::bodies_pending`) and this decides how much of it one pass may
/// take. It must not reorder — reordering would make the newest mail the last to be indexed.
pub fn pick_pending(pending: Vec<PendingBody>, limit: usize) -> Vec<PendingBody> {
    pending.into_iter().take(limit).collect()
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
}

impl BodySource for crate::imap::ImapClient {
    async fn fetch_body(&mut self, _folder: &str, uid: u32) -> Result<MessageDetail> {
        // The folder is not passed through: the client is already selected by the caller, and
        // `fetch_message` fetches from the current selection. Re-selecting per message would cost a
        // round trip the pool then has to pay for again on the next pass.
        self.fetch_message(uid).await
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
    for item in &picked {
        // Two kinds of pending work. A body that is already stored only needs its index row
        // rebuilt — one write transaction, no connection — and that is the upgrade path for mail
        // indexed before the current shape existed. A message with no body at all still has to be
        // fetched, which is the expensive half and the reason the pass is bounded at all.
        if item.has_body {
            match run.cache.reindex_row(&run.cfg.id, &item.folder, item.uid) {
                Ok(()) => report.indexed += 1,
                Err(_) => report.failed += 1,
            }
            continue;
        }
        match source.fetch_body(&item.folder, item.uid).await {
            Ok(detail) => {
                run.cache.put_detail(&run.cfg.id, &item.folder, &detail)?;
                report.indexed += 1;
            }
            Err(_) => {
                report.failed += 1;
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
}
