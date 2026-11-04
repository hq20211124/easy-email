//! The daily maintenance tick (`design/16` §4.2).
//!
//! **Why this file exists:** §4.2 asks for heal / FTS consistency / vacuum / expired-row cleanup to
//! run on their own, every 24 h or when idle. None of it was ever implemented — the pieces were, and
//! the button in the UI drove them, but nothing ran unless the user pressed something. The visible
//! cost is the one the design predicted: an index that drifted stays drifted (search silently misses
//! mail), and freed pages never come back (the cache only grows).
//!
//! **What it deliberately does not do:**
//!
//! * **It does not delete bodies.** `Cache::heal_index` removes orphaned body rows, and until the
//!   quarantine from `design/16` §5 exists (issue #7) that is a destructive action nobody approved.
//!   The tick therefore calls [`Cache::reindex_step`], which repairs the index and only *reports* the
//!   orphans. The report is what makes them findable; the deletion stays behind the button.
//! * **It does not apply a cleanup plan.** `design/16` §4.2 says "quota::plan + apply when necessary",
//!   but the app promises that nothing deletes the user's data on its own (`README`: 「没有任何东西
//!   会自己删你的数据」). An unattended delete would break that promise to save disk space nobody
//!   asked to save. The plan is a click away in 设置 → 存储与清理.
//! * **It does not prune "expired rows".** §4.2 names `sync_jobs` / `notification_log` /
//!   `import_dedupe`; none of those tables exist in this build, so there is nothing to expire. When
//!   they land, they belong here.
//!
//! What is left is real and bounded: reindex what drifted, and give the freed pages back.

use std::time::Instant;

use crate::cache::Cache;
use crate::error::Result;

/// How many messages one tick is allowed to reindex, per account.
///
/// Bounded for the same reason the background body indexer is: a tick must finish, and a rebuild that
/// does not finish is not lost — `rebuild_search_*` records its progress in the database, so the next
/// tick continues where this one stopped.
pub const REINDEX_STEP: usize = 500;

/// What one account looked like when the tick saw it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AccountMaintenance {
    pub account_id: String,
    /// Messages the cache holds for this account.
    pub messages: u32,
    /// Rows the search index holds for it — the number the health check trusts.
    pub indexed: u32,
    /// Bodies whose message row is gone. Counted, **not** deleted (see the module note).
    pub orphan_bodies: u32,
    /// Messages reindexed by this tick.
    pub reindexed: u32,
    /// Messages still waiting for the next tick.
    pub remaining: u32,
}

impl AccountMaintenance {
    /// Whether this account needed anything at all.
    pub fn touched(&self) -> bool {
        self.reindexed > 0 || self.remaining > 0 || self.orphan_bodies > 0
    }
}

/// One tick's worth of work, in the shape the log line and the UI read.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MaintenanceReport {
    pub accounts: Vec<AccountMaintenance>,
    /// Freelist pages left after the incremental vacuum (0 means the space came back).
    pub freelist_pages: i64,
    pub took_ms: u64,
}

impl MaintenanceReport {
    /// One line for the log, in the words a bug report can carry.
    ///
    /// Written as a sentence rather than as debug output because this is the only trace an unattended
    /// run leaves: `log_line` is what 设置 → 查看日志 and the diagnostic bundle both show.
    pub fn summary(&self) -> String {
        let reindexed: u32 = self.accounts.iter().map(|a| a.reindexed).sum();
        let remaining: u32 = self.accounts.iter().map(|a| a.remaining).sum();
        let orphans: u32 = self.accounts.iter().map(|a| a.orphan_bodies).sum();
        if reindexed == 0 && remaining == 0 && orphans == 0 {
            return format!(
                "维护：{} 个账号，索引与缓存一致，freelist {} 页，{} ms",
                self.accounts.len(),
                self.freelist_pages,
                self.took_ms
            );
        }
        let mut parts = Vec::new();
        if reindexed > 0 {
            parts.push(format!("重建索引 {reindexed} 封"));
        }
        if remaining > 0 {
            parts.push(format!("还剩 {remaining} 封待下一轮"));
        }
        if orphans > 0 {
            parts.push(format!(
                "孤儿正文 {orphans} 封（未删除：等「重建索引」按钮，见 issue #7）"
            ));
        }
        format!(
            "维护：{} 个账号，{}，freelist {} 页，{} ms",
            self.accounts.len(),
            parts.join("，"),
            self.freelist_pages,
            self.took_ms
        )
    }
}

/// Run one tick over the given accounts.
///
/// `account_ids` rather than "every account in the cache": the caller owns the account list, and a
/// cache row left behind by a deleted account is not something this tick should resurrect work for.
pub fn tick(cache: &Cache, account_ids: &[String]) -> Result<MaintenanceReport> {
    let started = Instant::now();
    let mut report = MaintenanceReport::default();

    for account_id in account_ids {
        let health = cache.index_health(account_id)?;
        let mut entry = AccountMaintenance {
            account_id: account_id.clone(),
            messages: health.messages,
            indexed: health.indexed,
            orphan_bodies: health.orphan_bodies,
            ..AccountMaintenance::default()
        };
        // Only when the index is actually behind: `reindex_step` checks the same two signals, but
        // asking here keeps a healthy account from paying for a rebuild-start round trip.
        if !health.fts_ok || health.indexed < health.messages {
            let healed = cache.reindex_step(account_id, REINDEX_STEP)?;
            entry.reindexed = healed.rebuilt;
            entry.remaining = healed.remaining;
        }
        report.accounts.push(entry);
    }

    report.freelist_pages = cache.vacuum_into_freelist()?;
    report.took_ms = started.elapsed().as_millis() as u64;
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache::Cache;
    use crate::model::{MailboxInfo, MessageDetail, MessageSummary};

    fn temp_cache(name: &str) -> Cache {
        let mut path = std::env::temp_dir();
        path.push(format!(
            "ee-maintenance-{name}-{}-{:?}.db",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_file(&path);
        Cache::open(&path).expect("cache opens")
    }

    fn summary(uid: u32, subject: &str) -> MessageSummary {
        MessageSummary {
            uid,
            subject: subject.to_string(),
            from_name: Some("张三".to_string()),
            from_addr: Some("zhang@example.com".to_string()),
            date: Some(1_700_000_000),
            size: Some(1024),
            seen: false,
            flagged: false,
            thread_key: None,
            subject_key: None,
            to_addrs: Vec::new(),
        }
    }

    fn detail(uid: u32, subject: &str) -> MessageDetail {
        MessageDetail {
            uid,
            subject: subject.to_string(),
            from_name: Some("张三".to_string()),
            from_addr: Some("zhang@example.com".to_string()),
            to: Vec::new(),
            cc: Vec::new(),
            reply_to: None,
            date: Some(1_700_000_000),
            message_id: Some(format!("<{uid}@example.com>")),
            in_reply_to: None,
            references: Vec::new(),
            body_text: Some(format!("{subject} 的正文")),
            body_html: None,
            attachments: Vec::new(),
            inline_images: Vec::new(),
        }
    }

    fn seed(cache: &Cache, account: &str) {
        cache
            .upsert_folder(
                account,
                &MailboxInfo {
                    name: "INBOX".to_string(),
                    display_name: "收件箱".to_string(),
                    delimiter: Some("/".to_string()),
                    selectable: true,
                },
                1,
            )
            .expect("folder");
        for uid in 1..=3 {
            cache
                .upsert_summaries(account, "INBOX", &[summary(uid, &format!("主题 {uid}"))])
                .expect("summary");
            cache
                .put_detail(account, "INBOX", &detail(uid, &format!("主题 {uid}")))
                .expect("detail");
        }
    }

    #[test]
    fn a_healthy_cache_is_left_alone_and_says_so() {
        let cache = temp_cache("healthy");
        seed(&cache, "a@example.com");
        let report = tick(&cache, &["a@example.com".to_string()]).expect("tick");
        assert_eq!(report.accounts.len(), 1);
        let account = &report.accounts[0];
        assert_eq!(account.messages, 3);
        assert_eq!(
            account.indexed, 3,
            "every message was indexed on the way in"
        );
        assert_eq!(account.reindexed, 0, "a healthy index must not be rebuilt");
        assert!(!account.touched());
        assert!(
            report.summary().contains("索引与缓存一致"),
            "the log line has to say nothing happened: {}",
            report.summary()
        );
    }

    /// The failure this whole file exists for: a drifted index used to stay drifted forever.
    #[test]
    fn a_drifted_index_is_repaired_by_the_tick() {
        let cache = temp_cache("drifted");
        seed(&cache, "a@example.com");

        // Simulate the damage the health check is meant to catch: a message whose index row is gone.
        // Written as raw SQL because that is the only way to produce the state — every write path
        // keeps the message and its index row in one transaction, which is exactly why the count is
        // a trustworthy signal.
        cache
            .db()
            .write_blocking(|conn| {
                conn.execute(
                    "delete from search where account_id = ?1 and folder = ?2 and uid = ?3",
                    rusqlite::params!["a@example.com", "INBOX", 2],
                )?;
                Ok(())
            })
            .expect("row removed");

        let before = cache.index_health("a@example.com").expect("health");
        assert_eq!(before.indexed, 2, "the index is one row behind");

        let report = tick(&cache, &["a@example.com".to_string()]).expect("tick");
        assert!(report.accounts[0].reindexed > 0, "the tick reindexed");
        assert_eq!(report.accounts[0].remaining, 0, "and it finished");

        let after = cache.index_health("a@example.com").expect("health");
        assert_eq!(after.indexed, 3, "the index caught up");
        assert!(
            report.summary().contains("重建索引"),
            "{}",
            report.summary()
        );
    }

    #[test]
    fn orphan_bodies_are_reported_but_not_deleted() {
        let cache = temp_cache("orphans");
        seed(&cache, "a@example.com");
        // A body with no message row: exactly what `heal_index` deletes and this tick must not.
        cache
            .put_detail("a@example.com", "INBOX", &detail(99, "孤儿"))
            .expect("orphan body");

        let report = tick(&cache, &["a@example.com".to_string()]).expect("tick");
        assert_eq!(report.accounts[0].orphan_bodies, 1);
        assert!(
            report.summary().contains("未删除"),
            "the report must say why it kept them: {}",
            report.summary()
        );
        // Still there — deletion is the user's button, not the timer's.
        let health = cache.index_health("a@example.com").expect("health");
        assert_eq!(health.orphan_bodies, 1, "the tick must not delete bodies");
    }

    #[test]
    fn an_account_with_no_mail_is_not_an_error() {
        let cache = temp_cache("empty");
        let report = tick(&cache, &["nobody@example.com".to_string()]).expect("tick");
        assert_eq!(report.accounts[0].messages, 0);
        assert!(!report.accounts[0].touched());
    }
}
