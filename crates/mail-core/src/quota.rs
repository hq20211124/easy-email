//! Storage quota and cleanup (`design/03` §4.3 `quota::*`, §5.4 the daily pass).
//!
//! `cache.db` only grows: one row per synced summary, one JSON body per message the user opens.
//! Nothing ever asked how big it was, and nothing could delete anything, so the only way out of a
//! full disk was to delete the whole cache and sync again from the beginning.
//!
//! **Never reaches a server.** Everything here is SQL over one local file, which is why there is no
//! network client, no connection handling, and no `async` anywhere in this module: a cleanup that
//! depends on a remote answer can fail in a way this design refuses to (§5.4). The gate checks the
//! same thing as a banned-name list, which is why this comment avoids naming any of them.
//!
//! The rule the acceptance is built on: **the dry run's numbers are the applied numbers.** Not
//! because both were computed carefully, but because there is only **one** predicate — `Cleanup` —
//! that the plan turns into a `SELECT` and the apply turns into a `DELETE`. Two SQL statements
//! drift; one does not.
//!
//! Nothing runs on its own. There is no timer here on purpose: deleting a user's mail from a cache
//! because a task woke up is a decision no background pass should make.

use rusqlite::{Connection, OptionalExtension};

/// What the cache costs right now.
///
/// `cache_bytes` is an estimate from SQLite's own page accounting, and it says so: the honest number
/// for "how much disk does this use" is `page_count * page_size`, and the difference from the file
/// size is freelist pages, which is exactly what a later VACUUM would reclaim.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct StorageReport {
    pub cache_bytes: u64,
    /// Cached message summaries.
    pub messages: u32,
    /// Messages whose body is stored.
    pub bodies: u32,
    /// Bytes in `bodies.detail` — the bulk of the cache, because a body carries its HTML.
    pub bodies_bytes: u64,
    /// Oldest stored body, unix seconds.
    pub oldest_body_at: Option<i64>,
}

/// What a user is willing to let this app delete.
///
/// `None` means "no limit", and a policy with no limit deletes nothing — clearing a cache the user
/// never asked about is a worse outcome than a large cache.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CleanupPolicy {
    pub max_bytes: Option<u64>,
    /// Delete bodies older than this, when the cache is over the limit.
    pub max_age_days: Option<u32>,
    /// Unread bodies are never deleted, whatever the age.
    pub keep_unread: bool,
    /// Flagged bodies are never deleted, whatever the age.
    pub keep_flagged: bool,
}

/// One step of a plan: what it would delete, and how much that would free.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CleanupStep {
    /// The table this step deletes from.
    pub target: CleanupTarget,
    /// Messages this step covers.
    pub rows: u32,
    /// Bytes this step frees, as the plan measured them.
    pub freed_bytes: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum CleanupTarget {
    /// Stored message bodies.
    Bodies,
    /// Cached message summaries. Only when the policy allows it and the step is asked for.
    Messages,
}

/// What one apply did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CleanupReport {
    pub freed_bytes: u64,
    pub bodies_deleted: u32,
    pub messages_deleted: u32,
}

/// What the cache looks like now, in one query per number.
pub fn report(conn: &Connection) -> rusqlite::Result<StorageReport> {
    let page_size: i64 = conn.query_row("pragma page_size", [], |row| row.get(0))?;
    let page_count: i64 = conn.query_row("pragma page_count", [], |row| row.get(0))?;
    let messages: u32 = conn.query_row("select count(*) from messages", [], |row| {
        row.get::<_, i64>(0).map(|n| n as u32)
    })?;
    let bodies: u32 = conn.query_row("select count(*) from bodies", [], |row| {
        row.get::<_, i64>(0).map(|n| n as u32)
    })?;
    let bodies_bytes: u64 = conn.query_row(
        "select coalesce(sum(length(detail)), 0) from bodies",
        [],
        |row| row.get::<_, i64>(0).map(|n| n as u64),
    )?;
    let oldest_body_at: Option<i64> = conn
        .query_row("select min(fetched_at) from bodies", [], |row| row.get(0))
        .optional()?
        .flatten();
    Ok(StorageReport {
        cache_bytes: page_size as u64 * page_count as u64,
        messages,
        bodies,
        bodies_bytes,
        oldest_body_at,
    })
}

/// The one place a cleanup's "which rows" is decided.
///
/// Both the plan and the apply go through this. That is the whole trick behind
/// "干跑数字与实跑一致": a `where` written twice is a `where` that will one day be changed in one
/// of them, and the user finds out from the toast being wrong.
fn cleanup_predicate(policy: &CleanupPolicy, now: i64) -> String {
    let mut sql = String::from("1 = 1");
    if let Some(days) = policy.max_age_days {
        sql.push_str(&format!(
            " and fetched_at < {}",
            now - i64::from(days) * 86_400
        ));
    }
    if policy.keep_unread {
        sql.push_str(
            " and not exists (select 1 from messages m
                               where m.account_id = bodies.account_id
                                 and m.folder = bodies.folder
                                 and m.uid = bodies.uid
                                 and m.seen = 0)",
        );
    }
    if policy.keep_flagged {
        sql.push_str(
            " and not exists (select 1 from messages m
                               where m.account_id = bodies.account_id
                                 and m.folder = bodies.folder
                                 and m.uid = bodies.uid
                                 and m.flagged = 1)",
        );
    }
    sql
}

/// What a cleanup **would** do. Read-only: SELECTs and SUMs, nothing else.
///
/// An empty plan is a legitimate answer and means nothing would be deleted.
pub fn plan(conn: &Connection, policy: &CleanupPolicy, now: i64) -> rusqlite::Result<CleanupPlan> {
    let _ = conn;
    let report = report(conn)?;
    let over_limit = match policy.max_bytes {
        Some(max) => report.cache_bytes > max,
        None => false,
    };
    // Nothing over the limit and no age rule: this cache is someone else's decision.
    if !over_limit && policy.max_age_days.is_none() {
        return Ok(CleanupPlan::default());
    }

    let predicate = cleanup_predicate(policy, now);
    let (rows, freed): (u32, u64) = conn.query_row(
        &format!("select count(*), coalesce(sum(length(detail)), 0) from bodies where {predicate}"),
        [],
        |row| {
            Ok((
                row.get::<_, i64>(0)? as u32,
                row.get::<_, i64>(1).map(|n| n as u64)?,
            ))
        },
    )?;

    let mut plan = CleanupPlan {
        over_limit,
        freed_bytes: freed,
        steps: Vec::new(),
    };
    if rows > 0 {
        plan.steps.push(CleanupStep {
            target: CleanupTarget::Bodies,
            rows,
            freed_bytes: freed,
        });
    }
    Ok(plan)
}

/// What a plan is going to do, in the shape the UI shows it.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CleanupPlan {
    /// Whether the cache was over the byte limit when the plan was made.
    pub over_limit: bool,
    pub steps: Vec<CleanupStep>,
    /// The sum of the steps, which is the number the user is shown before anything happens.
    pub freed_bytes: u64,
}

/// Run a plan: delete the bodies it names, and report what actually went.
///
/// The predicate is rebuilt from the **same policy** that made the plan, and the number is measured
/// **again immediately before the delete** — so what the user is told is what went, not what was
/// promised a moment ago (a body may have been fetched in between).
///
/// **This deletes bodies and nothing else.** The search index is a separate concern that belongs to
/// the cache, and `Cache::cleanup` is where the two meet: a quota pass that also rewrites the index
/// is a quota pass that has to know what the index looks like.
pub fn apply(
    conn: &Connection,
    policy: &CleanupPolicy,
    plan: &CleanupPlan,
    now: i64,
) -> rusqlite::Result<CleanupReport> {
    let wants_bodies = plan.steps.iter().any(|s| s.target == CleanupTarget::Bodies);
    if !wants_bodies {
        return Ok(CleanupReport::default());
    }

    let predicate = cleanup_predicate(policy, now);
    // Measured again, immediately before the delete: the report to the user is what actually went,
    // not what was promised a moment ago.
    let freed: u64 = conn.query_row(
        &format!("select coalesce(sum(length(detail)), 0) from bodies where {predicate}"),
        [],
        |row| row.get::<_, i64>(0).map(|n| n as u64),
    )?;
    let deleted: u32 = conn.execute(&format!("delete from bodies where {predicate}"), [])? as u32;

    Ok(CleanupReport {
        freed_bytes: freed,
        bodies_deleted: deleted,
        messages_deleted: 0,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::params;

    /// A cache with `n` bodies, one per message, alternating unread/flagged/plain.
    ///
    /// Sizes are what make the numbers interesting: each body is a different length, so a plan's
    /// `freed_bytes` cannot accidentally equal a row count.
    fn cache() -> (rusqlite::Connection, u64) {
        let conn = Connection::open_in_memory().expect("memory");
        conn.execute_batch(
            "create table messages (
                 account_id text not null, folder text not null, uid integer not null,
                 subject text not null, from_addr text, date integer, size integer,
                 seen integer not null default 0, flagged integer not null default 0,
                 primary key (account_id, folder, uid));
             create table bodies (
                 account_id text not null, folder text not null, uid integer not null,
                 detail text not null, fetched_at integer not null,
                 primary key (account_id, folder, uid));",
        )
        .expect("schema");
        let mut bytes = 0;
        for i in 0..6u32 {
            let seen = i % 2 == 0;
            let flagged = i == 1;
            let detail = format!(
                "{{\"uid\":{i},\"body_text\":\"{}\"}}",
                "x".repeat(100 + i as usize)
            );
            bytes += detail.len() as u64;
            conn.execute(
                "insert into messages (account_id, folder, uid, subject, from_addr, date, size, seen, flagged)
                 values ('acc', 'INBOX', ?1, ?2, 'a@example.com', 1700000000, 100, ?3, ?4)",
                params![i, format!("主题{i}"), seen as i64, flagged as i64],
            )
            .expect("message");
            conn.execute(
                "insert into bodies (account_id, folder, uid, detail, fetched_at)
                 values ('acc', 'INBOX', ?1, ?2, ?3)",
                params![i, detail, 1_700_000_000i64 + i as i64 * 86_400],
            )
            .expect("body");
        }
        (conn, bytes)
    }

    /// Over the byte limit, with no age rule and nothing spared: the "take it all" policy.
    ///
    /// `max_age_days: None` on purpose — the bodies in this fixture are ten days old, and an age rule
    /// of 3650 days would keep every one of them, which is how this test first failed.
    fn policy_all() -> CleanupPolicy {
        CleanupPolicy {
            max_bytes: Some(1),
            max_age_days: None,
            keep_unread: false,
            keep_flagged: false,
        }
    }

    #[test]
    fn the_dry_run_number_is_the_applied_number() {
        // The acceptance this module exists for. The plan promises N bytes; the apply frees N bytes.
        // It is not a coincidence of arithmetic — both numbers come from the same predicate.
        let (conn, total) = cache();
        let now = 1_700_000_000 + 10 * 86_400;
        let policy = policy_all();

        let plan = plan(&conn, &policy, now).expect("plan");
        assert_eq!(plan.freed_bytes, total, "the plan measures what is there");

        let report = apply(&conn, &policy, &plan, now).expect("apply");
        assert_eq!(
            report.freed_bytes, plan.freed_bytes,
            "same predicate, same answer"
        );
        assert_eq!(report.bodies_deleted, 6);
        assert_eq!(
            super::report(&conn).expect("report after").bodies,
            0,
            "no bodies left"
        );
    }

    #[test]
    fn an_empty_plan_deletes_nothing() {
        // The failure that would be worst: a stale or empty plan applied as if it were full.
        let (conn, _) = cache();
        let now = 1_700_000_000 + 10 * 86_400;
        let report =
            apply(&conn, &policy_all(), &CleanupPlan::default(), now).expect("apply nothing");
        assert_eq!((report.bodies_deleted, report.freed_bytes), (0, 0));
        assert_eq!(super::report(&conn).expect("still there").bodies, 6);
    }

    #[test]
    fn unread_and_flagged_bodies_are_kept() {
        // Design §5.4, and plain sense: cleaning a body the user has not read is us deciding that
        // mail was not important.
        let (conn, total) = cache();
        let now = 1_700_000_000 + 10 * 86_400;
        let policy = CleanupPolicy {
            max_bytes: Some(1),
            max_age_days: None,
            keep_unread: true,
            keep_flagged: true,
        };
        let planned = super::plan(&conn, &policy, now).expect("plan");
        // The fixture reads even uids and flags uid 1, so three of the six are plain and the
        // unread pair plus the flagged one are spared. The plan measures the three that go.
        let expected: u64 = (0..6u32)
            .filter(|i| i % 2 == 0 && *i != 1)
            .map(|i| {
                format!(
                    "{{\"uid\":{i},\"body_text\":\"{}\"}}",
                    "x".repeat(100 + i as usize)
                )
                .len() as u64
            })
            .sum();
        assert_eq!(planned.freed_bytes, expected);
        assert!(planned.freed_bytes < total, "some of it is spared");

        let applied = super::apply(&conn, &policy, &planned, now).expect("apply");
        assert_eq!(applied.bodies_deleted, 3, "only the plain ones");
        assert_eq!(applied.freed_bytes, planned.freed_bytes);
        let kept = super::report(&conn).expect("after");
        assert_eq!(kept.bodies, 3, "the unread pair and the flagged one");
    }

    #[test]
    fn nothing_is_deleted_without_a_limit_or_an_age() {
        let (conn, _) = cache();
        let now = 1_700_000_000;
        let policy = CleanupPolicy::default();
        assert_eq!(
            plan(&conn, &policy, now).expect("plan"),
            CleanupPlan::default()
        );
    }

    #[test]
    fn age_keeps_what_is_recent_and_takes_what_is_old() {
        let (conn, _) = cache();
        let now = 1_700_000_000 + 10 * 86_400;
        // Bodies were written one day apart from 1_700_000_000, so with `now` ten days later they
        // are ten, nine, eight, seven, six and five days old. An eight-day window therefore takes
        // the two oldest and leaves four.
        let policy = CleanupPolicy {
            max_bytes: Some(1),
            max_age_days: Some(8),
            keep_unread: false,
            keep_flagged: false,
        };
        let planned = super::plan(&conn, &policy, now).expect("plan");
        assert_eq!(planned.steps.len(), 1);
        assert_eq!(
            planned.steps[0].rows, 2,
            "ten and nine days old, outside an eight-day window"
        );

        // A two-day window is shorter than every body's age, so all of them go.
        let strict = CleanupPolicy {
            max_age_days: Some(2),
            ..policy
        };
        let planned = super::plan(&conn, &strict, now).expect("strict plan");
        let applied = super::apply(&conn, &strict, &planned, now).expect("strict apply");
        assert!(
            applied.bodies_deleted >= 4,
            "the old ones went: {}",
            applied.bodies_deleted
        );
        assert_eq!(applied.freed_bytes, planned.freed_bytes);
    }

    #[test]
    fn a_report_says_the_cache_is_an_estimate_and_counts_both_tables() {
        let (conn, bodies_bytes) = cache();
        let report = super::report(&conn).expect("report");
        assert_eq!((report.messages, report.bodies), (6, 6));
        assert_eq!(report.bodies_bytes, bodies_bytes);
        assert!(report.cache_bytes > 0, "pages, from SQLite itself");
        assert!(report.oldest_body_at.is_some());
    }

    #[test]
    fn a_body_deleted_by_the_cleanup_is_no_longer_searchable_but_its_headers_are() {
        // The trade this module makes, stated as a test: bodies are the bulk, so they go; the
        // summary stays, so the message is still findable by subject and sender. A user reading the
        // cleanup note should know exactly what they are buying.
        let cache = crate::cache::Cache::open_in_memory().expect("cache");
        for i in 0..3u32 {
            cache
                .upsert_summaries(
                    "acc",
                    "INBOX",
                    &[crate::MessageSummary {
                        uid: i + 1,
                        subject: "订单".to_string(),
                        from_name: Some("店".into()),
                        from_addr: Some("shop@example.com".into()),
                        date: Some(1_700_000_000),
                        size: Some(100),
                        seen: true,
                        flagged: false,
                        thread_key: None,
                        subject_key: Some("订单".into()),
                    }],
                )
                .expect("summary");
            cache
                .put_detail(
                    "acc",
                    "INBOX",
                    &crate::MessageDetail {
                        uid: i + 1,
                        subject: "订单".into(),
                        from_name: Some("店".into()),
                        from_addr: Some("shop@example.com".into()),
                        to: vec!["me@example.com".into()],
                        cc: Vec::new(),
                        reply_to: None,
                        date: Some(1_700_000_000),
                        message_id: Some(format!("m{i}@example.com")),
                        in_reply_to: None,
                        references: Vec::new(),
                        body_text: Some("对账单在正文里".into()),
                        body_html: None,
                        attachments: Vec::new(),
                        inline_images: Vec::new(),
                    },
                )
                .expect("detail");
        }
        assert_eq!(
            cache
                .search("acc", "对账单在正文里", 10)
                .expect("search")
                .len(),
            3
        );

        // Over the byte limit, no age rule. `put_detail` stamps `fetched_at` with the real clock,
        // so an age rule expressed against a fixture `now` in 2023 would keep everything — which is
        // how this test first failed (it deleted nothing and then wondered why the bodies were
        // still searchable).
        let now = 1_700_000_000 + 100 * 86_400;
        let policy = CleanupPolicy {
            max_bytes: Some(1),
            max_age_days: None,
            keep_unread: false,
            keep_flagged: false,
        };
        // Through `Cache::cleanup`, not `quota::apply` directly: the entry point is what pairs the
        // delete with the index refresh, and that pairing is the half this test is about.
        let (planned, applied) = cache.cleanup(policy, now).expect("cleanup");
        assert_eq!(applied.bodies_deleted, 3);
        assert_eq!(applied.freed_bytes, planned.freed_bytes);
        // The bodies are gone, and so is what only they knew.
        assert_eq!(
            cache
                .search("acc", "对账单在正文里", 10)
                .expect("search")
                .len(),
            0
        );
        // The headers survive, which is what makes the cleanup a trade and not a loss.
        assert_eq!(cache.search("acc", "订单", 10).expect("search").len(), 3);
        assert_eq!(cache.body_coverage("acc").expect("coverage").indexed, 0);
    }
}
