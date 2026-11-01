//! D-1: the read-only probe that decides *which* fix the "sent copy is invisible" bug needs, plus the
//! end-to-end check of the fix it chose.
//!
//! `design/07` §14.2 states the symptom with real data: the outbox row says `sent`, `sent_index`
//! points at UID 226 in `Sent Messages`, the folder's cursor is 209, and ten syncs in a row reported
//! "新增 0 封". The cause was *not* located — at least three different faults produce that symptom,
//! and they need three different fixes. §14.3 therefore puts a diagnostic before any of them, and
//! §14.10 makes it a hard precondition: "出结论之前不许动第 2 步".
//!
//! The verdict on 2026-11-01 was **branch A**: `UID SEARCH 210:*` empty, `UID SEARCH ALL` containing
//! 226, `UID FETCH 210:* (UID)` containing 226. `fetch_headers_since` now enumerates the range with
//! `UID FETCH … (UID)` instead of searching it, and the second test here is what shows the user's
//! contradiction is gone: after one sync of that folder the cursor is at the UID the outbox claimed.
//!
//! Both tests are `#[ignore]`d so the gate never dials a real server:
//!
//! ```text
//! cargo test -p mail-core --test sent_uid_probe -- --ignored --nocapture
//! ```
//!
//! `EE_PROBE_ACCOUNT` picks the account id (default: the first password account). When the local
//! cache has no `sent_index` row to test, they say so and stop rather than inventing a target.

use std::path::{Path, PathBuf};

use mail_core::cache::Cache;
use mail_core::imap::{ImapClient, UidBranch};
use mail_core::model::{AccountConfig, MailboxInfo};
use mail_core::secret;
use mail_core::sync::{SyncDriver, SyncOutcome};

#[derive(serde::Deserialize)]
struct AccountsFile {
    accounts: Vec<AccountConfig>,
}

fn profile_dir() -> Option<PathBuf> {
    let appdata = std::env::var_os("APPDATA")?;
    let dir = PathBuf::from(appdata).join("com.easyemail.desktop");
    dir.is_dir().then_some(dir)
}

/// `(last_uid, uidvalidity)` for one folder, and the highest UID `sent_index` claims for it.
///
/// Opened read-only on purpose: this diagnostic must not be able to migrate, checkpoint or repair the
/// user's cache. When the read fails the probe still runs, because a missing local number is not a
/// reason to skip the question.
fn local_state(
    dir: &Path,
    account_id: &str,
    folder: &str,
) -> (Option<u32>, Option<u32>, Option<u32>) {
    let path = dir.join("cache.db");
    let Ok(conn) = rusqlite::Connection::open_with_flags(
        &path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_URI,
    ) else {
        return (None, None, None);
    };

    let cursor = conn
        .query_row(
            "select last_uid, uidvalidity from folders where account_id = ?1 and name = ?2",
            rusqlite::params![account_id, folder],
            |row| Ok((row.get::<_, u32>(0)?, row.get::<_, u32>(1)?)),
        )
        .ok();
    let target = conn
        .query_row(
            "select max(uid) from sent_index where account_id = ?1 and folder = ?2",
            rusqlite::params![account_id, folder],
            |row| row.get::<_, Option<u32>>(0),
        )
        .ok()
        .flatten();

    (
        cursor.map(|(uid, _)| uid),
        cursor.map(|(_, validity)| validity),
        target,
    )
}

struct Live {
    client: ImapClient,
    account: AccountConfig,
    dir: PathBuf,
    sent: String,
    after_uid: u32,
    local_validity: Option<u32>,
    target: u32,
}

/// Everything both tests need: a logged-in client and the exact numbers from the user's own cache.
///
/// `None` means "this machine cannot answer the question" — no profile, no password, no `sent_index`
/// row to compare against. A test that silently passes in that case would be worse than one that
/// says so out loud.
async fn live_account() -> Option<Live> {
    let dir = profile_dir()?;
    let raw = std::fs::read_to_string(dir.join("accounts.json")).ok()?;
    let accounts: AccountsFile = serde_json::from_str(&raw).ok()?;

    let wanted = std::env::var("EE_PROBE_ACCOUNT").ok();
    let account = accounts
        .accounts
        .iter()
        .find(|a| match &wanted {
            Some(id) => &a.id == id,
            None => matches!(a.auth, mail_core::model::AuthMethod::Password),
        })?
        .clone();

    let password = secret::load_password(&account.id)
        .expect("credential vault")
        .expect("a stored password for this account");
    let mut client = ImapClient::connect(&account, &password)
        .await
        .expect("connect and log in");

    let sent = client
        .sent_mailbox()
        .await
        .expect("list mailboxes")
        .expect("this account has a Sent folder");

    let (cursor, local_validity, target) = local_state(&dir, &account.id, &sent);
    let (Some(after_uid), Some(target)) = (cursor, target) else {
        eprintln!(
            "no sent_index row for {sent} in the local cache — nothing to compare. \
             Send one mail from the app first; the probe needs a UID the cache already knows about."
        );
        return None;
    };

    Some(Live {
        client,
        account,
        dir,
        sent,
        after_uid,
        local_validity,
        target,
    })
}

#[tokio::test]
#[ignore = "dials the real IMAP server; run by hand, see the module comment"]
async fn probe_what_uid_search_and_fetch_say_about_a_sent_copy() {
    let Some(mut live) = live_account().await else {
        return;
    };

    let probe = live
        .client
        .probe_uid_visibility(&live.sent, live.after_uid, live.target)
        .await
        .expect("probe runs");

    println!("\n=== D-1 探针 ===");
    println!("账号        {}", live.account.id);
    println!("文件夹      {}（线路名）", live.sent);
    println!(
        "SELECT      UIDVALIDITY={} UIDNEXT={}",
        probe.uidvalidity, probe.uidnext
    );
    println!(
        "本地游标    last_uid={} uidvalidity={}",
        live.after_uid,
        live.local_validity.unwrap_or(0)
    );
    println!("目标 UID    {}", live.target);
    println!(
        "SEARCH {}:*  → {:?}",
        live.after_uid + 1,
        probe.search_since
    );
    println!("SEARCH ALL             → {:?}", probe.search_all);
    println!("FETCH  {}:*  → {:?}", live.after_uid + 1, probe.fetch_since);
    if let Some(error) = &probe.error {
        println!("首个失败     {error}");
    }
    println!("{}\n", probe.verdict());

    assert_ne!(
        probe.branch(),
        UidBranch::Undecided,
        "the probe could not conclude anything — read the lines above, do not guess a branch"
    );

    if probe.branch() == UidBranch::ServerAgrees {
        // B or C: the server is fine, so the difference has to be in the local numbers. Stating the
        // comparison is what turns "server agrees" into a usable next step.
        println!(
            "本地 uidvalidity {} vs 服务器 {} ⇒ {}",
            live.local_validity.unwrap_or(0),
            probe.uidvalidity,
            if live.local_validity == Some(probe.uidvalidity) {
                "同一代文件夹，游标就是问题所在（查 advance_cursor / absorb）"
            } else {
                "文件夹被重建过，缓存里的行本来就该作废（不是 bug）"
            }
        );
    }

    live.client.logout().await.expect("logout");
}

/// The fix, end to end, against the account that reported the bug.
///
/// Branch A says the loss was "the sync never saw the message"; this test is the claim that it now
/// does: one sync of the folder the outbox pointed at must leave the cursor on that very UID. It
/// writes only to the local cache (exactly what opening the folder does), never to the mailbox.
#[tokio::test]
#[ignore = "writes to the real cache and dials the real server"]
async fn one_sync_moves_the_cursor_onto_the_copy_the_outbox_claims() {
    let Some(mut live) = live_account().await else {
        return;
    };
    let cache = Cache::open(&live.dir.join("cache.db")).expect("open the app's own cache");
    let folder = MailboxInfo {
        name: live.sent.clone(),
        display_name: "已发送".to_string(),
        delimiter: Some("/".to_string()),
        selectable: true,
    };

    let mut driver = SyncDriver::start(
        &cache,
        &live.account.id,
        &folder,
        &mail_core::userdb::Tombstones::empty(),
    )
    .expect("plan");
    let outcome: SyncOutcome = loop {
        let page = driver
            .execute(&mut live.client)
            .await
            .expect("fetch the page");
        if let Some(outcome) = driver.absorb(&cache, &page).expect("absorb the page") {
            break outcome;
        }
    };

    println!(
        "\n=== 同步 {} ===\n新增 {} 封 · 缓存共 {} 封 · last_uid {} · uidvalidity 变了？{}\n",
        live.sent,
        outcome.fetched,
        outcome.total_cached,
        outcome.last_uid,
        outcome.uidvalidity_changed
    );

    assert!(
        !outcome.uidvalidity_changed,
        "a renumbered folder would invalidate the comparison rather than prove anything"
    );
    assert_eq!(
        outcome.last_uid, live.target,
        "the copy the outbox recorded (UID {}) has to be inside the cache after one sync",
        live.target
    );

    live.client.logout().await.expect("logout");
}
