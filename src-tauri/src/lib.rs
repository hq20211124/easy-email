//! Tauri command surface for Easy Email.
//!
//! Every command is a thin wrapper over `mail-core`; the protocol logic lives there so it can be
//! tested without the desktop shell.
//!
//! Connection strategy for v0.1: each command opens a fresh IMAP connection, does its work, and
//! logs out. That is one TLS handshake per action, which is wasteful, but it removes an entire
//! connection-pool subsystem from the first version and guarantees we never hold a socket in a
//! half-broken state. Pooling, and honouring `AccountConfig::max_connections`, is the natural
//! next step.

mod store;

use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use mail_core::providers::{self, Provider};
#[macro_use]
extern crate mail_core;
use mail_core::{
    AccountConfig, BackfillBudget, BackfillReport, Cache, ConnectionPool, ConnectionReport,
    MailError, MailboxInfo, MessageDetail, MessageSummary, NewOutbox, OutboxItem, OutboxRun,
    OutgoingAttachment, OutgoingMail, PushEvent, RecoveryReport, RecoveryRun, RunReport, SearchHit,
    Spool, SyncDriver, SyncOutcome, SyncWindow, account_with_login, backfill_older,
    recover_in_flight, run_due, secret, smtp,
    userdb::{Draft, UserStore},
    watch,
};
use tauri::menu::{Menu, MenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Emitter, Manager, State};
use tauri_plugin_dialog::DialogExt;
use tauri_plugin_notification::NotificationExt;
use tokio::sync::Notify;

/// Shared, mutable application state.
struct AppState {
    /// Where the non-secret account list is persisted.
    accounts_path: PathBuf,
    accounts: Mutex<Vec<AccountConfig>>,
    /// The open cache handle, shared by every command.
    ///
    /// A handle rather than a bare connection (and rather than a path): `Cache` is `Clone + Send +
    /// Sync` because it owns a [`mail_core::db::Db`] — one writer thread plus a pool of readers — so
    /// an `async` command can hold it across an `.await` without its future becoming non-`Send`.
    /// Cloning it per command is what removed v0.2's "one SQLite connection per command, one implicit
    /// transaction per row" shape.
    cache: Cache,
    /// Why the last attempt to refresh from the server failed, if it did.
    ///
    /// Exists so the UI can say "showing cached mail" instead of silently presenting data of
    /// unknown age as if it were fresh.
    last_sync_error: Mutex<Option<String>>,
    /// Reused IMAP connections.
    ///
    /// In an `Arc` because the IDLE watcher holds one for the whole life of the app: it reconnects
    /// and re-syncs on its own, long after the command that started it returned.
    pool: Arc<ConnectionPool>,
    /// Running IDLE watchers, keyed by account id.
    watchers: Mutex<HashMap<String, Watcher>>,
    /// Where attachments are saved. Resolved once at startup from Tauri's path resolver.
    download_dir: PathBuf,
    /// The **user** database: drafts, queued sends and offline actions.
    ///
    /// A separate file from the cache because it holds things that cannot be re-fetched. That is also
    /// why "just delete cache.db" is safe advice and "delete user.db" is not — the two must never be
    /// the same file (`design/03` §3.1).
    user: UserStore,
    /// Frozen message bytes, waiting to be sent (`design/07` §5.3).
    spool: Spool,
    /// Wakes the outbox worker immediately.
    ///
    /// The tick is 5 s, but three events cannot wait for it: a message just queued (the undo window
    /// counts down in front of the user), an undo window that just expired, and a manual "retry now".
    outbox_wake: Arc<Notify>,
}

/// The two tasks that make up one account's push pipeline.
///
/// Kept separately so both can be stopped: aborting only the handler would leave the IDLE task
/// holding a connection until its next idle window ended, which can be most of a minute.
struct Watcher {
    pump: tauri::async_runtime::JoinHandle<()>,
    handler: tauri::async_runtime::JoinHandle<()>,
}

/// Payload of the `mail:new` event the frontend listens for.
#[derive(Clone, serde::Serialize)]
struct MailUpdate {
    account_id: String,
    mailbox: String,
    /// Messages the sync actually pulled in, i.e. how many are new.
    new_messages: usize,
    total_cached: u32,
    unread: u32,
}

impl AppState {
    fn snapshot(&self) -> Vec<AccountConfig> {
        self.accounts
            .lock()
            .expect("accounts mutex poisoned")
            .clone()
    }

    /// Tell the outbox worker to look at the queue now instead of at its next tick.
    fn wake_outbox(&self) {
        self.outbox_wake.notify_one();
    }

    /// Clone one account out of the lock.
    ///
    /// Returning an owned value matters: commands are `async`, and holding a guard across an
    /// `.await` would both fail to compile and risk a deadlock.
    fn find(&self, account_id: &str) -> Result<AccountConfig, String> {
        self.accounts
            .lock()
            .expect("accounts mutex poisoned")
            .iter()
            .find(|a| a.id == account_id)
            .cloned()
            .ok_or_else(|| format!("找不到账号「{account_id}」，请重新添加。"))
    }
}

/// Render an error the way a user can act on it: cause first, then the suggested fix.
fn describe(e: MailError) -> String {
    match e.user_hint() {
        Some(hint) => format!("{e}\n\n{hint}"),
        None => e.to_string(),
    }
}

/// Fetch the stored credential, with a message that explains the common case (never saved).
fn password_for(account_id: &str) -> Result<String, String> {
    match secret::load_password(account_id) {
        Ok(Some(password)) => Ok(password),
        Ok(None) => Err("该账号还没有保存密码或授权码。请删除后重新添加账号。".to_string()),
        Err(e) => Err(describe(e)),
    }
}

// ---------------------------------------------------------------- provider database

/// The full built-in provider database, for the setup wizard.
#[tauri::command]
fn list_providers() -> Vec<&'static Provider> {
    providers::all().iter().collect()
}

/// Guess the provider from an email address as the user types it.
#[tauri::command]
fn suggest_provider(email: String) -> Option<&'static Provider> {
    providers::by_email(&email)
}

/// Resolve a provider preset into a concrete account, without saving anything.
///
/// Lets the wizard show the exact host/port it is about to use, so the user never has to look
/// them up but can still see what was chosen.
#[tauri::command]
fn build_account(
    provider_id: String,
    email: String,
    display_name: String,
    username: Option<String>,
) -> Result<AccountConfig, String> {
    account_with_login(&provider_id, &email, &display_name, username.as_deref()).map_err(describe)
}

// ---------------------------------------------------------------- accounts

#[tauri::command]
fn list_accounts(state: State<'_, AppState>) -> Vec<AccountConfig> {
    state.snapshot()
}

/// Add or update an account and stash the credential in the Windows Credential Manager.
#[tauri::command]
fn save_account(
    state: State<'_, AppState>,
    app: AppHandle,
    provider_id: String,
    email: String,
    display_name: String,
    password: String,
    username: Option<String>,
) -> Result<AccountConfig, String> {
    let cfg = account_with_login(&provider_id, &email, &display_name, username.as_deref())
        .map_err(describe)?;

    // Persist the credential first: an account that exists but cannot authenticate is worse than
    // a credential with no account entry.
    secret::save_password(&cfg.id, &password).map_err(describe)?;

    {
        let mut accounts = state.accounts.lock().expect("accounts mutex poisoned");
        match accounts.iter_mut().find(|a| a.id == cfg.id) {
            Some(existing) => *existing = cfg.clone(),
            None => accounts.push(cfg.clone()),
        }
        store::save(&state.accounts_path, &accounts).map_err(describe)?;
    }

    // Start push for the new account straight away, rather than waiting for the next launch. A
    // failure here is logged, not returned: the account is saved and usable either way.
    if let Err(error) = start_watcher(&app, state.inner(), cfg.clone(), "INBOX".to_string()) {
        log_line(&format!("[easy-email] 未能启动 {} 的推送：{error}", cfg.id));
    }

    Ok(cfg)
}

#[tauri::command]
async fn delete_account(state: State<'_, AppState>, account_id: String) -> Result<(), String> {
    {
        let mut accounts = state.accounts.lock().expect("accounts mutex poisoned");
        accounts.retain(|a| a.id != account_id);
        store::save(&state.accounts_path, &accounts).map_err(describe)?;
    }
    // Best effort: a missing vault entry must not prevent removing the account.
    let _ = secret::delete_password(&account_id);
    // And stop its IDLE watcher, or a task would keep a connection open for a deleted account.
    stop_watcher(state.inner(), &account_id);
    state.pool.clear(&account_id);
    // The cache is keyed by account id and would otherwise sit there forever. A failure here is
    // logged rather than returned: it must not block removing the account.
    if let Err(error) = state.cache.clone().forget_account(&account_id) {
        log_line(&format!("[easy-email] 清理账号缓存失败：{error}"));
    }
    Ok(())
}

/// Probe IMAP and SMTP with a real login before the user commits to the account.
#[tauri::command]
async fn verify_account(
    state: State<'_, AppState>,
    account_id: String,
) -> Result<ConnectionReport, String> {
    let cfg = state.find(&account_id)?;
    let password = password_for(&cfg.id)?;
    Ok(mail_core::verify_account(&cfg, &password).await)
}

// ---------------------------------------------------------------- receive

/// One page of **older** mail, fetched from the server and cached.
///
/// The list only ever holds the newest window, so a mailbox used for years needs a way back. This is
/// that way: ask the server for the messages just below the oldest cached one, store them, and hand
/// them back for the list to prepend.
///
/// It deliberately does **not** touch the incremental cursor — `last_uid` keeps pointing at the
/// newest stored message, so scrolling into history can never make the updater skip mail it never
/// stored. `Cache::oldest_uid` is the only cursor it reads, and `tools/check.ps1` pins that.
#[tauri::command]
async fn load_more_messages(
    state: State<'_, AppState>,
    account_id: String,
    mailbox: String,
    limit: usize,
) -> Result<HistoryPage, String> {
    let cfg = state.find(&account_id)?;
    let limit = limit.clamp(1, 200);
    let cache = state.cache.clone();

    let before_uid = cache
        .oldest_uid(&account_id, &mailbox)
        .map_err(describe)?
        .ok_or_else(|| "这个文件夹还没有缓存内容，请先刷新一次。".to_string())?;

    let password = password_for(&account_id)?;
    let page = pool_op!(&state.pool, &cfg, &password, |client| {
        client
            .fetch_older_headers(&mailbox, before_uid, limit)
            .await
    })
    .map_err(describe)?;

    if page.is_empty() {
        // The bottom of the mailbox. Not an error: the UI says "no older mail" and drops the button.
        return Ok(HistoryPage {
            messages: Vec::new(),
            done: true,
        });
    }

    cache
        .upsert_summaries(&account_id, &mailbox, &page)
        .map_err(describe)?;

    Ok(HistoryPage {
        messages: page,
        done: false,
    })
}

/// What `load_more_messages` gives the list.
#[derive(serde::Serialize)]
struct HistoryPage {
    messages: Vec<MessageSummary>,
    /// True when there is nothing older on the server, so the button can be disabled for good.
    done: bool,
}

/// Refresh one folder from the server into the cache.
///
/// Best effort by design: a mail client that shows nothing when the network is down is worse than
/// one showing slightly stale mail, which is the whole reason the cache exists. Callers fall back to
/// whatever is on disk and surface [`AppState::last_sync_error`] when it fails.
async fn refresh_folder(
    pool: &ConnectionPool,
    cache: &Cache,
    cfg: &AccountConfig,
    mailbox: &str,
) -> Result<SyncOutcome, String> {
    // Reuse the display name and delimiter we already know; only ask the server for `LIST` when
    // this folder has never been seen, so a normal refresh costs no extra round trip.
    let known = cache.get_folder(&cfg.id, mailbox).map_err(describe)?;

    let info = match known {
        Some(info) => info,
        None => {
            let password = password_for(&cfg.id)?;
            let all = pool_op!(pool, cfg, &password, |client| {
                client.list_mailboxes().await
            })
            .map_err(describe)?;

            let found = all
                .into_iter()
                .find(|m| m.name == mailbox)
                .ok_or_else(|| format!("服务器上找不到文件夹「{mailbox}」"))?;
            // Record it with a zero generation; the sync below reports the real one.
            cache.upsert_folder(&cfg.id, &found, 0).map_err(describe)?;
            found
        }
    };

    let password = password_for(&cfg.id)?;

    // The driver is built *inside* the block, which matters twice over: the block runs more than
    // once for the pool's retry, and rebuilding the driver is what makes a retry safe —
    // `SyncDriver::start` re-reads the cursor, so a half-applied first attempt is simply resumed.
    // Everything here works in `mail_core::Result`, so `?` maps nothing.
    let outcome = pool_op!(pool, cfg, &password, |client| {
        let mut driver = SyncDriver::start(cache, &cfg.id, &info)?;
        loop {
            let page = driver.execute(&mut client).await?;
            if let Some(outcome) = driver.absorb(cache, &page)? {
                break Ok(outcome);
            }
            // A generation change: the folder was discarded and the driver wants a fresh window.
        }
    })
    .map_err(describe)?;

    log_line(&format!(
        "[easy-email] 同步 {}：新增 {} 封，缓存共 {} 封{}",
        mailbox,
        outcome.fetched,
        outcome.total_cached,
        if outcome.uidvalidity_changed {
            "（服务器重新编号，已重建）"
        } else {
            ""
        }
    ));
    Ok(outcome)
}

#[tauri::command]
async fn list_mailboxes(
    state: State<'_, AppState>,
    account_id: String,
) -> Result<Vec<MailboxInfo>, String> {
    let cfg = state.find(&account_id)?;
    let password = password_for(&cfg.id)?;

    let listed = pool_op!(state.pool, &cfg, &password, |client| {
        client.list_mailboxes().await
    });

    match listed {
        Ok(mailboxes) => {
            // Take the opportunity to record the folder list, so an offline start still has one.
            // Every phase that touches the cache opens its own short-lived connection.
            let cache = state.cache.clone();
            for mailbox in &mailboxes {
                let generation = cache
                    .folder_state(&cfg.id, &mailbox.name)
                    .map_err(describe)?
                    .map_or(0, |state| state.uidvalidity);
                cache
                    .upsert_folder(&cfg.id, mailbox, generation)
                    .map_err(describe)?;
            }
            Ok(mailboxes)
        }
        Err(error) => {
            // Offline: serve the folder list from the last successful `LIST`.
            let cached = state
                .cache
                .clone()
                .list_folders(&cfg.id)
                .map_err(describe)?;
            if cached.is_empty() {
                return Err(describe(error));
            }
            log_line(&format!(
                "[easy-email] 离线，使用缓存的 {} 个文件夹",
                cached.len()
            ));
            Ok(cached)
        }
    }
}

#[tauri::command]
async fn list_messages(
    state: State<'_, AppState>,
    account_id: String,
    mailbox: String,
    limit: usize,
) -> Result<Vec<MessageSummary>, String> {
    let cfg = state.find(&account_id)?;

    // Guard against a caller asking for the whole mailbox; the UI pages at 50.
    let limit = limit.clamp(1, 500);

    let sync_failure = refresh_folder(&state.pool, &state.cache, &cfg, &mailbox)
        .await
        .err();
    *state.last_sync_error.lock().expect("sync error mutex") = sync_failure.clone();

    let cached = state
        .cache
        .clone()
        .list_summaries(&account_id, &mailbox, limit as u32, 0)
        .map_err(describe)?;

    match (cached.is_empty(), sync_failure) {
        // Something on disk: show it, even if the refresh failed.
        (false, _) => Ok(cached),
        // Nothing on disk and the refresh failed: there is genuinely nothing to show, so report why
        // rather than rendering an empty inbox as if the folder were empty.
        (true, Some(error)) => Err(error),
        (true, None) => Ok(cached),
    }
}

#[tauri::command]
async fn fetch_message(
    state: State<'_, AppState>,
    account_id: String,
    mailbox: String,
    uid: u32,
) -> Result<MessageDetail, String> {
    let cfg = state.find(&account_id)?;

    // Cache first: a message that has been read once is readable again with no round trip, and
    // remains readable offline. This is also what makes the day-1 case slow and every later one
    // instant.
    if let Some(detail) = state
        .cache
        .clone()
        .get_detail(&account_id, &mailbox, uid)
        .map_err(describe)?
    {
        return Ok(detail);
    }

    let password = password_for(&cfg.id)?;
    // One pooled connection does both commands, so the select and the fetch cannot land on
    // different sessions.
    let detail = pool_op!(state.pool, &cfg, &password, |client| {
        client.select(&mailbox).await?;
        client.fetch_message(uid).await
    })
    .map_err(describe)?;

    state
        .cache
        .clone()
        .put_detail(&account_id, &mailbox, &detail)
        .map_err(describe)?;
    Ok(detail)
}

/// Save (or update) a draft. Called on the composer's autosave timer.
///
/// Goes straight to `user.db`: a draft is the one thing in a mail client that must never be lost to a
/// crash, a power cut or a cache cleanup, which is why it lives in the file that is never deleted.
#[tauri::command]
async fn save_draft(state: State<'_, AppState>, draft: Draft) -> Result<(), String> {
    state.user.save_draft(&draft).map_err(describe)
}

/// Every draft for an account, most recently touched first.
#[tauri::command]
async fn list_drafts(state: State<'_, AppState>, account_id: String) -> Result<Vec<Draft>, String> {
    state.user.list_drafts(&account_id).map_err(describe)
}

/// Discard a draft. Permanent, and it says whether anything was actually removed.
#[tauri::command]
async fn delete_draft(state: State<'_, AppState>, id: String) -> Result<bool, String> {
    state.user.delete_draft(&id).map_err(describe)
}

/// Walk backwards until the **chosen sync window** is satisfied, and report where it stopped.
///
/// This is what makes the window a setting rather than a stored preference: the button (or a future
/// scheduled job) calls this, and `Years(2)` stops at the date boundary while `Count(n)` stops once
/// `n` messages are cached. The walk is bounded, so one click can never become an open-ended download;
/// the daily byte budget (`design/04` §5.7) will pace the automatic version.
#[tauri::command]
async fn backfill_to_window(
    state: State<'_, AppState>,
    account_id: String,
    mailbox: String,
) -> Result<BackfillReport, String> {
    let cfg = state.find(&account_id)?;
    let window = state.user.sync_window().map_err(describe)?;
    let cache = state.cache.clone();
    let password = password_for(&account_id)?;

    let report = pool_op!(&state.pool, &cfg, &password, |client| {
        backfill_older(
            &mut client,
            &cache,
            &account_id,
            &mailbox,
            window,
            BackfillBudget::generous(),
            now_seconds(),
        )
        .await
    })
    .map_err(describe)?;

    log_line(&format!(
        "[easy-email] 回填到窗口边界 {}：{} 页 / {} 封（{:?}）",
        window.as_key(),
        report.pages,
        report.fetched,
        report.stop
    ));
    Ok(report)
}

/// Seconds since the epoch, for the window's date boundary.
fn now_seconds() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

// ---------------------------------------------------------------- outbox

/// Default undo window: how long a sent message can still be called back.
///
/// Five seconds is long enough to catch the wrong recipient and short enough not to feel like the
/// app is sitting on the message (`design/07` §5.2).
const DEFAULT_UNDO_SECONDS: u32 = 5;

/// Longest undo window the UI may ask for. The design's range is 0–30 s; a bigger number would be a
/// scheduling feature, not an undo.
const MAX_UNDO_SECONDS: u32 = 30;

/// How often the worker looks at the queue when nothing wakes it.
const OUTBOX_TICK: std::time::Duration = std::time::Duration::from_secs(5);

/// How many messages one account may attempt per pass.
const OUTBOX_PASS_LIMIT: usize = 5;

/// Queue a message instead of sending it inline.
///
/// Everything the send needs is frozen here — the built bytes and the `Message-ID` — because
/// `design/07` §5.3 puts the build at enqueue time: a retry, a restart or a rebuilt spool file must
/// not be able to produce a different message than the one the user approved. With `undo_seconds`
/// at its default the worker will not touch the row for five seconds, which is what makes 撤销 mean
/// "nothing ever happened" rather than "we sent it and then apologised".
#[tauri::command]
async fn enqueue_send(
    state: State<'_, AppState>,
    account_id: String,
    mut mail: OutgoingMail,
    draft_id: Option<String>,
    undo_seconds: Option<u32>,
) -> Result<i64, String> {
    let cfg = state.find(&account_id)?;

    // Freeze the identity before anything else: lettre generates a fresh Message-ID per build, so
    // rebuilding later would silently produce a *different* message as far as every client is
    // concerned — and crash recovery would have nothing to search the Sent folder for.
    let message = smtp::build_message(&cfg, &mail).map_err(describe)?;
    if mail.message_id.is_none() {
        mail.message_id = smtp::message_id_of(&message);
    }
    let bytes = message.formatted();

    if mail.to.is_empty() && mail.cc.is_empty() && mail.bcc.is_empty() {
        return Err("请至少填写一个收件人。".to_string());
    }

    let now = now_seconds();
    let undo = i64::from(
        undo_seconds
            .unwrap_or(DEFAULT_UNDO_SECONDS)
            .min(MAX_UNDO_SECONDS),
    );
    let id = state
        .user
        .enqueue_outbox(&NewOutbox {
            account_id: account_id.clone(),
            draft_id,
            payload_json: serde_json::to_string(&mail)
                .map_err(|e| format!("发件内容无法序列化：{e}"))?,
            spool_path: None,
            spool_bytes: 0,
            undo_until: now + undo,
            scheduled_at: 0,
            created_at: now,
        })
        .map_err(describe)?;

    // The row id names the spool file, so this has to come after the insert. A failure here leaves a
    // queued row with no bytes; the worker rebuilds it from the payload and records the new path.
    match state.spool.write(id, &bytes) {
        Ok((path, size)) => {
            if let Err(error) =
                state
                    .user
                    .set_outbox_spool(id, &path.to_string_lossy(), size as i64)
            {
                log_line(&format!("[easy-email] 发件暂存路径未记录：{error}"));
            }
        }
        Err(error) => log_line(&format!("[easy-email] 发件暂存写入失败：{error}")),
    }

    state.wake_outbox();
    Ok(id)
}

/// Everything in the outbox, newest first. `account_id` narrows it to one account.
#[tauri::command]
async fn list_outbox(
    state: State<'_, AppState>,
    account_id: Option<String>,
) -> Result<Vec<OutboxItem>, String> {
    state
        .user
        .list_outbox(account_id.as_deref())
        .map_err(describe)
}

/// Call a queued send back. False means it was already on its way — the UI says so rather than
/// pretending the message was stopped (`design/07` §5.2).
#[tauri::command]
async fn cancel_send(state: State<'_, AppState>, id: i64) -> Result<bool, String> {
    let cancelled = state.user.cancel_outbox(id).map_err(describe)?;
    if cancelled {
        // The spool is no longer needed, but the draft stays: cancelling must leave the user with
        // what they wrote (design/07 §5.2).
        if let Ok(Some(item)) = state.user.outbox_item(id)
            && let Some(path) = item.spool_path.as_deref()
        {
            state.spool.remove(std::path::Path::new(path));
        }
    }
    state.wake_outbox();
    Ok(cancelled)
}

/// Put a failed or cancelled message back in the queue and try now.
#[tauri::command]
async fn retry_send(state: State<'_, AppState>, id: i64) -> Result<bool, String> {
    let queued = state.user.requeue_outbox(id).map_err(describe)?;
    state.wake_outbox();
    Ok(queued)
}

/// Drop an outbox row, and its spool file with it. The message is not recoverable afterwards.
#[tauri::command]
async fn discard_outbox(state: State<'_, AppState>, id: i64) -> Result<bool, String> {
    let path = state
        .user
        .outbox_item(id)
        .map_err(describe)?
        .and_then(|item| item.spool_path);
    let removed = state.user.delete_outbox(id).map_err(describe)?;
    if removed && let Some(path) = path {
        state.spool.remove(std::path::Path::new(&path));
    }
    Ok(removed)
}

/// Run one pass of the outbox worker for every configured account.
///
/// One account at a time on purpose: the connection budget in `design/04` is per account, and a
/// mailbox that is being rate-limited should not be sent two connections' worth of work at once.
async fn outbox_pass(app: &AppHandle) -> Result<RunReport, String> {
    let (accounts, store, spool) = {
        let state = app.state::<AppState>();
        (state.snapshot(), state.user.clone(), state.spool.clone())
    };

    let mut total = RunReport::default();
    for account in accounts {
        // No credential means no send, and the row must stay queued rather than be marked failed:
        // the user may simply not have typed the authorization code yet.
        let password = match secret::load_password(&account.id) {
            Ok(Some(password)) => password,
            _ => continue,
        };

        let report = run_due(
            OutboxRun {
                store: &store,
                spool: &spool,
                cfg: &account,
                password: &password,
                now: now_seconds(),
                limit: OUTBOX_PASS_LIMIT,
            },
            |cfg, password, bytes, recipients| async move {
                smtp::send_spool(&cfg, &password, bytes, recipients).await
            },
        )
        .await
        .map_err(describe)?;

        total.attempted += report.attempted;
        total.sent += report.sent;
        total.retried += report.retried;
        total.gave_up += report.gave_up;
        total.needs_attention += report.needs_attention;
    }

    Ok(total)
}

/// Keep the queue moving for as long as the app runs.
///
/// A tick plus a wake-up, because the interesting moments are not periodic: a message just queued,
/// an undo window that just expired, a user pressing "重试". Without the wake-ups those would each
/// wait up to five seconds, which is exactly the delay `design/07` §5.3 calls out.
fn start_outbox_worker(app: AppHandle) {
    tauri::async_runtime::spawn(async move {
        // `design/07` §5.3.1, before anything is sent in this run: settle whatever the previous run
        // left mid-flight. It runs here rather than in `setup` so the window never waits on a network
        // round trip — and it runs before the first pass, so a row that turns out to have been
        // delivered already can never be sent a second time.
        match recover_outbox(&app).await {
            Ok(report) if report.already_sent + report.requeued + report.asked_user > 0 => {
                log_line(&format!(
                    "[easy-email] 崩溃恢复：已发出 {} / 重新排队 {} / 待用户确认 {} / 未决 {}",
                    report.already_sent, report.requeued, report.asked_user, report.unresolved
                ));
                let _ = app.emit("outbox:changed", ());
            }
            Ok(_) => {}
            Err(error) => log_line(&format!("[easy-email] 崩溃恢复失败：{error}")),
        }

        loop {
            let wake = {
                let state = app.state::<AppState>();
                state.outbox_wake.clone()
            };
            tokio::select! {
                _ = tokio::time::sleep(OUTBOX_TICK) => {}
                _ = wake.notified() => {}
            }

            match outbox_pass(&app).await {
                Ok(report) => {
                    if report.attempted > 0 {
                        log_line(&format!(
                            "[easy-email] 发件箱：本轮尝试 {} 封（成功 {} / 重试 {} / 放弃 {} / 待处理 {}）",
                            report.attempted,
                            report.sent,
                            report.retried,
                            report.gave_up,
                            report.needs_attention
                        ));
                    }
                    if report.sent > 0
                        || report.retried > 0
                        || report.gave_up > 0
                        || report.needs_attention > 0
                    {
                        // Tell the window, which owns what is on screen. Quiet when the pass only
                        // looked at rows it could not use.
                        let _ = app.emit("outbox:changed", ());
                    }
                }
                Err(error) => log_line(&format!("[easy-email] 发件箱：{error}")),
            }
        }
    });
}

/// Messages larger than this wait for the user after a crash instead of being re-sent unattended
/// (`design/07` §5.3.1).
const CRASH_RESEND_LIMIT: i64 = mail_core::DEFAULT_CRASH_RESEND_LIMIT;

/// Answer "did this already go out?" for the rows a crash left in flight.
///
/// The question goes to the **Sent folder**, by the `Message-ID` frozen into the spool at enqueue
/// time — the same identity the archive step looks for. An account with nothing in flight is skipped
/// entirely: this must not cost one connection per account on every startup.
async fn recover_outbox(app: &AppHandle) -> Result<RecoveryReport, String> {
    let (accounts, store, spool, pool) = {
        let state = app.state::<AppState>();
        (
            state.snapshot(),
            state.user.clone(),
            state.spool.clone(),
            state.pool.clone(),
        )
    };

    let mut total = RecoveryReport::default();
    for account in accounts {
        let in_flight = store
            .outbox_in_flight()
            .map_err(describe)?
            .into_iter()
            .any(|item| item.account_id == account.id);
        if !in_flight {
            continue;
        }

        // Without the credential we cannot ask, and the rows stay exactly as they are for the next
        // attempt — including one that runs after the user re-enters their authorization code.
        let password = match secret::load_password(&account.id) {
            Ok(Some(password)) => password,
            _ => continue,
        };

        let report = recover_in_flight(
            RecoveryRun {
                store: &store,
                spool: &spool,
                now: now_seconds(),
                max_auto_bytes: CRASH_RESEND_LIMIT,
            },
            |message_id| {
                let pool = pool.clone();
                let cfg = account.clone();
                let password = password.clone();
                async move { sent_folder_has(&pool, &cfg, &password, &message_id).await }
            },
        )
        .await
        .map_err(describe)?;

        total.already_sent += report.already_sent;
        total.requeued += report.requeued;
        total.asked_user += report.asked_user;
        total.unresolved += report.unresolved;
    }

    Ok(total)
}

/// Whether the account's Sent folder already holds this `Message-ID`.
///
/// A server with no Sent folder answers "no": there is nowhere a copy could have landed, so there is
/// nothing that could be duplicated.
async fn sent_folder_has(
    pool: &ConnectionPool,
    cfg: &AccountConfig,
    password: &str,
    message_id: &str,
) -> mail_core::Result<bool> {
    pool_op!(pool, cfg, password, |client| {
        match client.sent_mailbox().await? {
            Some(folder) => Ok(client
                .find_uid_by_message_id(&folder, message_id)
                .await?
                .is_some()),
            None => Ok(false),
        }
    })
}

/// The sync window the user chose (how far back the client keeps mail).
#[tauri::command]
async fn get_sync_window(state: State<'_, AppState>) -> Result<SyncWindow, String> {
    state.user.sync_window().map_err(describe)
}

/// Remember a new sync window. The next sync honours it; nothing already cached is deleted.
///
/// Deliberately **not** destructive: narrowing the window stops future backfill, it does not wipe
/// mail the user may still be reading. Reclaiming space is a separate, explicit action
/// (`design/03` §5.4).
#[tauri::command]
async fn set_sync_window(state: State<'_, AppState>, window: SyncWindow) -> Result<(), String> {
    state.user.set_sync_window(window).map_err(describe)?;
    log_line(&format!("[easy-email] 同步窗口改为 {}", window.as_key()));
    Ok(())
}

/// Why the last server refresh failed, and how much is cached. Drives the "offline" notice.
#[tauri::command]
async fn sync_status(state: State<'_, AppState>, account_id: String) -> Result<SyncStatus, String> {
    let last_error = state
        .last_sync_error
        .lock()
        .expect("sync error mutex")
        .clone();
    let cached_total = {
        let cache = state.cache.clone();
        let folders = cache.list_folders(&account_id).map_err(describe)?;
        let mut total = 0;
        for folder in folders {
            total += cache
                .summary_count(&account_id, &folder.name)
                .map_err(describe)?;
        }
        total
    };
    // Counts from the user database: the UI shows "drafts (2)" and "waiting to send (1)", and both
    // have to survive a restart — which is exactly what this file is for.
    let user = state.user.clone();
    let drafts = user.draft_count().map_err(describe)?;
    let pending = user.pending_op_count().map_err(describe)?;

    Ok(SyncStatus {
        offline: last_error.is_some(),
        last_error,
        cached_total,
        drafts,
        pending_ops: pending,
    })
}

/// Search the local cache. Never touches the network, so it also works offline.
#[tauri::command]
async fn search_messages(
    state: State<'_, AppState>,
    account_id: String,
    query: String,
    limit: usize,
) -> Result<Vec<SearchHit>, String> {
    let limit = limit.clamp(1, 200) as u32;
    let hits = state
        .cache
        .clone()
        .search(&account_id, &query, limit)
        .map_err(describe)?;
    log_line(&format!(
        "[easy-email] 本地搜索 {query:?}：命中 {} 条",
        hits.len()
    ));
    Ok(hits)
}

#[derive(serde::Serialize)]
struct SyncStatus {
    offline: bool,
    last_error: Option<String>,
    cached_total: u32,
    /// Drafts on disk (they are the reason the user database exists).
    drafts: u32,
    /// Offline actions still waiting to reach the server.
    pending_ops: u32,
}

#[tauri::command]
async fn mark_seen(
    state: State<'_, AppState>,
    account_id: String,
    mailbox: String,
    uid: u32,
    seen: bool,
) -> Result<(), String> {
    let cfg = state.find(&account_id)?;
    let password = password_for(&cfg.id)?;

    pool_op!(state.pool, &cfg, &password, |client| {
        client.select(&mailbox).await?;
        client.mark_seen(uid, seen).await
    })
    .map_err(describe)?;

    // Mirror it locally, or the cached list keeps showing the old flag next time it is opened
    // offline and the UI disagrees with the server.
    state
        .cache
        .clone()
        .set_seen(&account_id, &mailbox, uid, seen)
        .map_err(describe)?;
    Ok(())
}

#[tauri::command]
async fn delete_message(
    state: State<'_, AppState>,
    account_id: String,
    mailbox: String,
    uid: u32,
) -> Result<(), String> {
    let cfg = state.find(&account_id)?;
    let password = password_for(&cfg.id)?;

    pool_op!(state.pool, &cfg, &password, |client| {
        client.delete_message(&mailbox, uid).await
    })
    .map_err(describe)?;

    // Drop it from the cache as well, so a deleted message cannot reappear from disk.
    state
        .cache
        .clone()
        .delete_message(&account_id, &mailbox, uid)
        .map_err(describe)?;
    Ok(())
}

#[tauri::command]
async fn move_message(
    state: State<'_, AppState>,
    account_id: String,
    mailbox: String,
    uid: u32,
    destination: String,
) -> Result<(), String> {
    let cfg = state.find(&account_id)?;
    let password = password_for(&cfg.id)?;

    pool_op!(state.pool, &cfg, &password, |client| {
        client.move_message(&mailbox, uid, &destination).await
    })
    .map_err(describe)?;
    Ok(())
}

// ---------------------------------------------------------------- push (IDLE)

/// Start watching one folder for this account. Replaces any watcher already running for it.
///
/// The work happens in two background tasks: an IDLE *pump* that stays connected and reports
/// changes, and a *handler* that turns a change into a sync, a cache write, a UI event, a
/// notification and a tray update. They are kept separate so the pump never blocks on a sync — the
/// point of IDLE is that its connection is otherwise idle.
/// Start watching one folder for this account. Replaces any watcher already running for it.
///
/// A plain function rather than only a command, because the same thing has to happen at startup for
/// accounts that are already configured — otherwise push would only begin after the user clicked
/// something.
fn start_watcher(
    app: &AppHandle,
    state: &AppState,
    cfg: AccountConfig,
    mailbox: String,
) -> Result<(), String> {
    let password = password_for(&cfg.id)?;
    let pool = Arc::clone(&state.pool);
    let cache = state.cache.clone();
    let account_id = cfg.id.clone();

    stop_watcher(state, &account_id);

    let (tx, mut rx) = tokio::sync::mpsc::channel::<PushEvent>(16);

    let pump_cfg = cfg.clone();
    let pump_mailbox = mailbox.clone();
    // `tauri::async_runtime::spawn`, not `tokio::spawn`: this is also called from `setup`, which
    // runs on the main thread *outside* any Tokio runtime context, and `tokio::spawn` panics there
    // with "there is no reactor running" — which is a crash before the window is even usable.
    let pump = tauri::async_runtime::spawn(async move {
        // `watch` only returns when the receiver is dropped, i.e. when the handler stops.
        watch(pump_cfg, password, pump_mailbox, tx).await;
    });

    let app = app.clone();
    let handler = tauri::async_runtime::spawn(async move {
        while let Some(event) = rx.recv().await {
            match event {
                PushEvent::Changed { mailbox } => {
                    // Sync on a *different* pooled connection; the IDLE connection stays idle.
                    match refresh_folder(&pool, &cache, &cfg, &mailbox).await {
                        Ok(outcome) if outcome.fetched > 0 => {
                            announce(&app, &cfg, &mailbox, &cache, &outcome);
                        }
                        Ok(_) => {}
                        Err(error) => {
                            log_line(&format!("[easy-email] 推送后同步 {mailbox} 失败：{error}"));
                        }
                    }
                }
                PushEvent::Reconnected { mailbox, attempt } => {
                    log_line(&format!(
                        "[easy-email] {mailbox} 的推送连接已重连（第 {attempt} 次尝试）"
                    ));
                }
                PushEvent::Failed { reason } => {
                    log_line(&format!("[easy-email] 推送连接失败：{reason}"));
                }
            }
        }
    });

    state
        .watchers
        .lock()
        .expect("watcher mutex")
        .insert(account_id.clone(), Watcher { pump, handler });

    log_line(&format!(
        "[easy-email] 已开始监听 {account_id} 的 {mailbox}"
    ));
    Ok(())
}

#[tauri::command]
async fn watch_inbox(
    state: State<'_, AppState>,
    app: AppHandle,
    account_id: String,
    mailbox: String,
) -> Result<(), String> {
    let cfg = state.find(&account_id)?;
    start_watcher(&app, state.inner(), cfg, mailbox)
}

/// Bring the main window back from the tray.
///
/// `set_focus` alone is not enough when the window is minimised, so it is shown first.
fn reveal_window(app: &AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.show();
        let _ = window.unminimize();
        let _ = window.set_focus();
    }
}

/// Stop watching an account, if anything is running for it.
#[tauri::command]
async fn stop_watching(state: State<'_, AppState>, account_id: String) -> Result<(), String> {
    stop_watcher(state.inner(), &account_id);
    Ok(())
}

fn stop_watcher(state: &AppState, account_id: &str) {
    let existing = state
        .watchers
        .lock()
        .expect("watcher mutex")
        .remove(account_id);
    if let Some(watcher) = existing {
        // Both, explicitly: aborting only the handler would leave the IDLE task holding a connection
        // until its next idle window ended, which is most of a minute.
        watcher.handler.abort();
        watcher.pump.abort();
        log_line(&format!("[easy-email] 已停止监听 {account_id}"));
    }
}

/// Tell everything that cares about new mail: the window, Windows, and the tray.
fn announce(
    app: &AppHandle,
    cfg: &AccountConfig,
    mailbox: &str,
    cache: &Cache,
    outcome: &SyncOutcome,
) {
    let unread = cache.unread_count(&cfg.id).unwrap_or(0);

    log_line(&format!(
        "[easy-email] 推送：{} 的 {mailbox} 新增 {} 封（未读 {}）",
        cfg.id, outcome.fetched, unread
    ));

    // The window: the frontend decides whether it is looking at this folder.
    let payload = MailUpdate {
        account_id: cfg.id.clone(),
        mailbox: mailbox.to_string(),
        new_messages: outcome.fetched,
        total_cached: outcome.total_cached,
        unread,
    };
    if let Err(error) = app.emit("mail:new", payload) {
        log_line(&format!("[easy-email] 无法把事件发给界面：{error}"));
    }

    // Windows: a toast, because the whole point of a desktop client is knowing without looking.
    // Called from Rust rather than the frontend — the frontend has no bundler, so the plugin's JS
    // package cannot be imported.
    let body = if outcome.fetched == 1 {
        format!("新邮件 1 封 · 未读 {unread}")
    } else {
        format!("新邮件 {} 封 · 未读 {unread}", outcome.fetched)
    };
    if let Err(error) = app
        .notification()
        .builder()
        .title(format!("{} · {}", cfg.from_display(), mailbox))
        .body(body)
        .show()
    {
        // A refused toast (focus assist, missing permission) must not break the sync path.
        log_line(&format!("[easy-email] 系统通知未发送：{error}"));
    }

    if let Some(tray) = app.tray_by_id("main") {
        let _ = tray.set_tooltip(Some(format!("Easy Email · 未读 {unread}")));
    }
}

// ---------------------------------------------------------------- attachments

/// Ask the user which files to attach, and report what they chose.
///
/// The bytes are deliberately not read here: `OutgoingAttachment` carries the path and `smtp` reads
/// the file when it builds the message, so a large attachment never becomes a JSON string across
/// the IPC boundary. The size is included so the composer can show it.
///
/// The plugin's callback API is bridged to an await rather than using `blocking_pick_files`, so the
/// command does not tie up a runtime thread while the picker is open.
#[tauri::command]
async fn pick_attachments(app: AppHandle) -> Result<Vec<OutgoingAttachment>, String> {
    let (tx, rx) = tokio::sync::oneshot::channel();

    app.dialog()
        .file()
        .set_title("选择要附加的文件")
        .pick_files(move |paths| {
            let _ = tx.send(paths);
        });

    let picked = rx.await.map_err(|_| "文件选择被中断".to_string())?;
    // `None` is the user closing the dialog, which is not an error.
    let Some(paths) = picked else {
        return Ok(Vec::new());
    };

    let mut chosen = Vec::new();
    for file in paths {
        let path = file
            .into_path()
            .map_err(|e| format!("无法解析所选文件：{e}"))?;
        let metadata =
            std::fs::metadata(&path).map_err(|e| format!("读不到 {}：{e}", path.display()))?;
        if !metadata.is_file() {
            continue;
        }

        chosen.push(OutgoingAttachment {
            name: path
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("附件")
                .to_string(),
            mime: guess_mime(&path),
            size: metadata.len(),
            path: path.to_string_lossy().to_string(),
        });
    }

    log_line(&format!("[easy-email] 选择了 {} 个附件", chosen.len()));
    Ok(chosen)
}

/// Guess a MIME type from the extension.
///
/// Only the common ones: the fallback (`application/octet-stream`) is always valid, and a wrong
/// guess is worse than a generic one — a file mislabelled as text would be mangled in transit.
fn guess_mime(path: &Path) -> String {
    let extension = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();

    let mime = match extension.as_str() {
        "pdf" => "application/pdf",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "svg" => "image/svg+xml",
        "txt" | "log" | "md" => "text/plain",
        "csv" => "text/csv",
        "html" | "htm" => "text/html",
        "json" => "application/json",
        "xml" => "application/xml",
        "zip" => "application/zip",
        "gz" | "tgz" => "application/gzip",
        "7z" => "application/x-7z-compressed",
        "rar" => "application/vnd.rar",
        "doc" => "application/msword",
        "docx" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        "xls" => "application/vnd.ms-excel",
        "xlsx" => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        "ppt" => "application/vnd.ms-powerpoint",
        "pptx" => "application/vnd.openxmlformats-officedocument.presentationml.presentation",
        "mp3" => "audio/mpeg",
        "mp4" => "video/mp4",
        "eml" => "message/rfc822",
        _ => "application/octet-stream",
    };
    mime.to_string()
}

/// Save one attachment into the user's Downloads folder and return the full path.
///
/// **The name comes from the message, so it is hostile input.** It is reduced to a single path
/// component and stripped of everything Windows treats specially, because `..\..\Startup\x.exe` is a
/// perfectly legal `filename=` value for a sender to choose. A name that sanitises away entirely
/// falls back to something derived from the part number rather than to an empty path.
#[tauri::command]
async fn save_attachment(
    state: State<'_, AppState>,
    account_id: String,
    mailbox: String,
    uid: u32,
    part: u32,
    name: String,
) -> Result<String, String> {
    let cfg = state.find(&account_id)?;
    let password = password_for(&cfg.id)?;

    let directory = state
        .download_dir
        .join("Easy Email")
        .to_string_lossy()
        .to_string();
    let directory = PathBuf::from(directory);

    // `unique_path` rather than a fixed name: silently overwriting whatever is already there would
    // lose a file the user may not have looked at yet.
    let path = unique_path(&directory, &safe_file_name(&name, part));

    let written = pool_op!(state.pool, &cfg, &password, |client| {
        client.select(&mailbox).await?;
        client.save_attachment(uid, part, &path).await
    })
    .map_err(describe)?;

    let shown = path.to_string_lossy().to_string();
    log_line(&format!(
        "[easy-email] 已保存附件 {shown}（{written} 字节）"
    ));
    Ok(shown)
}

/// Reduce a message-supplied filename to a single, safe path component.
///
/// Rejects directory traversal by taking the last component, and replaces the characters Windows
/// refuses plus every control character. Reserved device names (`CON`, `NUL`, …) get a prefix,
/// because creating them fails in ways that look like a bug in this app.
fn safe_file_name(raw: &str, part: u32) -> String {
    // Only the final component, so `../../evil` cannot escape the directory.
    let leaf = raw
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or("")
        .trim()
        .trim_matches('.');

    let mut cleaned: String = leaf
        .chars()
        .map(|c| {
            if c.is_control() || matches!(c, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*') {
                '_'
            } else {
                c
            }
        })
        .collect();

    // Long names are a real hazard on Windows, and the extension is the part worth keeping.
    const MAX: usize = 120;
    if cleaned.chars().count() > MAX {
        let keep: String = cleaned.chars().take(MAX).collect();
        cleaned = keep;
    }

    let upper = cleaned.to_ascii_uppercase();
    let reserved = [
        "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "LPT1", "LPT2",
    ];
    // Compare the stem, since `NUL.txt` is just as unusable as `NUL`.
    let stem = upper.split('.').next().unwrap_or("");
    if reserved.contains(&stem) {
        cleaned = format!("file-{cleaned}");
    }

    if cleaned.trim().is_empty() {
        format!("attachment-{part}")
    } else {
        cleaned
    }
}

/// `directory/name`, or `directory/name (2)` and so on if that already exists.
fn unique_path(directory: &Path, name: &str) -> PathBuf {
    let candidate = directory.join(name);
    if !candidate.exists() {
        return candidate;
    }

    let path = Path::new(name);
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("attachment");
    let extension = path.extension().and_then(|s| s.to_str());

    for index in 2..1000 {
        let candidate = match extension {
            Some(extension) => directory.join(format!("{stem} ({index}).{extension}")),
            None => directory.join(format!("{stem} ({index})")),
        };
        if !candidate.exists() {
            return candidate;
        }
    }
    candidate
}

// ---------------------------------------------------------------- send
//
// There is deliberately no "send this now, synchronously" command any more. `design/07` §5.1 has
// exactly one send path — validate → freeze the bytes → enqueue with an undo window — and a second
// entry point that went straight to SMTP would silently skip the undo window, the retry/backoff
// policy and the "did it already go out?" recovery. `smtp::send` still exists for the tests that
// compare the spool path against a straightforward send.

/// Directory that `startup-error.log` is written to.
///
/// Filled in from `setup`, which is the first point where Tauri can resolve the real app config
/// directory — so the `com.easyemail.desktop` identifier is not duplicated here by hand.
static PANIC_LOG_DIR: OnceLock<PathBuf> = OnceLock::new();

/// Append one diagnostic line to `startup-error.log`.
///
/// The desktop build has no console (see the `windows_subsystem` note in `main.rs`), so anything
/// that used to be an `eprintln!` would otherwise disappear without a trace. Falls back to stderr,
/// which is visible when the app is started with `--console`.
fn log_line(message: &str) {
    let seconds = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0);
    let line = format!("[{seconds}] {message}\n");

    let stored = PANIC_LOG_DIR.get().is_some_and(|dir| {
        std::fs::create_dir_all(dir).is_ok()
            && std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(dir.join("startup-error.log"))
                .and_then(|mut file| file.write_all(line.as_bytes()))
                .is_ok()
    });

    if !stored {
        eprint!("{line}");
    }
}

/// Route panics into the same log.
///
/// A GUI app whose window opens blank and prints nothing is undebuggable; this is what replaced the
/// console. Installed before the Tauri builder so it also covers panics raised inside command
/// handlers, which is the realistic case.
fn install_panic_log() {
    std::panic::set_hook(Box::new(|info| {
        let location = info
            .location()
            .map(|place| format!("{}:{}", place.file(), place.line()))
            .unwrap_or_else(|| "<unknown location>".to_string());
        let payload = info
            .payload()
            .downcast_ref::<&str>()
            .map(|text| (*text).to_string())
            .or_else(|| info.payload().downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "(non-string panic payload)".to_string());
        log_line(&format!("panic at {location}: {payload}"));
    }));
}

/// Entry point used by `main.rs`.
pub fn run() {
    install_panic_log();

    tauri::Builder::default()
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_dialog::init())
        .setup(|app| {
            // Fit the window to the monitor.
            //
            // The configured default (1200x780 logical) assumes a normal desktop, but the
            // *logical* work area can be much smaller than that on a high-DPI panel, so Windows
            // clamps the window and the three panes end up fighting over the leftovers.
            //
            // Deliberately computed from the monitor's physical size rather than from
            // `scale_factor()`: on this machine the monitor reports 1.5x while the webview
            // renders at 1.935x, so multiplying a logical size by the scale factor produces a
            // window noticeably smaller than intended. A fraction of the real pixel size is
            // correct regardless of which scale is in play.
            if let Some(window) = app.get_webview_window("main") {
                let monitor = window
                    .current_monitor()
                    .ok()
                    .flatten()
                    .or_else(|| window.primary_monitor().ok().flatten());

                if let Some(monitor) = monitor {
                    let physical = monitor.size();
                    let width = (f64::from(physical.width) * 0.92).min(1920.0);
                    let height = (f64::from(physical.height) * 0.92).min(1200.0);
                    let _ = window.set_size(tauri::PhysicalSize::new(
                        width.round() as u32,
                        height.round() as u32,
                    ));
                    let _ = window.center();
                }
            }

            let dir = app.path().app_config_dir()?;
            let _ = PANIC_LOG_DIR.set(dir.clone());
            let accounts_path = dir.join("accounts.json");
            // The cache lives next to accounts.json rather than in a directory of its own, because
            // both belong to this installation and both should be equally easy to find and delete.
            let cache_path = dir.join("cache.db");
            // Resolved here rather than per command: it cannot change while the app runs, and the
            // resolver is only available where the app handle is.
            let download_dir = app.path().download_dir().unwrap_or_else(|_| dir.clone());

            let accounts = match store::load(&accounts_path) {
                Ok(accounts) => accounts,
                // A corrupt file has already been moved aside; carry on with an empty list so the
                // window still opens and the user can re-add their account.
                Err(e) => {
                    log_line(&format!("[easy-email] {e}"));
                    Vec::new()
                }
            };

            // Open the cache **once** and keep the handle. It owns a writer thread and a read pool,
            // and it is `Clone + Send + Sync`, so `async` commands share it instead of opening a
            // connection each (v0.2's shape: one connection per command, one implicit transaction per
            // row).
            //
            // A failure here must not stop the app: a mail client that refuses to start over its own
            // scratch data is worse than one running without a cache. The commands that need it
            // report the failure themselves.
            let cache = match Cache::open(&cache_path) {
                Ok(cache) => cache,
                Err(e) => {
                    log_line(&format!("[easy-email] 本地缓存不可用：{e}"));
                    // Fall back to an in-memory cache: the app runs, nothing persists, and every
                    // command that reads mail reports "no cached mail" rather than hanging.
                    Cache::open_in_memory().expect("in-memory cache must be available")
                }
            };

            // A cache created before the auto-vacuum pragma cannot reclaim space: SQLite only
            // accepts the setting on an empty database, so "cleanup freed 200 MB" would be a lie on
            // disk. Detect it and rebuild **in the background** — a full `VACUUM` rewrites the file,
            // which must not happen before the window opens (and the design asks for it to be
            // explicit rather than hidden; the result is logged either way).
            if cache.needs_auto_vacuum_rebuild().unwrap_or(false) {
                let rebuild = cache.clone();
                std::thread::spawn(move || {
                    // `std::thread`, not `tokio::spawn`: this can run before the runtime exists, and
                    // `Db::exec_blocking` parks this thread rather than a runtime worker.
                    match rebuild.rebuild_with_auto_vacuum() {
                        Ok(()) => log_line("[easy-email] 已为老缓存启用 incremental auto-vacuum"),
                        Err(e) => log_line(&format!("[easy-email] 老缓存重建失败：{e}")),
                    }
                });
            }

            // `user.db` lives beside the cache but is **not** part of it: drafts, queued sends and
            // offline actions cannot be re-fetched, so they must survive "delete the cache". Same
            // rule as above: a failure must not stop the window from opening.
            let user_path = dir.join("user.db");
            let user = match UserStore::open(&user_path) {
                Ok(store) => store,
                Err(e) => {
                    log_line(&format!("[easy-email] 用户数据不可用：{e}"));
                    UserStore::open_in_memory().expect("in-memory user store must be available")
                }
            };

            // The send spool. Same rule once more: if it cannot be created, the app still starts —
            // the worker rebuilds a missing spool file from the stored payload before sending.
            let spool = match Spool::open(dir.join("spool")) {
                Ok(spool) => spool,
                Err(e) => {
                    log_line(&format!("[easy-email] 发件暂存目录不可用：{e}"));
                    Spool::open(std::env::temp_dir().join("easy-email-spool"))
                        .expect("the temp directory must be usable")
                }
            };

            app.manage(AppState {
                accounts_path,
                accounts: Mutex::new(accounts),
                cache,
                last_sync_error: Mutex::new(None),
                pool: Arc::new(ConnectionPool::new()),
                watchers: Mutex::new(HashMap::new()),
                download_dir,
                user,
                // Its own directory under the app's config dir, next to the two databases: a spool
                // file is disposable, but it must survive a restart to keep a queued send's bytes.
                spool,
                outbox_wake: Arc::new(Notify::new()),
            });

            // Start the outbox worker. Queued messages outlive the window, so this is what actually
            // sends them: the composer only commits them to `user.db`.
            start_outbox_worker(app.handle().clone());

            // The tray icon. Built here rather than in config so the menu can drive the app.
            let show = MenuItem::with_id(app, "show", "打开主窗口", true, None::<&str>)?;
            let check = MenuItem::with_id(app, "check", "立即检查新邮件", true, None::<&str>)?;
            let quit = MenuItem::with_id(app, "quit", "退出", true, None::<&str>)?;
            let menu = Menu::with_items(app, &[&show, &check, &quit])?;

            let tray = TrayIconBuilder::with_id("main")
                .icon(tauri::image::Image::from_bytes(include_bytes!(
                    "../icons/icon.png"
                ))?)
                .tooltip("Easy Email")
                .menu(&menu)
                .show_menu_on_left_click(false)
                .on_menu_event(|app, event| match event.id().as_ref() {
                    "show" => reveal_window(app),
                    // Ask the window to refresh; the frontend owns which folder is on screen.
                    "check" => {
                        reveal_window(app);
                        let _ = app.emit("mail:check", ());
                    }
                    "quit" => app.exit(0),
                    _ => {}
                })
                .on_tray_icon_event(|tray, event| {
                    // Left click means "show me", which is what every tray icon on Windows does.
                    if let TrayIconEvent::Click {
                        button: MouseButton::Left,
                        button_state: MouseButtonState::Up,
                        ..
                    } = event
                    {
                        reveal_window(tray.app_handle());
                    }
                })
                .build(app)?;
            let _ = &tray;

            // Start the watchers for accounts that are already configured, so push works from the
            // first moment rather than only after the user touches an account.
            {
                let state = app.state::<AppState>();
                for account in state.snapshot() {
                    let id = account.id.clone();
                    if let Err(error) =
                        start_watcher(app.handle(), state.inner(), account, "INBOX".into())
                    {
                        log_line(&format!("[easy-email] 未能启动 {id} 的推送：{error}"));
                    }
                }
            }
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            list_providers,
            suggest_provider,
            build_account,
            list_accounts,
            save_account,
            delete_account,
            verify_account,
            list_mailboxes,
            list_messages,
            get_sync_window,
            set_sync_window,
            backfill_to_window,
            save_draft,
            list_drafts,
            delete_draft,
            enqueue_send,
            list_outbox,
            cancel_send,
            retry_send,
            discard_outbox,
            load_more_messages,
            fetch_message,
            mark_seen,
            delete_message,
            move_message,
            search_messages,
            sync_status,
            save_attachment,
            pick_attachments,
            watch_inbox,
            stop_watching,
        ])
        .run(tauri::generate_context!())
        .expect("Easy Email 启动失败");
}
