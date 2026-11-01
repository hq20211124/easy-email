//! The settle step against the **real profile's own files** (`design/07` §14.7 ⓿, §14.11).
//!
//! The unit tests build the shapes they need. This one runs on a copy of what the user's machine
//! actually holds: three `outbox` rows in `state = 'sent'` whose spool is already gone (ids 11/12/14)
//! and whose copies are at UIDs 226/209/207 in `Sent Messages`. That shape is the whole reason the v5
//! review rejected the earlier plan: "the spool is empty" is *not* evidence of "there is no copy", and
//! settling on it would write three known locations down as unknown.
//!
//! Copies, never the originals: the test must not be able to damage a mailbox the user is using.
//! Run by hand:
//!
//! ```text
//! cargo test -p mail-core --test real_settle -- --ignored --nocapture
//! ```

use std::path::{Path, PathBuf};

use mail_core::cache::Cache;
use mail_core::model::AccountConfig;
use mail_core::spool::Spool;
use mail_core::userdb::UserStore;
use mail_core::worker::{FilingRun, settle_stale_sent_rows};

#[derive(serde::Deserialize)]
struct AccountsFile {
    accounts: Vec<AccountConfig>,
}

fn copy_profile_to(dir: &Path) -> Option<PathBuf> {
    let appdata = std::env::var_os("APPDATA")?;
    let source = PathBuf::from(appdata).join("com.easyemail.desktop");
    if !source.is_dir() {
        return None;
    }
    let target = dir.join("profile");
    std::fs::create_dir_all(&target).expect("mkdir");
    for name in [
        "user.db",
        "user.db-wal",
        "cache.db",
        "cache.db-wal",
        "accounts.json",
    ] {
        let from = source.join(name);
        if from.exists() {
            std::fs::copy(&from, target.join(name)).expect("copy");
        }
    }
    Some(target)
}

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "easy-email-real-settle-{}-{}-{}",
        std::process::id(),
        name,
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).expect("dir");
    dir
}

#[test]
#[ignore = "reads a copy of the real profile; run by hand"]
fn the_real_leftover_rows_keep_the_location_the_index_knows() {
    let dir = temp_dir("settle");
    let Some(profile) = copy_profile_to(&dir) else {
        eprintln!("no app profile on this machine — nothing to settle");
        return;
    };

    let raw = std::fs::read_to_string(profile.join("accounts.json")).expect("accounts.json");
    let accounts: AccountsFile = serde_json::from_str(&raw).expect("accounts.json parses");
    let account = accounts
        .accounts
        .into_iter()
        .find(|a| a.imap.host.contains("qq"))
        .or_else(|| {
            serde_json::from_str::<AccountsFile>(&raw)
                .ok()
                .and_then(|file| file.accounts.into_iter().next())
        })
        .expect("an account to settle");

    let store = UserStore::open(&profile.join("user.db")).expect("user.db");
    let cache = Cache::open(&profile.join("cache.db")).expect("cache.db");
    let spool = Spool::open(dir.join("spool")).expect("spool");

    let before: Vec<(i64, Option<String>)> = store
        .list_outbox(Some(&account.id))
        .expect("list")
        .into_iter()
        .filter(|item| item.state == "sent" && item.spool_path.is_none())
        .map(|item| (item.id, item.sent_message_id))
        .collect();
    println!("\n结案前：{} 条 state=sent 且 spool 为空", before.len());
    for (id, message_id) in &before {
        println!("  outbox id {id}  message_id {message_id:?}");
    }

    let settled = settle_stale_sent_rows(&FilingRun {
        store: &store,
        spool: &spool,
        account_id: &account.id,
        cache: &cache,
        now: 1_800_000_000,
    })
    .expect("settle");
    println!("结案 {settled} 条");

    let left = store
        .list_outbox(Some(&account.id))
        .expect("list")
        .into_iter()
        .filter(|item| item.state == "sent")
        .count();
    assert_eq!(
        left, 0,
        "一条都不该留在队列里（design/07 §14.6.3 的可证伪）"
    );

    let mut unknown = 0;
    for (id, message_id) in &before {
        let message_id = message_id
            .as_ref()
            .expect("these rows carry their frozen id");
        let record = store
            .sent_record(&account.id, message_id)
            .expect("read")
            .expect("every settled row is recorded");
        println!(
            "  id {id} → folder {:?} uid {:?} uidvalidity {}",
            record.folder, record.uid, record.uidvalidity
        );
        if record.folder.is_empty() {
            unknown += 1;
        }
    }
    assert_eq!(
        unknown, 0,
        "spool 为空不等于没有副本：真库这三条的副本在 Sent Messages 的 226/209/207"
    );

    let records = store.count_sent_records(&account.id).expect("count");
    println!("sent_records 共 {records} 条\n");
    assert!(
        records >= before.len() as u32,
        "至少要把搬过来的这几条记下来"
    );

    drop(store);
    drop(cache);
    let _ = std::fs::remove_dir_all(&dir);
}
