//! Performance fixture and benchmark for the storage path (`design/15` §3/§4).
//!
//! `design/15` promised `tools/make-fixture.ps1` + `tests/fixtures/` and `tools/bench.ps1`, and a
//! 16-row budget table. None of that existed, which meant every performance number in the design
//! was unmeasured. This file is the fixture and the measurement; `tools/bench.ps1` drives it.
//!
//! Why a `#[ignore]`d test rather than a `benches/` target: a criterion benchmark needs a new
//! dependency (and this project has kept its dependency tree deliberately small), while an ignored
//! test still gets **compiled** by `cargo test --workspace` — so it cannot rot — and it reuses the
//! very API the application calls, which is the only way the numbers mean anything.
//!
//! Run it:
//! ```text
//! cargo test --release -p mail-core --test bench_fixture -- --ignored --nocapture
//! ```
//! Knobs (all optional, so a bare run is a fast smoke test):
//! * `EE_BENCH_MESSAGES` — messages to generate (default 5 000)
//! * `EE_BENCH_BODY_BYTES` — average body size (default 2 048; `design/15` assumes ~8 KB)
//! * `EE_BENCH_DIR` — where the throwaway database lives (default `%TEMP%\easy-email-bench`)
//! * `EE_BENCH_KEEP=1` — keep the database after the run
//!
//! What this deliberately does **not** measure: cold start, resident memory, idle CPU, scroll
//! smoothness and IPC payload latency. All five need the running desktop shell, so they are measured
//! by `tools/bench-app.ps1` instead. Saying so here matters: a storage benchmark that quietly stood
//! in for "the app is fast" would be the most expensive kind of wrong number.

use mail_core::{Cache, MessageDetail, MessageSummary};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

// ---------------------------------------------------------------------------- synthetic data

/// xorshift64*. Deterministic on purpose: two runs of the same spec must produce the same bytes, or
/// the numbers cannot be compared across days.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n.max(1)
    }

    fn chance(&mut self, percent: u64) -> bool {
        self.below(100) < percent
    }
}

/// Words that make the FTS index behave like a real Chinese mailbox: `design/09` exists because a
/// tokenizer that cannot match two-character Chinese words is useless here.
const CJK_WORDS: [&str; 20] = [
    "账单", "发票", "报销", "会议", "邀请", "项目", "进度", "周报", "合同", "审批", "采购", "客户",
    "需求", "上线", "版本", "发布", "测试", "数据", "报表", "预算",
];

const EN_WORDS: [&str; 20] = [
    "invoice", "meeting", "report", "project", "deploy", "review", "budget", "contract",
    "customer", "release", "build", "alert", "weekly", "agenda", "ticket", "incident", "invoice",
    "summary", "quota", "backlog",
];

/// A word that appears in ~1% of mail.
///
/// The vocabulary above is *twenty words long*, so any of them shows up in a large fraction of
/// messages — which makes it a worst case, not a realistic one. A real mailbox searching for
/// `账单` finds a handful of rows. Both shapes are measured, because they answer different
/// questions: the rare word is what a user waits for, the common one is the tail.
const RARE_WORD: &str = "龘靐";

fn sentence(rng: &mut Rng, words: usize, cjk: bool) -> String {
    let mut out = String::new();
    for _ in 0..words {
        if cjk {
            out.push_str(CJK_WORDS[rng.below(CJK_WORDS.len() as u64) as usize]);
        } else {
            out.push_str(EN_WORDS[rng.below(EN_WORDS.len() as u64) as usize]);
        }
        out.push(if cjk { '，' } else { ' ' });
    }
    out.push('。');
    out
}

/// Fill to roughly `target` bytes, so the fixture's size distribution is controlled rather than
/// whatever `sentence` happened to produce.
fn body(rng: &mut Rng, target: usize) -> String {
    let mut out = String::with_capacity(target + 64);
    while out.len() < target {
        let cjk = rng.chance(30);
        let words = 6 + rng.below(10) as usize;
        out.push_str(&sentence(rng, words, cjk));
        out.push('\n');
    }
    out
}

const FOLDERS: [&str; 8] = [
    "INBOX", "Sent", "Drafts", "Junk", "Trash", "Archive", "工作", "项目",
];

const ACCOUNT: &str = "bench@example.com";

struct Spec {
    messages: usize,
    body_bytes: usize,
    dir: PathBuf,
    keep: bool,
}

impl Spec {
    fn from_env() -> Self {
        let read = |name: &str, default: usize| {
            std::env::var(name)
                .ok()
                .and_then(|v| v.parse::<usize>().ok())
                .filter(|v| *v > 0)
                .unwrap_or(default)
        };
        let dir = std::env::var("EE_BENCH_DIR").map_or_else(
            |_| std::env::temp_dir().join("easy-email-bench"),
            PathBuf::from,
        );
        Self {
            messages: read("EE_BENCH_MESSAGES", 5_000),
            body_bytes: read("EE_BENCH_BODY_BYTES", 2_048),
            dir,
            keep: std::env::var("EE_BENCH_KEEP").is_ok_and(|v| v == "1"),
        }
    }
}

// ---------------------------------------------------------------------------- statistics

/// Sorted samples → the two numbers a budget table is actually about.
fn percentiles(mut samples: Vec<Duration>) -> (Duration, Duration, Duration) {
    samples.sort_unstable();
    let at = |p: f64| -> Duration {
        if samples.is_empty() {
            return Duration::ZERO;
        }
        let idx = ((samples.len() as f64 - 1.0) * p).round() as usize;
        samples[idx]
    };
    (at(0.50), at(0.95), at(1.0))
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1_000.0
}

fn mb(bytes: u64) -> f64 {
    bytes as f64 / (1024.0 * 1024.0)
}

fn file_bytes(path: &Path) -> u64 {
    std::fs::metadata(path).map(|m| m.len()).unwrap_or(0)
}

/// Checkpoint the WAL into the main file and report its size, so P13 is the size of the database
/// rather than the size of whichever part of it happened to still be in memory.
fn measured_db_bytes(path: &Path) -> u64 {
    let db = mail_core::db::Db::open(
        path,
        mail_core::db::DbKind::Cache,
        mail_core::db::DbOptions::for_kind(mail_core::db::DbKind::Cache),
    )
    .expect("raw handle for the checkpoint");
    let _ = db.exec_blocking(|conn| {
        conn.execute_batch("pragma wal_checkpoint(truncate);")?;
        Ok(())
    });
    drop(db);
    file_bytes(path)
}

/// Drop the search index, rebuild the file, and report its size: the difference against
/// [`measured_db_bytes`] is what the FTS index costs (`design/15` P14).
fn drop_index_and_measure(path: &Path) -> u64 {
    let db = mail_core::db::Db::open(
        path,
        mail_core::db::DbKind::Cache,
        mail_core::db::DbOptions::for_kind(mail_core::db::DbKind::Cache),
    )
    .expect("raw handle for the index delta");
    let _ = db.exec_blocking(|conn| {
        conn.execute_batch("delete from search; vacuum; pragma wal_checkpoint(truncate);")?;
        Ok(())
    });
    drop(db);
    file_bytes(path)
}

// ---------------------------------------------------------------------------- the benchmark

#[derive(Default)]
struct Indexed {
    /// Tokenised bytes handed to FTS5, i.e. what `design/15` P14 calls "已索引文本量".
    indexed_bytes: u64,
}

#[test]
#[ignore = "performance fixture; run explicitly, see the module docs"]
fn bench_storage_path() {
    let spec = Spec::from_env();
    let _ = std::fs::remove_dir_all(&spec.dir);
    std::fs::create_dir_all(&spec.dir).expect("fixture dir");

    let db_path = spec.dir.join("cache.db");
    let cache = Cache::open(&db_path).expect("open fixture cache");

    println!();
    println!(
        "fixture : messages={} folders={} body_bytes={} dir={}",
        spec.messages,
        FOLDERS.len(),
        spec.body_bytes,
        spec.dir.display()
    );

    let mut rng = Rng(0x2545_F491_4F6C_DD1D);
    let mut indexed = Indexed::default();

    // ---------------------------------------------------------------- P7: header ingest
    //
    // Batches of 500, which is what `design/15` §5 mandates ("one transaction per batch"): the
    // fixture must exercise the real write shape, not a per-row one that would make the number
    // meaningless (and four times slower).

    const BATCH: usize = 500;
    let ingest_start = Instant::now();
    // Messages are generated folder by folder, and a UID is unique **within its folder** — which is
    // what the primary key `(account_id, folder, uid)` demands. The first version of this fixture
    // numbered UIDs across folders, so most rows overwrote each other through the upsert and a
    // "2 000-message" database held 252 rows. That is the failure mode worth naming: the benchmark
    // measured a tenth of its own fixture and still printed a plausible number.
    let per_folder = spec.messages.div_ceil(FOLDERS.len());
    let mut generated: Vec<(&'static str, MessageSummary)> = Vec::with_capacity(spec.messages);
    let mut written = 0usize;

    'folders: for folder in FOLDERS {
        let mut batch: Vec<MessageSummary> = Vec::with_capacity(BATCH);
        for uid in 1..=per_folder as u32 {
            if written >= spec.messages {
                break;
            }
            let cjk = rng.chance(30);
            let words = 4 + rng.below(8) as usize;
            let mut subject = sentence(&mut rng, words, cjk);
            if rng.chance(1) {
                subject.push_str(RARE_WORD);
            }
            let from_name = if cjk {
                format!("发件人{}", rng.below(500))
            } else {
                format!("Sender {}", rng.below(500))
            };
            let from_addr = format!("user{}@example.com", rng.below(500));
            // P14's denominator: **every** column FTS5 indexes, tokenised the same way the cache
            // tokenises it. Counting only the body made the ratio exceed 100% and look like a bug
            // in the indexer rather than in the arithmetic here.
            indexed.indexed_bytes += (mail_core::cache::to_tokens(&subject).len()
                + mail_core::cache::to_tokens(&from_name).len()
                + mail_core::cache::to_tokens(&from_addr).len())
                as u64;
            // 35% of mail carries a `References` chain (design/15 §3's threads_ratio).
            let thread_key = rng
                .chance(35)
                .then(|| format!("root-{}", rng.below(20_000)));
            batch.push(MessageSummary {
                uid,
                subject,
                from_name: Some(from_name),
                from_addr: Some(from_addr),
                // UID and date must rise together, as they do on a real server: a fixture whose
                // newest mail has the lowest UID would make "newest first" and every `limit 50`
                // page read a different part of the table than production does.
                date: Some(1_600_000_000 + i64::from(uid) * 60),
                size: Some(2_048 + rng.below(5_000_000) as u32),
                seen: !rng.chance(18),
                flagged: rng.chance(3),
                thread_key,
                subject_key: None,
            });
            written += 1;

            if batch.len() == BATCH {
                cache
                    .upsert_summaries(ACCOUNT, folder, &batch)
                    .expect("upsert batch");
                generated.extend(batch.drain(..).map(|s| (folder, s)));
            }
        }
        if !batch.is_empty() {
            cache
                .upsert_summaries(ACCOUNT, folder, &batch)
                .expect("upsert batch");
            generated.extend(batch.drain(..).map(|s| (folder, s)));
        }
        if written >= spec.messages {
            break 'folders;
        }
    }
    let ingest = ingest_start.elapsed();
    assert_eq!(
        generated.len(),
        spec.messages,
        "the fixture must generate every message it claims to"
    );

    // ---------------------------------------------------------------- P9: bodies + index

    // 35% of messages are inside the prefetch window (design/15 §3).
    let with_body = generated.len() * 35 / 100;
    let body_start = Instant::now();
    for (folder, summary) in generated.iter().take(with_body) {
        let text = body(&mut rng, spec.body_bytes);
        let to_json = serde_json::to_string(&[ACCOUNT.to_string()]).expect("recipients");
        indexed.indexed_bytes += (mail_core::cache::to_tokens(&text).len()
            + mail_core::cache::to_tokens(&to_json).len()) as u64;
        let detail = MessageDetail {
            uid: summary.uid,
            subject: summary.subject.clone(),
            from_name: summary.from_name.clone(),
            from_addr: summary.from_addr.clone(),
            to: vec![ACCOUNT.to_string()],
            cc: Vec::new(),
            reply_to: None,
            date: summary.date,
            message_id: Some(format!("<bench-{}@example.com>", summary.uid)),
            in_reply_to: None,
            references: Vec::new(),
            body_text: Some(text),
            body_html: None,
            attachments: if rng.chance(20) {
                vec![mail_core::AttachmentMeta {
                    name: "report.pdf".into(),
                    mime: "application/pdf".into(),
                    size: 200_000 + rng.below(5_000_000) as usize,
                    part: 2,
                    inline: false,
                    content_id: None,
                }]
            } else {
                Vec::new()
            },
            inline_images: Vec::new(),
        };
        cache
            .put_detail(ACCOUNT, folder, &detail)
            .expect("put detail");
    }
    let bodies = body_start.elapsed();

    // ---------------------------------------------------------------- P2/P4: reads

    const ITERATIONS: usize = 200;
    let mut list_samples = Vec::with_capacity(ITERATIONS);
    for i in 0..ITERATIONS {
        // Offsets spread across the folder, so a missing index shows up as a rising tail rather
        // than hiding behind "the newest 50 rows are always in the first page".
        let offset = (i * 37) as u32 % 5_000;
        let t = Instant::now();
        let rows = cache
            .list_summaries(ACCOUNT, "INBOX", 50, offset)
            .expect("list");
        list_samples.push(t.elapsed());
        std::hint::black_box(rows);
    }
    let (list_p50, list_p95, list_max) = percentiles(list_samples);

    // Three query shapes, because they are three different costs and averaging them hides the one
    // that matters:
    //  * `龘靐` is a rare two-character CJK word — the case `design/09` exists for, and the fastest;
    //  * `账单` is a *common* two-character CJK word — the same shape with a large match set;
    //  * `invoice` is an ASCII word, and `to_query` turns it into a seven-token phrase
    //    (`"i n v o i c e"`), which is what makes substring search work and is by far the most
    //    expensive thing a user can type.
    let mut rare_samples = Vec::new();
    let mut common_samples = Vec::new();
    let mut ascii_samples = Vec::new();
    let mut rare_hits = 0;
    let mut common_hits = 0;
    let mut ascii_hits = 0;
    for _ in 0..ITERATIONS {
        let t = Instant::now();
        let hits = cache.search(ACCOUNT, RARE_WORD, 50).expect("search rare");
        rare_samples.push(t.elapsed());
        rare_hits = rare_hits.max(hits.len());
        std::hint::black_box(hits);

        let t = Instant::now();
        let hits = cache.search(ACCOUNT, "账单", 50).expect("search common");
        common_samples.push(t.elapsed());
        common_hits = common_hits.max(hits.len());
        std::hint::black_box(hits);
    }
    // Fewer samples for the expensive shape, which is the honest way to keep the run bounded without
    // dropping the measurement.
    for _ in 0..20 {
        let t = Instant::now();
        let hits = cache.search(ACCOUNT, "invoice", 50).expect("search ascii");
        ascii_samples.push(t.elapsed());
        ascii_hits = ascii_hits.max(hits.len());
        std::hint::black_box(hits);
    }
    let (rare_p50, rare_p95, rare_max) = percentiles(rare_samples);
    let (common_p50, common_p95, common_max) = percentiles(common_samples);
    let (ascii_p50, ascii_p95, ascii_max) = percentiles(ascii_samples);

    // Conversation view: one query on `thread_key`, which is what opening a row in the list does.
    let mut thread_samples = Vec::new();
    if let Some(first) = generated
        .iter()
        .map(|(_, s)| s)
        .find(|s| s.thread_key.is_some())
    {
        let key = first.thread_key.clone().expect("checked");
        for _ in 0..ITERATIONS {
            let t = Instant::now();
            let rows = cache.thread_summaries(ACCOUNT, &key, 200).expect("thread");
            thread_samples.push(t.elapsed());
            std::hint::black_box(rows);
        }
    }
    let (thread_p50, thread_p95, thread_max) = percentiles(thread_samples);

    // P12: what crosses the IPC boundary for one page of the list.
    let page = cache
        .list_summaries(ACCOUNT, "INBOX", 100, 0)
        .expect("page");
    let json_start = Instant::now();
    let json = serde_json::to_string(&page).expect("serialize page");
    let json_time = json_start.elapsed();

    // ---------------------------------------------------------------- P13/P14: volume
    //
    // Measured on the main file after a WAL checkpoint, not on the directory: while the connection
    // is open, the newest pages live in `-wal` and a directory sum under-reports the total, which
    // is exactly the kind of number that looks fine and means nothing.
    //
    // The FTS footprint is a **delta**, not a guess: the index is dropped and the file rebuilt, and
    // the difference is what the index was costing. `dbstat` would answer directly but rusqlite's
    // bundled build does not compile it in.

    drop(cache);
    let db_bytes = measured_db_bytes(&db_path);
    let without_index = drop_index_and_measure(&db_path);
    let index_bytes = db_bytes.saturating_sub(without_index);

    println!();
    println!(
        "P7  头部入库吞吐        {:>9.0} 封/秒   ({:.0} ms / {} 封)",
        spec.messages as f64 / ingest.as_secs_f64(),
        ms(ingest),
        spec.messages
    );
    println!(
        "P9  正文+索引入库吞吐   {:>9.0} 封/秒   ({:.0} ms / {} 封)",
        with_body as f64 / bodies.as_secs_f64(),
        ms(bodies),
        with_body
    );
    println!(
        "P2  列表分页 50 行      P50 {:>6.2} ms  P95 {:>6.2} ms  max {:>6.2} ms",
        ms(list_p50),
        ms(list_p95),
        ms(list_max)
    );
    println!(
        "P4  搜索 CJK 稀有词      P50 {:>7.2} ms  P95 {:>7.2} ms  max {:>7.2} ms  ({rare_hits} 行)",
        ms(rare_p50),
        ms(rare_p95),
        ms(rare_max)
    );
    println!(
        "P4  搜索 CJK 高频词      P50 {:>7.2} ms  P95 {:>7.2} ms  max {:>7.2} ms  ({common_hits} 行)",
        ms(common_p50),
        ms(common_p95),
        ms(common_max)
    );
    println!(
        "P4  搜索 ASCII 单词      P50 {:>7.2} ms  P95 {:>7.2} ms  max {:>7.2} ms  ({ascii_hits} 行)",
        ms(ascii_p50),
        ms(ascii_p95),
        ms(ascii_max)
    );
    println!(
        "    会话视图 200 行     P50 {:>6.2} ms  P95 {:>6.2} ms  max {:>6.2} ms",
        ms(thread_p50),
        ms(thread_p95),
        ms(thread_max)
    );
    println!(
        "P12 100 行 JSON         {:.2} ms  载荷 {:.1} KB",
        ms(json_time),
        json.len() as f64 / 1024.0
    );
    println!(
        "P13 数据库体积          {:.1} MB   ({:.1} KB/封, 含 {} 封正文, 不含附件字节)",
        mb(db_bytes),
        db_bytes as f64 / 1024.0 / spec.messages as f64,
        with_body
    );
    println!(
        "P14 FTS 索引            {:.1} MB   = {}% 的已索引文本量 ({:.1} MB)",
        mb(index_bytes),
        if indexed.indexed_bytes > 0 {
            format!(
                "{:.1}",
                index_bytes as f64 / indexed.indexed_bytes as f64 * 100.0
            )
        } else {
            "n/a".to_string()
        },
        mb(indexed.indexed_bytes)
    );
    println!();

    // ---------------------------------------------------------------------------- assertions
    //
    // Deliberately loose. A debug build is 10-30x slower than a release build for this code, and a
    // threshold tight enough to be a regression test would fail on every `cargo test` a developer
    // runs. These catch the catastrophic (a missing index, a per-row transaction) and the report
    // carries the real numbers for the budget comparison.

    let ingest_rate = spec.messages as f64 / ingest.as_secs_f64();
    assert!(
        ingest_rate > 200.0,
        "header ingest collapsed to {ingest_rate:.0}/s; a per-row transaction is the usual cause"
    );
    assert!(
        ms(list_p95) < 200.0,
        "list page P95 is {:.1} ms; `messages_by_date` is probably not being used",
        ms(list_p95)
    );
    assert!(
        ms(common_p95) < 500.0,
        "the worst-case search P95 is {:.1} ms; the FTS index is probably not being used",
        ms(common_p95)
    );

    if !spec.keep {
        let _ = std::fs::remove_dir_all(&spec.dir);
    } else {
        println!("fixture kept at {}", spec.dir.display());
    }
}

/// A tiny, fast smoke test that is **not** ignored, so a broken fixture is caught by `cargo test`
/// rather than by whoever next needs a performance number.
#[test]
fn the_fixture_generates_and_can_be_searched() {
    let dir = std::env::temp_dir().join(format!("easy-email-bench-smoke-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("dir");
    let cache = Cache::open(&dir.join("cache.db")).expect("open");

    let mut rng = Rng(7);
    let summaries: Vec<MessageSummary> = (1..=40)
        .map(|uid| MessageSummary {
            uid,
            subject: sentence(&mut rng, 4, true),
            from_name: Some("发件人".into()),
            from_addr: Some("a@example.com".into()),
            date: Some(1_600_000_000 + i64::from(uid) * 60),
            size: Some(1_024),
            seen: false,
            flagged: false,
            thread_key: None,
            subject_key: None,
        })
        .collect();
    cache
        .upsert_summaries(ACCOUNT, "INBOX", &summaries)
        .expect("upsert");

    let text = "账单 发票 报销 会议 邀请".repeat(20);
    cache
        .put_detail(
            ACCOUNT,
            "INBOX",
            &MessageDetail {
                uid: 1,
                subject: "账单".into(),
                from_name: Some("发件人".into()),
                from_addr: Some("a@example.com".into()),
                to: vec![ACCOUNT.into()],
                cc: Vec::new(),
                reply_to: None,
                date: Some(1_700_000_000),
                message_id: None,
                in_reply_to: None,
                references: Vec::new(),
                body_text: Some(text.clone()),
                body_html: None,
                attachments: Vec::new(),
                inline_images: Vec::new(),
            },
        )
        .expect("detail");

    // 账单 must match through the body, and 发票 is a two-character word: both are the point of
    // `to_tokens`/`to_query`. The synthetic *subjects* are drawn from the same word list, so the
    // assertion is on identity (the body's own uid) rather than on a count — a count here would
    // only be pinning how often the fixture happened to pick that word.
    let hits = cache.search(ACCOUNT, "账单", 50).expect("search body");
    assert!(
        hits.iter().any(|h| h.uid == 1),
        "the body's own text must be findable: {hits:?}"
    );
    let hits = cache.search(ACCOUNT, "发票", 50).expect("search cjk word");
    assert!(!hits.is_empty(), "two-character Chinese must match");
    let hits = cache
        .search(ACCOUNT, "zzz-not-present", 10)
        .expect("search miss");
    assert!(
        hits.is_empty(),
        "a term that is not there must return nothing"
    );

    let page = cache.list_summaries(ACCOUNT, "INBOX", 10, 0).expect("list");
    assert_eq!(page.len(), 10);
    assert_eq!(page[0].uid, 40, "newest first");

    drop(cache);
    let _ = std::fs::remove_dir_all(&dir);
}
