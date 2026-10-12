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
    AccountConfig, BackfillBudget, BackfillReport, BackfillStop, Cache, ConnectionPool,
    ConnectionReport, DraftMirrorReport, FilingReport, FilingRun, MailError, MailboxInfo,
    MessageDetail, MessageSummary, NewOutbox, OutboxItem, OutboxRun, OutgoingAttachment,
    OutgoingMail, PushEvent, RecoveryReport, RecoveryRun, RunReport, SearchHit, SentCopy, Spool,
    SyncDriver, SyncOutcome, SyncWindow, account_with_endpoints, backfill_older, ensure_sent_copy,
    file_sent_copies, file_sha256, fill_thread_keys, prepare_attachments, recover_in_flight,
    run_due, secret, smtp,
    userdb::{Draft, DraftMirror, UserStore},
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
    /// Access tokens already fetched, per account, so concurrent work refreshes once.
    tokens: mail_core::oauth::TokenCache,
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
    ///
    /// A **kind** alongside the text, because "the refresh failed" and "you are offline" are
    /// different situations with different remedies, and the raw error text cannot be told apart by
    /// anyone reading it: an expired authorization code and a dead Wi-Fi both arrive as a connect or
    /// authentication error, and only the `MailError` variant that was still in hand at the capture
    /// point knows which one it was (`design/20-v0.2-plan.md` C3). Once it has been flattened into
    /// a sentence for the user, that knowledge is gone.
    last_sync_error: Mutex<Option<SyncFailure>>,
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
    /// Why `user.db` could not be opened, when that happened. See [`SyncStatus::user_store_error`].
    user_store_error: Mutex<Option<String>>,
    /// Wakes the outbox worker immediately.
    ///
    /// The tick is 5 s, but three events cannot wait for it: a message just queued (the undo window
    /// counts down in front of the user), an undo window that just expired, and a manual "retry now".
    outbox_wake: Arc<Notify>,
    /// The update the server announced, waiting for the user to say yes.
    ///
    /// Held rather than re-fetched because the plugin's `Update` is a resource with a private
    /// payload: it cannot be serialised to the renderer and back, so "ask the server again" would
    /// mean a second round trip and a second chance for the answer to change between the question
    /// and the click. `Mutex` (not `tokio::sync::Mutex`) because nothing is held across an `.await`
    /// — the download is started by the same command that owns it.
    pending_update: std::sync::Mutex<Option<tauri_plugin_updater::Update>>,
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

/// The secret a connection authenticates with: a password, or a freshly refreshed access token.
///
/// The account says which (`cfg.auth`), so call sites keep passing one string and the protocols keep
/// deciding the mechanism (M1a-52). OAuth accounts therefore have no password to fetch — the stored
/// refresh token is turned into an access token here, which is also where A1-F34's rotation gets
/// persisted (inside `oauth::fresh_access_token`).
///
/// A missing registration or a dead refresh token is reported as "sign in again", not as a network
/// problem, because that is what the user has to do about it.
async fn secret_for(state: &AppState, cfg: &AccountConfig) -> Result<String, String> {
    if cfg.auth != mail_core::AuthMethod::OAuth2 {
        return password_for(&cfg.id);
    }

    let Some(settings) = state.user.oauth_settings(&cfg.id).map_err(describe)? else {
        return Err(format!(
            "{} 是 OAuth2 账号，但还没有保存客户端 ID。请重新登录一次。",
            cfg.email
        ));
    };

    let provider = match cfg.provider_id.as_str() {
        "gmail" => mail_core::oauth::GOOGLE,
        "outlook" => mail_core::oauth::MICROSOFT,
        other => {
            return Err(format!("{other} 不支持 OAuth2 登录。"));
        }
    };

    let store = token_store(state)?;
    let transport = http_tokens()?;
    // Through the cache, so two commands that need this account at the same time refresh **once**:
    // with rotation (A1-F34) two racing refreshes can leave the older token on disk, and the account
    // then signs itself out at the next launch (`design/05`'s acceptance list asks for exactly this).
    state
        .tokens
        .access_token(
            &transport,
            &store,
            &cfg.id,
            provider,
            &settings.client_id,
            settings.client_secret.as_deref(),
            now_seconds(),
        )
        .await
        .map_err(describe)
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
    // No endpoints: this only ever previews a provider whose servers the table knows. A `custom`
    // account has nothing to preview until the user has typed something, and `account_with_endpoints`
    // says so instead of guessing.
    account_with_endpoints(
        &provider_id,
        &email,
        &display_name,
        username.as_deref(),
        None,
    )
    .map_err(describe)
}

// ---------------------------------------------------------------- accounts

/// The security choices the wizard offers, and the defaults for a new custom account.
///
/// A command rather than three constants in the renderer: the value that travels the wire is serde's
/// spelling of `Security`, and a second copy of it in JavaScript would keep looking right after a
/// rename and fail only at the moment a user picked that option.
#[derive(serde::Serialize)]
struct EndpointChoices {
    securities: Vec<SecurityChoice>,
    /// `custom`'s own row: the ports/security a new account starts from, so the defaults live in the
    /// provider table and not in a second place.
    default_imap_port: u16,
    default_imap_security: mail_core::Security,
    default_smtp_port: u16,
    default_smtp_security: mail_core::Security,
}

#[derive(serde::Serialize)]
struct SecurityChoice {
    value: mail_core::Security,
    label: &'static str,
    hint: &'static str,
}

#[tauri::command]
fn endpoint_choices() -> Result<EndpointChoices, String> {
    let custom = mail_core::providers::by_id("custom")
        .ok_or_else(|| "服务商表里没有 custom 这一项".to_string())?;
    Ok(EndpointChoices {
        securities: mail_core::Security::ALL
            .iter()
            .map(|s| SecurityChoice {
                value: *s,
                label: s.label(),
                hint: s.hint(),
            })
            .collect(),
        default_imap_port: custom.imap_port,
        default_imap_security: custom.imap_security,
        default_smtp_port: custom.smtp_port,
        default_smtp_security: custom.smtp_security,
    })
}

#[tauri::command]
fn list_accounts(state: State<'_, AppState>) -> Vec<AccountConfig> {
    state.snapshot()
}

/// Add or update an account and stash the credential in the Windows Credential Manager.
// Every parameter is one field of the wizard and one argument name on the wire (Tauri maps them
// straight through), so bundling them into a struct would only rename the contract the renderer
// sends — and `endpoints` is the one that is genuinely optional.
#[allow(clippy::too_many_arguments)]
#[tauri::command]
async fn save_account(
    state: State<'_, AppState>,
    app: AppHandle,
    provider_id: String,
    email: String,
    display_name: String,
    password: String,
    username: Option<String>,
    endpoints: Option<mail_core::model::EndpointOverride>,
    // The user stated this server offers no SMTP. Off means "both legs are expected", which is
    // every existing account (design/20-v0.2-plan.md, C8).
    smtp_none: bool,
) -> Result<AccountConfig, String> {
    // FIRST, before the credential is written and before the account file is touched: a bad host must
    // not leave a half-created account behind. (`closeModal` would delete it, but only if the dialog
    // is closed — a user who fixes the field and retries should never have created one.)
    let mut cfg = account_with_endpoints(
        &provider_id,
        &email,
        &display_name,
        username.as_deref(),
        endpoints.as_ref(),
    )
    .map_err(describe)?;

    cfg.smtp_none = smtp_none;

    // An account whose registration is already stored was signed in with OAuth a moment ago (the
    // wizard does that before saving). Marking it here — rather than asking the frontend to say so —
    // keeps the two facts from being able to disagree: the registration is what makes it OAuth.
    if state
        .user
        .oauth_settings(&cfg.id)
        .map_err(describe)?
        .is_some()
    {
        cfg.auth = mail_core::AuthMethod::OAuth2;
    }

    if cfg.auth == mail_core::AuthMethod::OAuth2 {
        // Nothing to store: the token already is the credential, and writing an empty password into
        // the vault would only create a second, wrong answer to "how does this account sign in".
        let _ = secret::delete_password(&cfg.id);
    } else {
        // Persist the credential first: an account that exists but cannot authenticate is worse than
        // a credential with no account entry.
        secret::save_password(&cfg.id, &password).map_err(describe)?;
    }

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
    if let Err(error) = start_watcher(&app, state.inner(), cfg.clone(), "INBOX".to_string()).await {
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
    let password = secret_for(&state, &cfg).await?;
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

    let password = secret_for(&state, &cfg).await?;
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

/// Which kind of thing went wrong the last time we tried to talk to the server.
///
/// The categories are the ones the user can act on: being offline and having an expired
/// authorization code both stop a refresh, but telling someone to check their network when their
/// credential needs regenerating sends them to the wrong place (`design/20-v0.2-plan.md` C3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureKind {
    /// The server could not be reached at all: no network, or a dead route.
    Network,
    /// The server was reached and refused the credential: an expired authorization code, a revoked
    /// app password, an OAuth token that needs another sign-in.
    Auth,
    /// The provider's own daily cap was hit. Time-limited, so retrying today cannot help.
    Quota,
    /// Anything the server said that is neither of the above: a protocol error, a closed SMTP AUTH,
    /// a folder that vanished.
    Server,
    /// A local problem: the cache or the user database. The server is fine.
    Local,
    /// A transient condition worth retrying: the database was locked, an operation timed out.
    Busy,
}

/// One refresh that did not make it, kept for the status line.
#[derive(Debug, Clone)]
pub struct SyncFailure {
    pub kind: FailureKind,
    /// The sentence the user reads, produced once so the log and the UI cannot disagree.
    pub detail: String,
}

/// The sentence, so every log line that used to print the error string still prints it.
///
/// The kind is deliberately *not* in here: it is for the machine-readable half of the contract, and
/// mixing it into the prose is how a user ends up reading "failure_kind: auth" in a message.
impl std::fmt::Display for SyncFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.detail)
    }
}

/// Classify a refresh failure while the structured error is still in hand.
///
/// This is the whole point of the type: the same `MailError` becomes "授权码可能过期" or
/// "现在连不上服务器" depending on its variant, and by the time it has been flattened into a
/// sentence nobody can tell them apart again.
pub fn failure_kind(error: &mail_core::error::MailError) -> FailureKind {
    use mail_core::error::MailError;
    match error {
        MailError::Connect { .. } | MailError::Tls { .. } => FailureKind::Network,
        MailError::Auth { .. } => FailureKind::Auth,
        MailError::Busy(_) | MailError::Timeout(_) => FailureKind::Busy,
        MailError::Cache(_) | MailError::LocalDb(_) => FailureKind::Local,
        MailError::Imap(_)
        | MailError::Smtp(_)
        | MailError::Config(_)
        | MailError::Credential(_)
        | MailError::Parse(_) => FailureKind::Server,
    }
}

/// Turn a protocol error into a classified failure, keeping the sentence the user reads.
///
/// The sentence goes through `describe` so the shape the user sees is the same one the log gets;
/// what this adds is the kind, which cannot be recovered from it afterwards.
fn failed(error: mail_core::error::MailError) -> SyncFailure {
    SyncFailure {
        kind: failure_kind(&error),
        detail: describe(error),
    }
}

/// Refresh one folder from the server into the cache.
///
/// Best effort by design: a mail client that shows nothing when the network is down is worse than
/// one showing slightly stale mail, which is the whole reason the cache exists. Callers fall back to
/// whatever is on disk and surface [`AppState::last_sync_error`] when it fails.
///
/// The error carries a **[`FailureKind`] next to the sentence** rather than a bare string. This is
/// the one place where that is still possible: the `MailError` is alive here, and past this function
/// it has been flattened by `describe` into prose that no reader can classify again. Everything that
/// wants to *say* something different for an offline account versus an expired authorization code
/// needs the kind, and it can only come from inside (`design/20-v0.2-plan.md` C3).
async fn refresh_folder(
    state: &AppState,
    pool: &ConnectionPool,
    cache: &Cache,
    cfg: &AccountConfig,
    mailbox: &str,
) -> std::result::Result<SyncOutcome, SyncFailure> {
    // Reuse the display name and delimiter we already know; only ask the server for `LIST` when
    // this folder has never been seen, so a normal refresh costs no extra round trip.
    let known = cache.get_folder(&cfg.id, mailbox).map_err(failed)?;

    let info = match known {
        Some(info) => info,
        None => {
            let password = secret_for(state, cfg).await.map_err(|detail| SyncFailure {
                kind: FailureKind::Auth,
                detail,
            })?;
            let all = pool_op!(pool, cfg, &password, |client| {
                client.list_mailboxes().await
            })
            .map_err(failed)?;

            let found = all
                .into_iter()
                .find(|m| m.name == mailbox)
                .ok_or_else(|| SyncFailure {
                    kind: FailureKind::Server,
                    detail: format!("服务器上找不到文件夹「{mailbox}」"),
                })?;
            // Record it with a zero generation; the sync below reports the real one.
            cache.upsert_folder(&cfg.id, &found, 0).map_err(failed)?;
            found
        }
    };

    let password = secret_for(state, cfg).await.map_err(|detail| SyncFailure {
        kind: FailureKind::Auth,
        detail,
    })?;

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
    .map_err(failed)?;

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
    let password = secret_for(&state, &cfg).await?;

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

    let sync_failure = refresh_folder(&state, &state.pool, &state.cache, &cfg, &mailbox)
        .await
        .err();
    // The classification happens inside `refresh_folder`, on the `MailError`, because this is the
    // last moment it exists.
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
        (true, Some(failure)) => Err(failure.detail),
        (true, None) => Ok(cached),
    }
}

/// One page of the message list, straight from the cache. **No network, no refresh.**
///
/// This is the paging endpoint the list moved to in round 75, and it is deliberately *not* the same
/// command as [`Self::list_messages`]:
///
/// * `list_messages` is the **first page** loader — it refreshes the folder first, because arriving
///   at a mailbox should show something fresh even when the network is the slow part;
/// * `message_page` is what 「加载更多」 calls, and it only reads what is already cached. It used to
///   call `list_messages`, which meant every press re-read the *entire* visible window (so both the
///   IPC payload and the frontend's re-sort grew with what was on screen) and triggered a full IMAP
///   refresh before it could show anything older.
///
/// `offset` is a row offset in the cached ordering (newest first), not a UID: it is the number of
/// rows the list already shows. `limit` is capped at `design/15` §6's IPC bound (≤ 200 rows per
/// transfer); the offset is clamped rather than rejected, because a stale offset from a folder the
/// user just switched away from is a normal thing for the renderer to send.
#[tauri::command]
async fn message_page(
    state: State<'_, AppState>,
    account_id: String,
    mailbox: String,
    offset: usize,
    limit: usize,
) -> Result<Vec<MessageSummary>, String> {
    // The account has to exist — otherwise this would page a folder id that nothing knows about.
    state.find(&account_id)?;
    let limit = limit.clamp(1, 200);
    let offset = offset.min(1_000_000);

    state
        .cache
        .clone()
        .list_summaries(&account_id, &mailbox, limit as u32, offset as u32)
        .map_err(describe)
}

/// Every cached message of one conversation, across folders, oldest first.
///
/// No network: the conversation is assembled from what is already on disk, so this is the cheap half
/// of opening a thread and the reason the reader can show a whole exchange instantly. Bodies are a
/// separate, on-demand call per message (`fetch_message`), because most of a conversation is headers.
///
/// The limit is a bound on a `limit`-able query, not a product rule: the UI shows a conversation, and
/// "a conversation" has no upper bound in the data. Rows that predate `thread_key` are absent here by
/// design — the renderer groups those from the list it already has, and guessing in SQL would merge
/// conversations permanently.
#[tauri::command]
async fn list_thread(
    state: State<'_, AppState>,
    account_id: String,
    thread_key: String,
) -> Result<Vec<mail_core::model::ThreadMember>, String> {
    if thread_key.trim().is_empty() {
        return Ok(Vec::new());
    }
    state
        .cache
        .clone()
        .thread_summaries(&account_id, &thread_key, THREAD_MEMBERS_MAX)
        .map_err(describe)
}

/// How many messages of one conversation are read from the cache.
///
/// Generous on purpose: this is a disk read of rows the account already has, and truncating a real
/// conversation is the failure the feature exists to prevent. It is a bound, not a budget.
const THREAD_MEMBERS_MAX: u32 = 500;

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

    let password = secret_for(&state, &cfg).await?;
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
///
/// The mirror goes with it: leaving our copy in the user's server-side 草稿箱 would be a copy nothing
/// can ever clean up, from a button whose whole job is "make this go away" (`design/07` §6).
/// Best-effort — the local row is what the user asked to remove, and a network failure must not turn
/// "discarded" into "not discarded".
#[tauri::command]
async fn delete_draft(state: State<'_, AppState>, id: String) -> Result<bool, String> {
    let mirror = state.user.take_draft_mirror(&id).map_err(describe)?;
    let removed = state.user.delete_draft(&id).map_err(describe)?;
    if let Some(copy) = mirror {
        drop_server_copy(&state, &copy).await;
    }
    Ok(removed)
}

/// Ask the server to drop the copy we wrote, ignoring every failure.
///
/// A stale copy is a nuisance; blocking the user's discard on the network is worse. What is *not*
/// acceptable is failing silently in a way that loses data — and this cannot: it only ever removes a
/// copy of a draft that is already gone locally.
async fn drop_server_copy(state: &AppState, copy: &DraftMirror) {
    let Some(uid) = copy.uid else {
        // APPEND did not tell us the UID (A1-F43) and the lookup failed at mirror time, so there is
        // no handle to delete by. The copy is still findable by Message-ID on the next mirror/sweep.
        return;
    };
    let Ok(cfg) = state.find(&copy.account_id) else {
        return;
    };
    let Ok(password) = secret_for(state, &cfg).await else {
        return;
    };
    let folder = copy.folder.clone();
    let _ = pool_op!(&state.pool, &cfg, &password, |client| {
        client.delete_message(&folder, uid).await
    });
}

/// Walk backwards until the **chosen sync window** is satisfied, and report where it stopped.
///
/// This is what makes the window a setting rather than a stored preference: the walk stops at the
/// date boundary for `Years(n)` and once `n` messages are cached for `Count(n)`.
///
/// `pages` bounds the walk, and the bound is the point:
/// * `None` — the cold-start case. One page, so opening the app tops the list up by a page without
///   becoming a multi-thousand-message download when the window is "all mail".
/// * `Some(n)` — an explicit "keep walking", bounded by `n` pages.
///
/// The daily byte budget (`design/04` §5.7) is now what paces this: the walk is given what is left
/// of the account's quota for the window, and what it fetched is debited afterwards.
#[tauri::command]
async fn backfill_to_window(
    state: State<'_, AppState>,
    account_id: String,
    mailbox: String,
    pages: Option<u32>,
) -> Result<BackfillReport, String> {
    let cfg = state.find(&account_id)?;
    let window = state.user.sync_window().map_err(describe)?;
    let cache = state.cache.clone();
    let password = secret_for(&state, &cfg).await?;
    let budget_for_account = mail_core::providers::by_id(&cfg.provider_id)
        .and_then(|provider| provider.daily_download_budget())
        .map(|bytes| bytes as i64);

    // What is left of today's quota. `None` means the provider has no documented limit, which is the
    // common case and must not be turned into a guess.
    let ledger = state
        .user
        .quota(&account_id, now_seconds(), budget_for_account)
        .map_err(describe)?;
    if !ledger.allows_download(now_seconds()) {
        // Stop before touching the network: overrunning the quota is what gets the account
        // throttled, and the provider's own refusal is a much worse way to find out.
        return Ok(BackfillReport {
            pages: 0,
            fetched: 0,
            bytes: 0,
            stop: BackfillStop::QuotaSpent,
        });
    }

    let mut budget = match pages {
        None => BackfillBudget::default(),
        Some(pages) => BackfillBudget {
            max_pages: pages.max(1),
            max_messages: pages.max(1) * mail_core::sync::INITIAL_SYNC as u32,
            max_bytes: u64::MAX,
        },
    };
    if let Some(remaining) = ledger.remaining() {
        budget.max_bytes = remaining.max(0) as u64;
    }

    let report = pool_op!(&state.pool, &cfg, &password, |client| {
        backfill_older(
            &mut client,
            &cache,
            &account_id,
            &mailbox,
            window,
            budget,
            now_seconds(),
        )
        .await
    })
    .map_err(describe)?;

    // Debit **after** storing, and only for what was actually stored: a walk that fetched and then
    // failed has not delivered anything the user can read, and charging for it would be inventing a
    // cost. (`design/04` §5.7 asks for the ledger to be incremented per batch; the report's `bytes`
    // is the sum of the sizes of the messages that landed in the cache.)
    let ledger = state
        .user
        .quota_add_bytes(
            &account_id,
            report.bytes as i64,
            now_seconds(),
            budget_for_account,
        )
        .map_err(describe)?;

    // `design/04` §5.7 item 4: when a provider we *know* caps the fetch range and the walk has
    // reached the end of what the server will give, the oldest date we hold is the observed floor.
    // Recording it is not an inference about the server — it is what we asked for and what came
    // back — and it is what lets the UI say "这个邮箱只能取到这一天" instead of looking broken.
    if report.stop == BackfillStop::HistoryExhausted
        && mail_core::providers::by_id(&cfg.provider_id)
            .is_some_and(|provider| provider.caps_third_party_range())
        && let Some(floor) = state
            .cache
            .clone()
            .oldest_date(&account_id, &mailbox)
            .map_err(describe)?
    {
        state
            .user
            .set_quota_server_floor(&account_id, floor)
            .map_err(describe)?;
    }

    log_line(&format!(
        "[easy-email] 回填到窗口边界 {}：{} 页 / {} 封 / {} 字节（{:?}）；今日已用 {}",
        window.as_key(),
        report.pages,
        report.fetched,
        report.bytes,
        report.stop,
        match (ledger.bytes_today, ledger.daily_budget) {
            (used, Some(budget)) => format!("{used}/{budget}"),
            (used, None) => format!("{used}（无上限）"),
        }
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

    if mail.to.is_empty() && mail.cc.is_empty() && mail.bcc.is_empty() {
        return Err("请至少填写一个收件人。".to_string());
    }

    // A receive-only account has no SMTP to talk to, by the user's own statement. Refusing here
    // rather than letting the worker discover it per attempt is the difference between one clear
    // sentence and a queue full of failures that all say the same thing.
    if cfg.smtp_none {
        return Err(
            "这个账号绑定的是只收信的服务器（添加时你标注了「不提供 SMTP」），因此不能从这里发信。"
                .to_string(),
        );
    }

    // Attachments are paths, so the file can have been edited or deleted since it was chosen, and
    // this is the last moment at which that can still be a question: once the bytes are frozen they
    // are what goes out. A draft that predates hashing is completed here rather than refused.
    prepare_attachments(&mut mail).map_err(describe)?;

    // Freeze the identity before anything else: lettre generates a fresh Message-ID per build, so
    // rebuilding later would silently produce a *different* message as far as every client is
    // concerned — and crash recovery would have nothing to search the Sent folder for.
    let message = smtp::build_message(&cfg, &mail).map_err(describe)?;
    if mail.message_id.is_none() {
        mail.message_id = smtp::message_id_of(&message);
    }
    let bytes = message.formatted();

    // `design/07` §6: refuse before the bytes go anywhere. The alternative is a 25 MB upload that ends
    // in a 552 five seconds later, which costs the user exactly the time it was meant to save — and
    // the number in the sentence is the one that actually applied, not a generic "too big".
    let limit =
        mail_core::providers::max_outgoing_bytes(mail_core::providers::by_id(&cfg.provider_id));
    if bytes.len() as u64 > limit {
        return Err(format!(
            "这封信编码后约 {:.1} MB，超过可发送上限 {:.0} MB。\
             请压缩附件、改用云盘链接，或者拆成两封再发。",
            bytes.len() as f64 / 1_048_576.0,
            limit as f64 / 1_048_576.0
        ));
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

/// Where a delivered message's Sent copy is, if it was ever filed (`design/07` §5.4).
///
/// The outbox row itself only knows the `Message-ID`; the folder and UID live in the cache's
/// `sent_index`, which is derived data — rebuilding the cache loses the *link*, not the copy.
#[tauri::command]
async fn sent_copy(
    state: State<'_, AppState>,
    account_id: String,
    message_id: String,
) -> Result<Option<mail_core::cache::SentCopyRecord>, String> {
    state
        .cache
        .clone()
        .sent_copy(&account_id, &message_id)
        .map_err(describe)
}

/// Mirror a draft to the server's Drafts folder, replacing the copy we wrote last time.
///
/// Best-effort by design (`design/07` §3): the local draft in `user.db` is the truth, and this is
/// the second copy that makes the draft show up on another device. A failure here is reported so the
/// composer can say "本地已保存（云端草稿待同步）" — it must never be why somebody loses what they
/// typed.
#[tauri::command]
async fn mirror_draft(
    state: State<'_, AppState>,
    draft_id: String,
) -> Result<Option<DraftMirrorReport>, String> {
    let Some(draft) = state.user.load_draft(&draft_id).map_err(describe)? else {
        // The draft was discarded between the save and the mirror. Not an error: there is nothing to
        // mirror, and the copy we may have written earlier is now the only stale one left.
        return Ok(None);
    };
    let cfg = state.find(&draft.account_id)?;
    let password = secret_for(&state, &cfg).await?;
    let previous = state.user.draft_mirror(&draft_id).map_err(describe)?;

    let report = pool_op!(&state.pool, &cfg, &password, |client| {
        mail_core::sentcopy::mirror_draft(&mut client, &cfg, &draft, previous.as_ref()).await
    })
    .map_err(describe)?;

    if let Some(copy) = &report {
        state
            .user
            .set_draft_mirror(&DraftMirror {
                draft_id,
                account_id: draft.account_id.clone(),
                folder: copy.folder.clone(),
                uid: copy.uid,
                message_id: copy.message_id.clone(),
                updated_at: now_seconds(),
            })
            .map_err(describe)?;
    }

    Ok(report)
}

/// How many pages of headers the key refresh walks. Five pages is ~2500 messages on a busy folder,
/// which covers the conversations a user actually scrolls into.
const THREAD_KEY_PAGES: u32 = 5;

/// Give the cached mail of one folder the conversation keys it was cached without.
///
/// The `thread_key` column arrived after those rows were written (`cache.db` v3), and an incremental
/// sync will never fill them: it asks only for UIDs *above* the cursor, and the rows below it were
/// stored without their `References` chain. Left alone, old mail keeps the subject-based fallback
/// forever while new mail groups exactly — two behaviours in one list, which is worse than either.
///
/// One re-fetch of the newest page fixes it for everything the list actually shows, and it is cheap:
/// headers only, the same call the cold sync already makes. Requested by the renderer, once per
/// account per session, and only when it can see rows without a key.
#[tauri::command]
async fn refresh_thread_keys(
    state: State<'_, AppState>,
    account_id: String,
    mailbox: String,
) -> Result<u32, String> {
    let cfg = state.find(&account_id)?;
    let password = secret_for(&state, &cfg).await?;
    let cache = state.cache.clone();

    // A few pages rather than one: the newest page covers what is on screen, and the pages below it
    // are where a conversation that started weeks ago still has keyless members. Bounded, because
    // every page is a round trip (`design/04`'s budget applies to this too).
    let filled = pool_op!(&state.pool, &cfg, &password, |client| {
        fill_thread_keys(
            &mut client,
            &cache,
            &account_id,
            &mailbox,
            THREAD_KEY_PAGES,
            mail_core::sync::INITIAL_SYNC,
        )
        .await
    })
    .map_err(describe)?;

    log_line(&format!(
        "[easy-email] 为 {account_id} 的 {mailbox} 补齐会话键：{filled} 封"
    ));
    Ok(filled)
}
/// Unread messages per folder, for the sidebar badges.
///
/// Read from the cache, not the server: the badges have to be there on an offline start (the same
/// reason the list itself is served from the cache), and a `LIST` round trip per refresh would make
/// them lag the very event they are reporting. A folder with nothing unread is absent, so the
/// renderer shows nothing for it instead of a row of zeroes.
#[tauri::command]
async fn folder_unread_counts(
    state: State<'_, AppState>,
    account_id: String,
) -> Result<std::collections::HashMap<String, u32>, String> {
    state
        .cache
        .clone()
        .unread_counts(&account_id)
        .map_err(describe)
}

/// Whether new mail for this account shows a system notification.
#[tauri::command]
async fn get_notify_enabled(
    state: State<'_, AppState>,
    account_id: String,
) -> Result<bool, String> {
    state.user.notify_enabled(&account_id).map_err(describe)
}

/// Remember the user's choice. Only affects the toast; mail still arrives either way.
#[tauri::command]
async fn set_notify_enabled(
    state: State<'_, AppState>,
    account_id: String,
    enabled: bool,
) -> Result<(), String> {
    state
        .user
        .set_notify_enabled(&account_id, enabled)
        .map_err(describe)
}
/// Sign an existing OAuth account in again, reusing the registration it was created with.
///
/// Needed because of what this app tells the user: when a refresh token is revoked or expired, every
/// message says "请重新登录一次" (`oauth::fresh_access_token`, A1-F34) — and until this existed there was
/// nothing to click. The client id is not asked for again: it is stored with the account (M1a-54), and
/// making somebody re-enter something we already know is how a recovery path stops being used.
#[tauri::command]
async fn oauth_relogin(
    state: State<'_, AppState>,
    account_id: String,
) -> Result<mail_core::oauth::TokenSet, String> {
    let cfg = state.find(&account_id)?;
    let settings = state
        .user
        .oauth_settings(&cfg.id)
        .map_err(describe)?
        .ok_or_else(|| {
            format!(
                "{} 没有保存 OAuth 客户端 ID，请删除后重新添加账号。",
                cfg.email
            )
        })?;

    let provider = match cfg.provider_id.as_str() {
        "gmail" => mail_core::oauth::GOOGLE,
        "outlook" => mail_core::oauth::MICROSOFT,
        other => return Err(format!("{other} 不支持 OAuth2 登录。")),
    };

    let tokens = mail_core::oauth::sign_in(
        &http_tokens()?,
        &mail_core::oauth::SystemBrowser,
        provider,
        &settings.client_id,
        settings.client_secret.as_deref(),
        std::time::Duration::from_secs(120),
    )
    .await
    .map_err(describe)?;

    match tokens.refresh_token.as_deref() {
        Some(refresh) => {
            token_store(&state)?
                .save(&cfg.id, refresh)
                .map_err(describe)?;
            log_line(&format!("[easy-email] {} 已重新授权", cfg.id));
        }
        None => {
            return Err(
                "服务商这次没有返回刷新令牌，账号仍然是未授权状态。请稍后再试。".to_string(),
            );
        }
    }

    Ok(tokens)
}
/// What happened to the last update attempt, if anything.
///
/// Called once at startup by the renderer. `None` is the ordinary case; `Updated` and `StillOld` are
/// the two sentences `design/17` §5 asks for, and the note is cleared as it is read so neither is
/// repeated on the next launch.
#[tauri::command]
async fn take_update_outcome(
    state: State<'_, AppState>,
) -> Result<Option<mail_core::update::Outcome>, String> {
    let dir = state
        .accounts_path
        .parent()
        .ok_or_else(|| "无法确定数据目录".to_string())?;
    mail_core::update::UpdateState::new(dir)
        .settle(env!("CARGO_PKG_VERSION"))
        .map_err(describe)
}
/// What the server said about a newer version, or `None` when there is nothing to install.
///
/// `notes` is the release text, shown before the user commits to a download: an update that says
/// nothing about what it fixes is an update nobody should install on faith.
#[derive(serde::Serialize)]
struct UpdateInfo {
    version: String,
    current_version: String,
    notes: Option<String>,
}

/// Ask the release page whether there is a newer build.
///
/// Returns `None` for three different situations and it does not need to tell them apart, because
/// all three mean "nothing to do right now": up to date, the manifest is not there yet (a release
/// that predates `latest.json`, or a draft), and the network is unreachable. The last one is logged
/// rather than raised — `design/17` §5 is explicit that a failed check must not disturb anyone, and
/// three consecutive failures are what the UI is allowed to mention.
#[tauri::command]
async fn check_update(app: AppHandle) -> Result<Option<UpdateInfo>, String> {
    use tauri_plugin_updater::UpdaterExt;
    let update = app
        .updater()
        .map_err(|error| error.to_string())?
        .check()
        .await
        .map_err(|error| {
            log_line(&format!("[easy-email] 检查更新失败：{error}"));
            error.to_string()
        })?;

    let Some(update) = update else {
        // Stale answers must not outlive the answer that replaced them.
        *app.state::<AppState>()
            .pending_update
            .lock()
            .expect("pending update mutex") = None;
        return Ok(None);
    };

    let info = UpdateInfo {
        version: update.version.clone(),
        current_version: update.current_version.clone(),
        notes: update.body.clone(),
    };
    *app.state::<AppState>()
        .pending_update
        .lock()
        .expect("pending update mutex") = Some(update);
    log_line(&format!(
        "[easy-email] 发现新版本 {}（当前 {}）",
        info.version, info.current_version
    ));
    Ok(Some(info))
}

/// Download and install the update the user just agreed to.
///
/// The signature was verified when the manifest was read, so this cannot install something the
/// private key did not sign. Progress is emitted as `update:progress` because the frontend has no
/// bundler: `@tauri-apps/plugin-updater`'s JS object is not in this app's assets, so the closure
/// here is the only place the numbers exist.
///
/// The app exits on Windows as soon as the installer is spawned (a platform limitation, not a
/// choice), so the "restart" question is answered by `install` itself rather than by the UI.
#[tauri::command]
async fn install_update(app: AppHandle) -> Result<(), String> {
    // Take it rather than borrow it: a second click must not start a second download of the same
    // bytes, and the update is single-use by nature (the version it installs is this one).
    let update = app
        .state::<AppState>()
        .pending_update
        .lock()
        .expect("pending update mutex")
        .take();

    let Some(update) = update else {
        return Err("没有待安装的更新，请先检查一次。".to_string());
    };

    // **Before** the installer runs: write down which version we are trying to become. If the app is
    // still the old version next launch, the installer did not take — and a silent failure there is
    // how someone presses "restart to update" forever (`design/17` §5).
    let note_dir = app
        .state::<AppState>()
        .accounts_path
        .parent()
        .ok_or_else(|| "无法确定数据目录".to_string())?
        .to_path_buf();
    let note = mail_core::update::UpdateState::new(note_dir);
    note.begin(&update.version, Some("用户从关于面板启动的更新"))
        .map_err(describe)?;

    let progress = app.clone();
    let mut last_logged = std::time::Instant::now();
    update
        .download_and_install(
            |chunk, total| {
                // Once a second, not once per chunk: a 30 MB download arrives in hundreds of
                // chunks and emitting every one would turn the IPC channel into the bottleneck.
                if last_logged.elapsed() >= std::time::Duration::from_secs(1) {
                    last_logged = std::time::Instant::now();
                    let _ = progress.emit(
                        "update:progress",
                        serde_json::json!({ "downloaded": chunk, "total": total }),
                    );
                }
            },
            || {
                log_line("[easy-email] 更新包下载完成，开始安装");
            },
        )
        .await
        .map_err(|error| {
            // The installer did not run, so the note is a lie. `settle` would report
            // "更新失败，已回滚" for an installer that was never launched, which is a promise the
            // user cannot act on; `forget` drops it instead (`design/20-v0.2-plan.md` V2-C2).
            let _ = note.forget();
            log_line(&format!("[easy-email] 安装更新失败：{error}"));
            error.to_string()
        })?;

    log_line("[easy-email] 更新已安装，应用即将退出");
    Ok(())
}

/// Refresh tokens live beside the account list, in their own subdirectory.
///
/// Derived from `accounts_path` rather than stored as a second field: two paths that must agree are
/// two chances to disagree, and the account list is already in the directory the tokens belong to.
fn token_store(state: &AppState) -> std::result::Result<mail_core::tokenstore::TokenStore, String> {
    let dir = state
        .accounts_path
        .parent()
        .ok_or_else(|| "无法确定数据目录，OAuth 令牌无处保存".to_string())?;
    Ok(mail_core::tokenstore::TokenStore::new(dir))
}

/// One HTTP client per sign-in: the flow happens once, by hand, and a pooled client kept alive for it
/// would be a connection to a token endpoint that nobody is using.
fn http_tokens() -> std::result::Result<mail_core::oauth::HttpTokens, String> {
    mail_core::oauth::HttpTokens::new().map_err(describe)
}
/// Which OAuth registration this installation should offer in the wizard.
///
/// Read from the app's data directory (the user's own registration, dropped in as the JSON Google
/// Cloud hands out), falling back to the published Thunderbird one — which is *borrowed*, revocable by
/// Mozilla, and shows up on the consent screen as "Mozilla Thunderbird", so it comes with a notice the
/// UI is expected to show (PROGRESS, M1a-63).
#[tauri::command]
fn oauth_client_default(
    state: State<'_, AppState>,
    provider_id: String,
) -> Result<mail_core::oauth::ClientChoice, String> {
    let dir = state
        .accounts_path
        .parent()
        .ok_or_else(|| "无法确定数据目录".to_string())?;

    let own = mail_core::oauthclient::client_registration(dir).map_err(describe)?;
    let mut choice = mail_core::oauth::client_choice(&provider_id, own);
    // The renderer decides whether to show the OAuth path from this, not from a provider id of its
    // own: one table decides, and `providers::oauth_available` is where the exception for Gmail lives.
    choice.oauth_available = mail_core::providers::by_id(&provider_id)
        .map(|provider| provider.oauth_available())
        .unwrap_or(false);
    Ok(choice)
}
/// Sign in with OAuth and stow the refresh token.
///
/// The wizard calls this when the address belongs to a provider that requires OAuth (Google, Microsoft)
/// and the user has supplied their own client id — M0-8's open question is whether the token endpoint
/// also demands a `client_secret`, so that is a parameter rather than an assumption.
///
/// The whole protocol lives in `mail_core::oauth`; this function only supplies the three things the
/// library cannot know: the account's identity, the app's data directory, and the provider's id.
#[tauri::command]
async fn oauth_sign_in(
    state: State<'_, AppState>,
    email: String,
    provider_id: String,
    client_id: String,
    client_secret: Option<String>,
) -> Result<mail_core::oauth::TokenSet, String> {
    let provider = match provider_id.as_str() {
        "gmail" => mail_core::oauth::GOOGLE,
        "outlook" => mail_core::oauth::MICROSOFT,
        other => {
            return Err(format!("{other} 不支持 OAuth 登录，请使用密码或授权码。"));
        }
    };

    // 120 seconds: the user has to find the browser, pick an account, and consent. Long enough to be
    // usable, short enough that a window left open does not hold a listening socket for ever.
    // The registration file also says *where* its endpoints are, and the kind of client it describes:
    // a "Web application" client is refused with that word instead of failing later as a
    // redirect_uri_mismatch, and a registration pointing somewhere else (a test environment) is used
    // as written. Absent a file, the provider's built-in endpoints are what the flow runs against.
    let (kind, provider) = match token_store(&state)
        .ok()
        .and_then(|_| state.accounts_path.parent())
        .map(mail_core::oauthclient::client_registration)
    {
        Some(Ok(mail_core::oauthclient::Registration::Own { registration })) => (
            registration.kind,
            provider.with_registration_endpoints(&registration),
        ),
        _ => (mail_core::oauthclient::ClientKind::Desktop, provider),
    };

    let tokens = mail_core::oauth::sign_in_as(
        &http_tokens()?,
        &mail_core::oauth::SystemBrowser,
        provider,
        &client_id,
        client_secret.as_deref(),
        kind,
        std::time::Duration::from_secs(120),
    )
    .await
    .map_err(|error| {
        let message = describe(error);
        // Logged as well as returned. The message the user sees is one sentence by design; the
        // provider's own words (`access_denied`, `invalid_client`, `redirect_uri_mismatch`) are what
        // diagnose it, and a toast is not something a person can hand back to whoever is debugging.
        // No secret goes in here: the text is the provider's, and the client id is not secret.
        let tail: String = client_id
            .chars()
            .rev()
            .take(14)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        log_line(&format!(
            "[easy-email] OAuth 登录失败（{provider_id}，client_id …{tail}）：{message}"
        ));
        message
    })?;

    // The browser signed in as *someone*, and it is not necessarily whoever was typed into the form:
    // the two are decided in different places and nothing used to compare them. An account whose
    // address does not match its token cannot ever authenticate — it fails on every connection, for
    // ever — so this is the last moment where the difference can become a sentence the user can act on,
    // instead of a saved account that will never work and a message blaming the network (M1a-82).
    //
    // Every branch says which of the three happened, because "we did not check" and "we checked and it
    // matched" look identical from the outside and only one of them is reassuring.
    match tokens
        .id_token
        .as_deref()
        .and_then(mail_core::oauth::email_from_id_token)
    {
        Some(authorized) if !authorized.eq_ignore_ascii_case(email.trim()) => {
            let message = format!(
                "浏览器里授权的账号是 {authorized}，但上面填的地址是 {}。保存这样一个账号没有意义 —— \
                 服务器每次都会拒绝它。请把地址改成 {authorized}，再点一次「打开浏览器登录」。",
                email.trim()
            );
            log_line(&format!(
                "[easy-email] OAuth 授权账号与填写的地址不一致：{message}"
            ));
            return Err(message);
        }
        Some(_) => log_line("[easy-email] OAuth 授权账号已核对（与填写的地址一致）"),
        None => log_line(
            "[easy-email] 服务商没有返回可用的 id_token，这次无法核对浏览器里授权的是哪个账号",
        ),
    }

    // The registration is stored with the token: a refresh needs both, and asking the user for the
    // client id a second time would be asking them to remember something we already know.
    state
        .user
        .set_oauth_settings(
            &email.to_ascii_lowercase(),
            &mail_core::userdb::OAuthSettings {
                client_id: client_id.clone(),
                client_secret: client_secret.clone(),
            },
        )
        .map_err(describe)?;

    // A1-F34: a refresh token that is not persisted signs the user out at the next launch — and with
    // Microsoft's rotation, it also invalidates the one we would otherwise have kept.
    match tokens.refresh_token.as_deref() {
        Some(refresh) => {
            token_store(&state)?
                .save(&email, refresh)
                .map_err(describe)?;
            log_line(&format!("[easy-email] {email} 的 OAuth 刷新令牌已保存"));
        }
        None => log_line(&format!(
            "[easy-email] {email} 的 OAuth 登录没有返回刷新令牌（access_type/prompt 可能没生效）"
        )),
    }

    Ok(tokens)
}
/// What the account has downloaded today, and what it is allowed to (`design/04` §5.7).
///
/// The ledger decides when the sync stops; this is the same number, shown to the user. Without it the
/// client would start refusing to fetch mail for a reason nothing on screen explains — "配额用尽"
/// would look exactly like "网络坏了".
#[tauri::command]
async fn quota_status(
    state: State<'_, AppState>,
    account_id: String,
) -> Result<mail_core::userdb::QuotaLedger, String> {
    let cfg = state.find(&account_id)?;
    let budget = mail_core::providers::by_id(&cfg.provider_id)
        .and_then(|provider| provider.daily_download_budget())
        .map(|bytes| bytes as i64);
    state
        .user
        .quota(&account_id, now_seconds(), budget)
        .map_err(describe)
}
/// The largest message this account may send, in bytes.
///
/// `enqueue_send` enforces this before the bytes are frozen; the composer asks for it as well so an
/// oversized attachment is refused when it is chosen rather than after the mail has been written
/// (`design/07` §6, `design/15` §4). One number, one place: the renderer must not keep its own copy.
#[tauri::command]
async fn max_send_bytes(state: State<'_, AppState>, account_id: String) -> Result<u64, String> {
    let cfg = state.find(&account_id)?;
    Ok(mail_core::providers::max_outgoing_bytes(
        mail_core::providers::by_id(&cfg.provider_id),
    ))
}

/// What is really behind a link in a message (`design/14` §4.2).
///
/// The renderer never follows a link from a message body: the sanitiser strips every `href` and keeps
/// the target as `data-ee-href`, so the reading pane cannot navigate anywhere. When the user asks to
/// open one, this is what they are shown first — the host that would really be connected to, and
/// whether the visible text contradicts it.
#[tauri::command]
fn classify_link(url: String, text: String) -> mail_core::links::LinkVerdict {
    mail_core::links::classify_link(&url, &text)
}

/// Open a link in the system browser, but only after classifying it again.
///
/// The verdict is computed **here** rather than taken from the caller: a renderer bug (or a future
/// caller that forgets) must not be able to turn "show the user what this is" into "open whatever the
/// mail said". A dangerous scheme is refused with the reason, not silently ignored.
#[tauri::command]
fn open_link(url: String, text: String) -> Result<(), String> {
    use mail_core::oauth::{BrowserLauncher, SystemBrowser};

    let verdict = mail_core::links::classify_link(&url, &text);
    if !verdict.openable() {
        return Err(verdict.reason);
    }
    SystemBrowser
        .open(&verdict.full)
        .map_err(|error| format!("{}（{error}）", verdict.reason))
}

/// The sentence to show when a sync stopped because the server will not go further back.
///
/// Only for services known to cap how far back a third-party client may read (A1-F2, the NetEase
/// family). For everyone else "no older mail" means the mailbox has no older mail, and a hint about a
/// setting would be noise. `None` means say nothing.
///
/// This exists because the symptom is indistinguishable from our own bug: the user asked for two
/// years, got thirty days, and nothing in the protocol explains why. The wording lives next to the
/// data rather than in the renderer, so the list of services that behave this way stays in one place.
#[tauri::command]
async fn range_cap_hint(
    state: State<'_, AppState>,
    account_id: String,
) -> Result<Option<String>, String> {
    let cfg = state.find(&account_id)?;
    let Some(provider) = providers::by_id(&cfg.provider_id) else {
        return Ok(None);
    };
    if !provider.caps_third_party_range() {
        return Ok(None);
    }

    Ok(Some(if provider.range_cap_is_enterprise_policy() {
        "该邮箱的企业策略只允许第三方客户端读取最近约 30 天的邮件。\
         如需更早，请在网易网页版设置里把「收取范围」改大，然后回到这里重试。"
            .to_string()
    } else {
        "这个邮箱只让第三方客户端读取最近约 30 天的邮件。\
         如需更早，请到网页版把「收取范围 / IMAP 收取」改大，然后回到这里重试。"
            .to_string()
    }))
}
/// Where the server-side copy of a draft is, if we ever wrote one.
///
/// Exposed so the composer can be honest later ("云端草稿待同步" vs "已同步") without guessing, and
/// so a future "另一台设备改了这份草稿" prompt has something to compare against.
#[tauri::command]
async fn draft_mirror(
    state: State<'_, AppState>,
    draft_id: String,
) -> Result<Option<DraftMirror>, String> {
    state.user.draft_mirror(&draft_id).map_err(describe)
}

/// Delete the server-side draft copies whose local draft is gone.
///
/// This is the other half of `mirror_draft`: a mirror row outlives its draft whenever the send path
/// deletes the draft after a successful delivery, and whenever the user discards a draft while
/// offline. Without this the user's 草稿箱 keeps a copy nothing in the app can reach.
///
/// Best-effort throughout, and it only ever removes copies we wrote ourselves — the folder and UID
/// come from our own `draft_mirrors` row, never from a search.
async fn sweep_draft_mirrors(app: &AppHandle) -> usize {
    let (store, pool) = {
        let state = app.state::<AppState>();
        (state.user.clone(), state.pool.clone())
    };

    let orphans = match store.orphaned_draft_mirrors() {
        Ok(orphans) => orphans,
        Err(error) => {
            log_line(&format!("[easy-email] 读不出待清理的草稿镜像：{error}"));
            return 0;
        }
    };

    let mut removed = 0;
    for orphan in orphans {
        let state = app.state::<AppState>();
        let Ok(cfg) = state.find(&orphan.account_id) else {
            // The account is gone, so there is nobody to ask and no way to reach the copy. Drop the
            // row: keeping it would only mean retrying this forever.
            let _ = store.take_draft_mirror(&orphan.draft_id);
            continue;
        };
        let Ok(password) = secret_for(&state, &cfg).await else {
            // No credential — the next sweep, after the user fixes the account, can still do it.
            continue;
        };
        if let Some(uid) = orphan.uid {
            let folder = orphan.folder.clone();
            let _ = pool_op!(&pool, &cfg, &password, |client| {
                client.delete_message(&folder, uid).await
            });
        }
        if store.take_draft_mirror(&orphan.draft_id).is_ok() {
            removed += 1;
        }
    }

    removed
}
/// Run one pass of the outbox worker for every configured account.
///
/// One account at a time on purpose: the connection budget in `design/04` is per account, and a
/// mailbox that is being rate-limited should not be sent two connections' worth of work at once.
async fn outbox_pass(app: &AppHandle) -> Result<(RunReport, FilingReport), String> {
    let (accounts, store, spool, pool, cache) = {
        let state = app.state::<AppState>();
        (
            state.snapshot(),
            state.user.clone(),
            state.spool.clone(),
            state.pool.clone(),
            state.cache.clone(),
        )
    };

    let mut total = RunReport::default();
    let mut filed = FilingReport::default();
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

        // Sent copies (`design/07` §5.4). Deliberately after the sends and never instead of one:
        // the message is delivered either way, and a failure here leaves the frozen bytes in place
        // for the next pass rather than un-sending anything.
        let filing = file_sent_copies(
            FilingRun {
                store: &store,
                spool: &spool,
                cache: &cache,
                now: now_seconds(),
            },
            |message_id, bytes| {
                let pool = pool.clone();
                let cfg = account.clone();
                let password = password.clone();
                // The size limit is the provider's *append* limit, not its SMTP one (A1-F3): the
                // archive copy is an IMAP upload, and the two numbers belong to different services.
                let append_limit = mail_core::providers::by_id(&cfg.provider_id)
                    .and_then(|provider| provider.append_limit_bytes());
                async move {
                    file_one_copy(&pool, &cfg, &password, &message_id, bytes, append_limit).await
                }
            },
        )
        .await
        .map_err(describe)?;

        filed.filed += filing.filed;
        filed.skipped += filing.skipped;
        filed.deferred += filing.deferred;
    }

    Ok((total, filed))
}

/// Put one delivered message's copy where it belongs, or find the one the server wrote.
async fn file_one_copy(
    pool: &ConnectionPool,
    cfg: &AccountConfig,
    password: &str,
    message_id: &str,
    bytes: Vec<u8>,
    append_limit: Option<u64>,
) -> mail_core::Result<Option<SentCopy>> {
    pool_op!(pool, cfg, password, |client| {
        ensure_sent_copy(&mut client, &bytes, message_id, append_limit).await
    })
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
        let swept = sweep_draft_mirrors(&app).await;
        if swept > 0 {
            log_line(&format!(
                "[easy-email] 清掉了 {swept} 份没有对应草稿的服务器副本"
            ));
            let _ = app.emit("outbox:changed", ());
        }

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
                Ok((report, filed)) => {
                    if report.sent > 0 {
                        // The send path deletes the draft it came from, which orphans its mirror row.
                        // Sweeping here (rather than only at startup) keeps the server copy from
                        // sitting in the user's 草稿箱 until the next launch.
                        sweep_draft_mirrors(&app).await;
                    }
                    if report.attempted > 0 {
                        log_line(&format!(
                            "[easy-email] 发件箱：本轮尝试 {} 封（成功 {} / 重试 {} / 放弃 {} / 待处理 {}）；\
                             已发送副本：归档 {} / 无法归档 {} / 待重试 {}",
                            report.attempted,
                            report.sent,
                            report.retried,
                            report.gave_up,
                            report.needs_attention,
                            filed.filed,
                            filed.skipped,
                            filed.deferred
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

/// Why the last server refresh failed, and how much is cached. Drives the status line.
///
/// `failure_kind` is what makes the notice actionable: "cannot reach the server" and "the server
/// refused the credential" need different responses from the user, and the text alone cannot be
/// classified by anyone reading it (`design/20-v0.2-plan.md` C3).
#[tauri::command]
async fn sync_status(state: State<'_, AppState>, account_id: String) -> Result<SyncStatus, String> {
    let failure = state
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
    let pending = user.pending_ops_outstanding().map_err(describe)?;

    Ok(SyncStatus {
        offline: failure.is_some(),
        failure_kind: failure.as_ref().map(|f| f.kind),
        last_error: failure.map(|f| f.detail),
        cached_total,
        drafts,
        pending_ops: pending,
        user_store_error: state
            .user_store_error
            .lock()
            .expect("user store error mutex")
            .clone(),
    })
}

/// The version of the build the user is running.
///
/// Exists because "which version am I on" had no answer in the UI at all: the only way to find out
/// was to look at the installer's filename, which is exactly the thing someone reporting a bug
/// cannot be asked to do. Compile-time, so it is always right and costs nothing.
#[tauri::command]
fn app_version() -> String {
    env!("CARGO_PKG_VERSION").to_string()
}

/// Open the release page in the system browser.
///
/// The same launcher the OAuth flow uses (argv, never a shell). This is the manual half of updating:
/// it works without a signing key, without a `latest.json`, and without trusting anything we host,
/// which is why it ships before automatic updates do (`design/20-v0.2-plan.md` V2-C1).
#[tauri::command]
async fn open_download_page() -> Result<(), String> {
    use mail_core::oauth::BrowserLauncher;
    mail_core::oauth::SystemBrowser
        .open("https://github.com/hq20211124/easy-email/releases/latest")
        .map_err(describe)
}

/// How much of this account has an indexed body — what search can actually see.
///
/// The number the search box shows. It exists because "search only covers opened mail" was invisible
/// on screen: the user typed a word, got nothing, and had no way to tell whether that meant "not
/// there" or "not indexed yet" (`design/20-v0.2-plan.md` V2-A).
#[tauri::command]
async fn index_coverage(
    state: State<'_, AppState>,
    account_id: String,
) -> Result<mail_core::BodyCoverage, String> {
    let cache = state.cache.clone();
    cache.body_coverage(&account_id).map_err(describe)
}

/// The bundle, as the renderer sees it before anything is written.
#[derive(serde::Serialize)]
struct DiagPreview {
    manifest: mail_core::diag::DiagManifest,
    preview: String,
    /// Where the export would land, so the preview can say it before it exists.
    planned_path: String,
}

/// Assemble a diagnostics bundle from what this installation actually has, previewing it.
///
/// Reads the two inputs the shell owns — the log file next to `startup-error.log` and the account
/// list — and hands them to the pure assembler. **Nothing is written.** The renderer shows the
/// result and asks, which is the entire point of `design/16` §3.3's `preview_only`.
#[tauri::command]
async fn diagnostics_preview(state: State<'_, AppState>) -> Result<DiagPreview, String> {
    let (log_text, settings_text, dir) = {
        let state = &state;
        (
            read_log_text(state),
            read_settings_text(state),
            state.accounts_path.parent().map(|p| p.to_path_buf()),
        )
    };
    let opts = mail_core::diag::DiagOptions {
        preview_only: true,
        ..mail_core::diag::DiagOptions::default()
    };
    let now = now_secs();
    let inputs = mail_core::diag::DiagInputs {
        app_version: env!("CARGO_PKG_VERSION").to_string(),
        os: std::env::consts::OS.to_string(),
        // The WebView2 version is not readable synchronously from here; saying "unknown" is better
        // than a number that would be a guess in a file whose whole purpose is to be trusted.
        webview: "unknown".to_string(),
        log_text,
        settings_text,
        sample_headers_text: String::new(),
    };
    let bundle = mail_core::diag::build_bundle(&inputs, &opts, now);
    let planned_path = dir
        .map(|d| d.join(format!("easy-email-diagnostics-{now}.txt")))
        .map(|p| p.display().to_string())
        .unwrap_or_default();
    Ok(DiagPreview {
        manifest: bundle.manifest,
        preview: bundle.preview,
        planned_path,
    })
}

/// Write the bundle, after the preview has been shown.
///
/// A separate command from the preview on purpose: one writes a file the user has seen and confirmed,
/// and the other must not be able to. Sharing an entry point would make "export" the easy typo away
/// from putting an unconfirmed copy of someone's mailbox on disk.
#[tauri::command]
async fn diagnostics_export(state: State<'_, AppState>) -> Result<String, String> {
    let (log_text, settings_text) = (read_log_text(&state), read_settings_text(&state));
    let dir = state
        .accounts_path
        .parent()
        .ok_or_else(|| "无法确定数据目录".to_string())?
        .to_path_buf();
    let opts = mail_core::diag::DiagOptions::default();
    let now = now_secs();
    let inputs = mail_core::diag::DiagInputs {
        app_version: env!("CARGO_PKG_VERSION").to_string(),
        os: std::env::consts::OS.to_string(),
        webview: "unknown".to_string(),
        log_text,
        settings_text,
        sample_headers_text: String::new(),
    };
    let bundle = mail_core::diag::build_bundle(&inputs, &opts, now);
    std::fs::create_dir_all(&dir).map_err(|e| format!("无法创建数据目录：{e}"))?;
    std::fs::write(
        dir.join(format!("easy-email-diagnostics-{now}.txt")),
        &bundle.preview,
    )
    .map_err(|e| format!("无法写入诊断包：{e}"))?;
    log_line(&format!(
        "诊断包已生成（{} 字节，{} 项已脱敏）",
        bundle.bytes, bundle.manifest.redactions
    ));
    Ok(dir
        .join(format!("easy-email-diagnostics-{now}.txt"))
        .display()
        .to_string())
}

/// The log file's contents, trimmed to the days the bundle is allowed to show.
fn read_log_text(_state: &AppState) -> String {
    // The log directory is a process-global set in `setup`, not a field on the state: the two
    // places that write it (this and the panic hook) must not be able to disagree about a path.
    let Some(dir) = PANIC_LOG_DIR.get() else {
        return String::new();
    };
    let Ok(text) = std::fs::read_to_string(dir.join("startup-error.log")) else {
        return String::new();
    };
    mail_core::diag::recent_log_lines(&text, 3, now_secs())
}

/// The account list, as stored. It goes through redaction like everything else.
fn read_settings_text(state: &AppState) -> String {
    std::fs::read_to_string(&state.accounts_path).unwrap_or_default()
}

/// What, if anything, is wrong with this query string, in words the user can act on.
///
/// A separate command rather than a field on the search result on purpose: `payload_keys` pins the
/// shape of the search contract, and changing that shape for a line of advice would be a poor trade.
/// The renderer only asks when the query contains a colon, so the extra call costs nothing in the
/// normal case.
#[tauri::command]
async fn search_hints(query: String) -> Result<Vec<String>, String> {
    Ok(mail_core::query::query_issues(&query))
}

/// One bounded pass of body indexing, so mail the user never opened becomes searchable.
///
/// Bounded on purpose: reading a body costs what opening the message costs, so this must not turn
/// into a whole-mailbox fetch that starves the user's own refresh. The desktop shell runs it on a
/// 60 s tick; the button in the search box runs it on demand.
#[tauri::command]
async fn index_bodies(
    state: State<'_, AppState>,
    account_id: String,
) -> Result<mail_core::index::IndexReport, String> {
    body_index_pass(&state, &account_id).await
}

/// Throw the search index away so the renderer can start rebuilding it.
///
/// **Deletes nothing the user owns**: the FTS table holds tokenised text derived from `messages`
/// and `bodies`, both of which stay exactly as they are (`design/09` §5.4). It is separate from
/// [`Self::index_bodies`] because the two answer different questions — that one goes to the network
/// for bodies the user never opened, this one only re-derives what is already on this machine.
#[tauri::command]
async fn rebuild_search_begin(state: State<'_, AppState>) -> Result<(), String> {
    let cache = state.cache.clone();
    cache.rebuild_search_begin().map_err(describe)
}

/// Rebuild one bounded batch of index rows and report where the rebuild now stands.
///
/// Both halves in one command on purpose: the renderer drives the loop, so it must not have to guess
/// whether a step finished before asking for the next one, and it must not be able to ask for a
/// second batch while the first is still running.
#[tauri::command]
async fn rebuild_search_step(
    state: State<'_, AppState>,
    limit: u32,
) -> Result<mail_core::RebuildProgress, String> {
    let cache = state.cache.clone();
    cache.rebuild_search_step(limit as usize).map_err(describe)
}

/// One pass for one account, shared by the command and the background tick.
///
/// Both callers need the same shape — claim a connection from the pool, walk the pending list, store
/// what comes back — and the pool's retry macro expands its block twice, so this lives in one
/// function rather than being written out at each call site.
async fn body_index_pass(
    state: &AppState,
    account_id: &str,
) -> Result<mail_core::index::IndexReport, String> {
    let cfg = state.find(account_id)?;
    let password = secret_for(state, &cfg).await?;
    let cache = state.cache.clone();

    let report = pool_op!(&state.pool, &cfg, &password, |client| {
        mail_core::index::index_bodies_pass(
            mail_core::index::BodyIndexRun {
                cache: &cache,
                cfg: &cfg,
                limit: mail_core::index::DEFAULT_PASS_LIMIT,
            },
            &mut client,
        )
        .await
    })
    .map_err(describe)?;

    if report.indexed > 0 {
        log_line(&format!(
            "[easy-email] 正文索引 {account_id}：新增 {} 封，还剩 {} 封",
            report.indexed, report.remaining
        ));
    }
    Ok(report)
}

/// Keep the body index moving on its own tick (`design/20-v0.2-plan.md` V2-A).
///
/// 60 s between passes because the per-message cost equals opening the message: slow and
/// interruptible beats a burst that holds the account's connection budget while the user is trying
/// to refresh. A pass that fails is simply not retried until the next tick — an offline account
/// must not spin.
fn start_body_index_worker(app: AppHandle) {
    tauri::async_runtime::spawn(async move {
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(60)).await;

            let accounts = app.state::<AppState>().snapshot();
            for cfg in accounts {
                // Ask the cache before claiming a connection: most ticks there is nothing to do, and
                // a pass on an account with no pending bodies would still pay for the handshake.
                let pending = match app.state::<AppState>().cache.bodies_pending_count(&cfg.id) {
                    Ok(count) => count,
                    Err(_) => continue,
                };
                if pending == 0 {
                    continue;
                }

                match body_index_pass(&app.state::<AppState>(), &cfg.id).await {
                    Ok(report) if report.indexed > 0 => {
                        // So the search box's coverage line stays honest without polling.
                        let _ = app.emit("body-index:changed", cfg.id.clone());
                    }
                    Ok(_) => {}
                    Err(error) => {
                        log_line(&format!("[easy-email] 正文索引 {} 失败：{error}", cfg.id));
                    }
                }
            }
        }
    });
}

/// The offline intent queue's tick.
///
/// Slower than the outbox tick on purpose: the queue is compensation for a period of being offline,
/// and the one thing it must not do is hammer a server that is only just back. Asking the store
/// first makes the common case (nothing queued) free — no credential, no connection, no handshake.
fn start_pending_replay_worker(app: AppHandle) {
    tauri::async_runtime::spawn(async move {
        // A previous run may have died between claiming an intent and recording the outcome. Every
        // intent in the queue is idempotent, so putting them back is the same answer the outbox
        // gives for its own crash recovery.
        {
            let state = app.state::<AppState>();
            match state.user.requeue_pending_ops(now_seconds()) {
                Ok(n) if n > 0 => log_line(&format!(
                    "[easy-email] 上一次运行留下 {n} 个没做完的离线意图，重新排队"
                )),
                _ => {}
            }
        }

        loop {
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
            if let Err(error) = pending_replay_pass(&app).await {
                log_line(&format!("[easy-email] 离线意图补做失败：{error}"));
            }
        }
    });
}

/// One bounded pass over every account's offline intent queue.
///
/// Runs **after** the outbox pass, which is the order `design/04` §5.5 asks for: send the mail
/// first, then fix the flags — otherwise a message the user archived while offline could be marked
/// read on the server before the message that move refers to exists. Both are idempotent, so the
/// ordering is about not looking strange to the user rather than about correctness.
///
/// An applier per account: the intents are applied through the same pooled connection the rest of
/// the app uses, so a pass that finds nothing costs one `pending_ops_outstanding()` per account and
/// no handshake at all.
async fn pending_replay_pass(app: &AppHandle) -> Result<(), String> {
    let (accounts, store, pool) = {
        let state = app.state::<AppState>();
        (state.snapshot(), state.user.clone(), state.pool.clone())
    };

    for account in accounts {
        // Asking the store first is what makes the empty case free: no credential, no connection.
        let outstanding = store.pending_ops_outstanding().unwrap_or(0);
        if outstanding == 0 {
            continue;
        }
        let password = match secret::load_password(&account.id) {
            Ok(Some(password)) => password,
            _ => continue,
        };

        struct AccountApplier<'a> {
            pool: &'a crate::ConnectionPool,
            cfg: &'a mail_core::model::AccountConfig,
            password: &'a str,
        }

        #[allow(async_fn_in_trait)]
        impl mail_core::pending::OpApplier for AccountApplier<'_> {
            async fn apply(
                &mut self,
                op: &mail_core::userdb::PendingOp,
            ) -> mail_core::error::Result<()> {
                use mail_core::userdb::PendingOpKind::*;
                let Some(uid) = op.uid else {
                    // An intent about a folder rather than a message. None exists yet, and guessing
                    // at one would apply something the user never asked for.
                    return Err(mail_core::error::MailError::Config(
                        "这个离线操作没有指定邮件".to_string(),
                    ));
                };
                let uid = u32::try_from(uid).unwrap_or(u32::MAX);
                pool_op!(self.pool, self.cfg, self.password, |client| {
                    match op.kind {
                        Some(MarkRead) => {
                            client.select(&op.folder).await?;
                            client.mark_seen(uid, true).await
                        }
                        Some(MarkUnread) => {
                            client.select(&op.folder).await?;
                            client.mark_seen(uid, false).await
                        }
                        Some(Delete) => {
                            client.select(&op.folder).await?;
                            client.delete_message(&op.folder, uid).await
                        }
                        Some(Move) => {
                            let target =
                                op.payload().and_then(|p| p.target_folder).ok_or_else(|| {
                                    mail_core::error::MailError::Config(
                                        "这个离线移动没有目标文件夹".to_string(),
                                    )
                                })?;
                            client.select(&op.folder).await?;
                            client.move_message(&op.folder, uid, &target).await
                        }
                        None => Err(mail_core::error::MailError::Config(
                            "这个版本不认识这种离线操作".to_string(),
                        )),
                    }
                })
            }
        }

        let mut applier = AccountApplier {
            pool: &pool,
            cfg: &account,
            password: &password,
        };
        let run = mail_core::pending::ReplayRun {
            store: &store,
            now: now_seconds(),
            limit: mail_core::pending::DEFAULT_REPLAY_LIMIT,
        };
        match mail_core::pending::replay_pending_ops(run, &mut applier).await {
            Ok(report) if report.applied + report.failed > 0 => {
                log_line(&format!(
                    "[easy-email] 离线意图补做 {}：应用 {} / 重试 {} / 还剩 {}",
                    account.id, report.applied, report.failed, report.remaining
                ));
                if report.remaining == 0 {
                    let _ = app.emit("pending-ops:changed", ());
                }
            }
            Ok(_) => {}
            Err(error) => {
                log_line(&format!(
                    "[easy-email] 离线意图补做 {} 失败：{error}",
                    account.id
                ));
            }
        }
    }
    Ok(())
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
    /// Which kind of failure, when there is one. `None` means the last refresh worked.
    failure_kind: Option<FailureKind>,
    last_error: Option<String>,
    cached_total: u32,
    /// Drafts on disk (they are the reason the user database exists).
    drafts: u32,
    /// Offline actions still waiting to reach the server.
    pending_ops: u32,
    /// Set when `user.db` could not be opened this run, with the reason.
    ///
    /// That fallback is the one place where the user's own data is not being read, so the window has
    /// to say so: an empty 草稿箱 otherwise reads as "my drafts are gone", which is the opposite of
    /// what happened — they are on disk, unread, because this version refuses to misread them.
    user_store_error: Option<String>,
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
    let password = secret_for(&state, &cfg).await?;

    let outcome = pool_op!(state.pool, &cfg, &password, |client| {
        client.select(&mailbox).await?;
        client.mark_seen(uid, seen).await
    });
    match outcome {
        Ok(()) => {}
        Err(error) => {
            // Offline is not a failure of the user's intent: the flag is applied locally now and the
            // queue carries it to the server when the connection is back (`design/04` §5.5). A
            // *refusal* is still a refusal — `is_network` is what tells the two apart.
            if !error.is_network() {
                return Err(describe(error));
            }
            queue_offline_intent(
                &state,
                &account_id,
                &mailbox,
                Some(uid),
                if seen {
                    mail_core::userdb::PendingOpKind::MarkRead
                } else {
                    mail_core::userdb::PendingOpKind::MarkUnread
                },
            )?;
        }
    }

    // Mirror it locally, or the cached list keeps showing the old flag next time it is opened
    // offline and the UI disagrees with the server.
    state
        .cache
        .clone()
        .set_seen(&account_id, &mailbox, uid, seen)
        .map_err(describe)?;
    Ok(())
}

/// Put a mutating intent on the offline queue, so the server catches up when it can.
///
/// Only called for failures [`MailError::is_network`] accepts, which is why it does not need to
/// decide anything: a queue that also swallowed refusals would hide them behind retries that can
/// never succeed. The queue lives in `user.db`, so it survives a restart — the intent outlives the
/// process that made it.
fn queue_offline_intent(
    state: &AppState,
    account_id: &str,
    folder: &str,
    uid: Option<u32>,
    kind: mail_core::userdb::PendingOpKind,
) -> Result<(), String> {
    let store = state.user.clone();
    store
        .enqueue_pending_op(
            account_id,
            folder,
            uid.map(u64::from),
            kind,
            &mail_core::userdb::PendingPayload::default(),
            now_secs(),
        )
        .map_err(describe)?;
    Ok(())
}

/// Unix seconds, in one place, so a queued row and its backoff agree on what "now" is.
fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[tauri::command]
async fn delete_message(
    state: State<'_, AppState>,
    account_id: String,
    mailbox: String,
    uid: u32,
) -> Result<(), String> {
    let cfg = state.find(&account_id)?;
    let password = secret_for(&state, &cfg).await?;

    let outcome = pool_op!(state.pool, &cfg, &password, |client| {
        client.delete_message(&mailbox, uid).await
    });
    match outcome {
        Ok(()) => {}
        Err(error) => {
            if !error.is_network() {
                return Err(describe(error));
            }
            queue_offline_intent(
                &state,
                &account_id,
                &mailbox,
                Some(uid),
                mail_core::userdb::PendingOpKind::Delete,
            )?;
        }
    }

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
    let password = secret_for(&state, &cfg).await?;

    let outcome = pool_op!(state.pool, &cfg, &password, |client| {
        client.move_message(&mailbox, uid, &destination).await
    });
    match outcome {
        Ok(()) => {}
        Err(error) => {
            if !error.is_network() {
                return Err(describe(error));
            }
            // `move` carries its destination in the payload, so the replay knows where to put it.
            let payload = mail_core::userdb::PendingPayload {
                target_folder: Some(destination.clone()),
            };
            let store = state.user.clone();
            store
                .enqueue_pending_op(
                    &account_id,
                    &mailbox,
                    Some(u64::from(uid)),
                    mail_core::userdb::PendingOpKind::Move,
                    &payload,
                    now_secs(),
                )
                .map_err(describe)?;
        }
    }
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
async fn start_watcher(
    app: &AppHandle,
    state: &AppState,
    cfg: AccountConfig,
    mailbox: String,
) -> Result<(), String> {
    let password = secret_for(state, &cfg).await?;
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
        // This task outlives the call, so it must not borrow `state`; the handle can produce one, and
        // that is also what keeps the reference valid for as long as the task runs.
        let state = app.state::<AppState>();
        while let Some(event) = rx.recv().await {
            match event {
                PushEvent::Changed { mailbox } => {
                    // Sync on a *different* pooled connection; the IDLE connection stays idle.
                    match refresh_folder(state.inner(), &pool, &cache, &cfg, &mailbox).await {
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
    start_watcher(&app, state.inner(), cfg, mailbox).await
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
    // The user can silence one account without silencing the rest (the preference is per account and
    // defaults to on). Checked here rather than at the call site so no future path can bypass it.
    let notify = app
        .state::<AppState>()
        .user
        .notify_enabled(&cfg.id)
        .unwrap_or(true);
    if !notify {
        return;
    }

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

        // Hash now, while the user is looking at the file they just chose: that is the version they
        // approved, and it is the only chance to record it. A file too large or too odd to hash is
        // added without one — the send path then hashes it instead of refusing the attachment.
        let sha256 = file_sha256(&path).ok();

        chosen.push(OutgoingAttachment {
            name: path
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("附件")
                .to_string(),
            mime: guess_mime(&path),
            size: metadata.len(),
            path: path.to_string_lossy().to_string(),
            sha256,
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
    let password = secret_for(&state, &cfg).await?;

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

    // Bidirectional overrides and zero-width characters come out **first**, before the replacement
    // table below. They are `Cf`, not `Cc`, so `is_control()` does not see them — and they are how a
    // file called `invoice\u{202E}gnp.exe` displays as `invoiceexe.png`. Deleting them is the only
    // honest fix: there is no legitimate reason for a filename to contain one.
    let leaf = mail_core::sanitize::strip_invisible_controls(leaf);

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
        // Truncating the whole string would drop the `.pdf` and leave a file nothing will open —
        // which is what the line above always promised and the code never did. The extension is
        // preserved when it is short enough to be a real one.
        let extension = Path::new(&cleaned)
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| format!(".{e}"))
            .filter(|e| e.chars().count() < 16);
        let budget = MAX - extension.as_ref().map_or(0, |e| e.chars().count());
        let stem: String = cleaned.chars().take(budget).collect();
        cleaned = match extension {
            Some(extension) => format!("{stem}{extension}"),
            None => stem,
        };
    }

    let upper = cleaned.to_ascii_uppercase();
    // The full Windows set, not just the first few: `COM5` and `LPT3` fail exactly like `COM1`, and
    // `CONIN$`/`CONOUT$`/`CLOCK$` are device names that look like ordinary files. Missing entries
    // here do not produce a security hole — they produce a save that fails with a bewildering error.
    let reserved = [
        "CON", "PRN", "AUX", "NUL", "CONIN$", "CONOUT$", "CLOCK$", "COM1", "COM2", "COM3", "COM4",
        "COM5", "COM6", "COM7", "COM8", "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6",
        "LPT7", "LPT8", "LPT9",
    ];
    // Compare the stem, since `NUL.txt` is just as unusable as `NUL`.
    let stem = upper.split('.').next().unwrap_or("");
    if reserved.contains(&stem) || upper.contains('$') {
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

// The rotation round (86) made the file bounded; reading it back is the other half of that
// promise — a user asking "what has the app been doing" gets an answer, in the same redacted form
// the file is written in. Reading it through the same redactor is deliberate: the file was
// redacted on the way in, so a second pass must not change it, and if one ever does, that is a bug
// worth seeing rather than hiding (see the counter below).

/// The last `n` lines of the app log, already redacted.
#[tauri::command]
async fn log_tail(state: State<'_, AppState>, lines: u32) -> Result<String, String> {
    let _ = &state;
    let Some(dir) = PANIC_LOG_DIR.get() else {
        return Ok(String::new());
    };
    let Ok(text) = std::fs::read_to_string(dir.join("startup-error.log")) else {
        // No log yet is not an error: a fresh install has nothing to say.
        return Ok(String::new());
    };
    // Capped like the design says (500), and clamped so a caller cannot ask for the whole file.
    let n = lines.clamp(1, 500) as usize;
    let tail = mail_core::logrot::tail_lines(&text, n).join("\n");
    let (redacted, hidden) = mail_core::redact::redact(&tail);
    if hidden > 0 {
        // The file was redacted when written, so this should be zero. If it is not, say so rather
        // than quietly showing the user a second layer of masking they did not ask for.
        log_line(&format!("读日志时又掩掉 {hidden} 项（写入时应当已掩）"));
    }
    Ok(redacted)
}

/// Format one log line, redacted.
///
/// Split out of [`log_line`] because the redaction is the part with rules worth testing and the file
/// append is the part that needs a filesystem. Returns the line and how many things were hidden —
/// the count is appended to the line when it is not zero, so a reader who compares their own actions
/// against the file sees *why* the line differs from what they expected instead of concluding the log
/// lied to them.
fn format_log_line(message: &str, now: i64) -> (String, usize) {
    let (redacted, hidden) = mail_core::redact::redact(message);
    let note = if hidden > 0 {
        format!(" [已脱敏 {hidden} 项]")
    } else {
        String::new()
    };
    (format!("[{now}] {redacted}{note}\n"), hidden)
}

/// Rotate the log when it has reached the cap, so the file a diagnostic bundle reads is the size of
/// the problem rather than the size of the app's history.
///
/// Three moves and one open: drop the oldest, shift `.n-1` to `.n`, move the current file to `.1`,
/// and let the caller's `append` create a fresh one. Every failure is **ignored on purpose**: a log
/// must never be the reason the app does not start, and a failed rotation costs a bigger file until
/// the next write, which is the failure this whole mechanism exists to make rare.
fn rotate_log_if_needed(dir: &std::path::Path) {
    use mail_core::logrot::{KEEP_LOG_FILES, MAX_LOG_BYTES, rotated_names, rotation_action};

    let current = dir.join("startup-error.log");
    let Ok(metadata) = std::fs::metadata(&current) else {
        return; // No file yet: nothing to rotate, and the caller is about to create it.
    };
    if !rotation_action(metadata.len(), MAX_LOG_BYTES) {
        return;
    }

    let names = rotated_names(KEEP_LOG_FILES);
    // From the oldest end so nothing is overwritten before it has been moved aside.
    if let Some(oldest) = names.last() {
        let _ = std::fs::remove_file(dir.join(format!("startup-error{oldest}")));
    }
    for n in (1..names.len()).rev() {
        let from = dir.join(format!("startup-error{}", names[n - 1]));
        let to = dir.join(format!("startup-error{}", names[n]));
        if from.exists() {
            let _ = std::fs::rename(from, to);
        }
    }
    let rotated = dir.join(format!("startup-error{}", names[0]));
    let _ = std::fs::rename(&current, rotated);
}

/// Append one diagnostic line to `startup-error.log`.
///
/// The desktop build has no console (see the `windows_subsystem` note in `main.rs`), so anything
/// that used to be an `eprintln!` would otherwise disappear without a trace. Falls back to stderr,
/// which is visible when the app is started with `--console`.
///
/// **Everything is redacted on the way in.** This file is the first thing anyone attaches to a bug
/// report, so the secret that reaches it is the secret that leaves the machine — and the caller
/// should not have to remember that (53 call sites do not, and one day one of them would).
///
/// And it **rotates**: the file is capped at 8 MB with seven generations kept (`design/16` §3.4),
/// checked before every write — a cap checked only at startup would let a long session fill a file
/// nobody trims until the next launch.
fn log_line(message: &str) {
    let seconds = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0);
    let (line, _) = format_log_line(message, seconds as i64);

    let stored = PANIC_LOG_DIR.get().is_some_and(|dir| {
        rotate_log_if_needed(dir);
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

#[cfg(test)]
mod log_tests {
    use super::format_log_line;

    #[test]
    fn a_normal_line_is_written_unchanged() {
        let (line, hidden) = format_log_line("同步 INBOX：新增 5 封", 1_800_000_000);
        assert_eq!(hidden, 0);
        assert_eq!(line, "[1800000000] 同步 INBOX：新增 5 封\n");
    }

    #[test]
    fn a_line_with_a_secret_is_written_redacted_and_says_so() {
        let (line, hidden) = format_log_line(
            "登录失败：bestbychinas@gmail.com 用了授权码 abcdefghij123456",
            1_800_000_000,
        );
        assert_eq!(hidden, 2, "the address and the code");
        assert!(!line.contains("bestbychinas"));
        assert!(!line.contains("abcdefghij123456"));
        // The domain survives: it is the half a maintainer can act on.
        assert!(line.contains("@gmail.com"));
        // And the reader is told the line was altered, and how much.
        assert!(line.contains("[已脱敏 2 项]"), "got {line}");
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
        // Reads its pubkey and endpoint from the config, so the verification is not something this
        // code can get wrong: there is no path where a downloaded bundle reaches the installer
        // unsigned.
        .plugin(tauri_plugin_updater::Builder::new().build())
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
            // Kept so the window can say "your drafts are on disk but this version will not read
            // them" instead of showing an empty list (see `SyncStatus::user_store_error`).
            let mut user_store_error: Option<String> = None;
            let user = match UserStore::open(&user_path) {
                Ok(store) => store,
                Err(e) => {
                    // Loud on purpose: this is the one fallback where the user's own data is *not*
                    // read (a corrupt file, or one written by a newer version — `UserStore` refuses
                    // that rather than misreading it). Drafts and queued sends will not appear, and
                    // anything saved during this run goes to a database that is thrown away, so the
                    // reason belongs in the log instead of being inferred from an empty list.
                    // (Putting it in front of the user in the window is the next step.)
                    log_line(&format!(
                        "[easy-email] 用户数据未能打开，本次运行不读写草稿/发件箱：{e}"
                    ));
                    user_store_error = Some(e.to_string());
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
                tokens: mail_core::oauth::TokenCache::new(),
                cache,
                last_sync_error: Mutex::new(None),
                pool: Arc::new(ConnectionPool::new()),
                watchers: Mutex::new(HashMap::new()),
                download_dir,
                user,
                // Its own directory under the app's config dir, next to the two databases: a spool
                // file is disposable, but it must survive a restart to keep a queued send's bytes.
                spool,
                user_store_error: Mutex::new(user_store_error),
                outbox_wake: Arc::new(Notify::new()),
                pending_update: std::sync::Mutex::new(None),
            });

            // Start the outbox worker. Queued messages outlive the window, so this is what actually
            // sends them: the composer only commits them to `user.db`.
            start_outbox_worker(app.handle().clone());
            // The offline intent queue. It runs after the outbox worker is started, and its pass
            // runs after the outbox pass inside its own loop, because `design/04` §5.5 wants mail
            // sent before flags are fixed.
            start_pending_replay_worker(app.handle().clone());
            start_body_index_worker(app.handle().clone());

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
                    // `setup` runs outside any Tokio runtime, so the watchers are started on the
                    // runtime instead of being awaited here (spawning from there panics).
                    let handle = app.handle().clone();
                    tauri::async_runtime::spawn(async move {
                        let state = handle.state::<AppState>();
                        if let Err(error) =
                            start_watcher(&handle, state.inner(), account, "INBOX".into()).await
                        {
                            log_line(&format!("[easy-email] 未能启动 {id} 的推送：{error}"));
                        }
                    });
                }
            }
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            list_providers,
            suggest_provider,
            build_account,
            endpoint_choices,
            list_accounts,
            save_account,
            delete_account,
            verify_account,
            list_mailboxes,
            app_version,
            check_update,
            index_bodies,
            index_coverage,
            install_update,
            message_page,
            rebuild_search_begin,
            rebuild_search_step,
            search_hints,
            diagnostics_preview,
            log_tail,
            diagnostics_export,
            list_messages,
            list_thread,
            get_sync_window,
            set_sync_window,
            backfill_to_window,
            save_draft,
            list_drafts,
            delete_draft,
            mirror_draft,
            draft_mirror,
            range_cap_hint,
            max_send_bytes,
            classify_link,
            open_download_page,
            open_link,
            get_notify_enabled,
            set_notify_enabled,
            refresh_thread_keys,
            quota_status,
            oauth_sign_in,
            oauth_client_default,
            oauth_relogin,
            take_update_outcome,
            enqueue_send,
            list_outbox,
            cancel_send,
            retry_send,
            discard_outbox,
            sent_copy,
            load_more_messages,
            fetch_message,
            mark_seen,
            folder_unread_counts,
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

#[cfg(test)]
mod tests {
    use super::{FailureKind, SyncFailure, app_version, failure_kind, safe_file_name};

    /// Hostile attachment names, table-driven.
    ///
    /// This function had no test at all, in a crate that otherwise has almost none, while `README`
    /// describes filenames as the hostile input it defends against. Every row here is a name a
    /// sender can put in `Content-Disposition` (`design/14` §9).
    #[test]
    fn hostile_attachment_names_are_neutralised() {
        let cases: &[(&str, &str)] = &[
            // Path traversal, both separators and a drive-qualified path.
            ("../../evil.exe", "evil.exe"),
            (r"..\..\evil.exe", "evil.exe"),
            (r"C:\Windows\System32\calc.exe", "calc.exe"),
            ("/etc/passwd", "passwd"),
            // A trailing dot or space is dropped by Windows and can defeat an extension check.
            ("evil.exe.", "evil.exe"),
            ("evil.exe   ", "evil.exe"),
            // Reserved device names, including the ones the old list missed.
            ("CON", "file-CON"),
            ("nul.txt", "file-nul.txt"),
            ("COM5", "file-COM5"),
            ("LPT9.log", "file-LPT9.log"),
            ("CONIN$", "file-CONIN$"),
            // Characters that cannot appear in a Windows name.
            (r#"a<b>c:d"e.txt"#, "a_b_c_d_e.txt"),
            // A bidi override that makes `invoicegnp.exe` *display* as `invoiceexe.png`.
            ("invoice\u{202E}gnp.exe", "invoicegnp.exe"),
            ("a\u{200B}b.txt", "ab.txt"),
            // Nothing usable left.
            ("", "attachment-3"),
            ("...", "attachment-3"),
            ("///", "attachment-3"),
            // Ordinary names are not mangled.
            ("report.pdf", "report.pdf"),
            ("年度报表 2026.xlsx", "年度报表 2026.xlsx"),
        ];

        for (raw, expected) in cases {
            assert_eq!(&safe_file_name(raw, 3), expected, "raw name: {raw:?}");
        }
    }

    #[test]
    fn a_long_name_keeps_its_extension() {
        let long = format!("{}.pdf", "a".repeat(300));
        let name = safe_file_name(&long, 1);
        assert_eq!(name.chars().count(), 120);
        assert!(name.ends_with(".pdf"), "the extension must survive: {name}");

        // A name with no extension is simply cut.
        let long = "b".repeat(300);
        let name = safe_file_name(&long, 1);
        assert_eq!(name.chars().count(), 120);
    }

    /// **V2-B.** A refresh failure is classified by what the user can do about it.
    ///
    /// The whole point of `FailureKind` is that "cannot connect" and "credential refused" are
    /// different situations, and a sentence cannot be classified by anyone reading it. This pins each
    /// `MailError` variant to the one remedy it deserves — including the pair that is easiest to get
    /// backwards: an auth failure arrives as an `Auth`, not as a `Connect`, and telling that user to
    /// check their network sends them to the wrong place.
    #[test]
    fn every_failure_kind_points_at_a_different_remedy() {
        use mail_core::error::{AuthHint, MailError};

        let io = std::io::Error::new(std::io::ErrorKind::TimedOut, "no route");
        let auth = MailError::Auth {
            server: "imap.qq.com".into(),
            raw: "Invalid credentials".into(),
            hint: AuthHint::Unknown,
        };

        assert_eq!(
            failure_kind(&MailError::Connect {
                host: "imap.qq.com".into(),
                port: 993,
                source: io
            }),
            FailureKind::Network
        );
        // `Tls` maps to `Network` too. It is not constructed here: the error type wraps a
        // `native_tls::Error` with no public constructor, and building one would mean performing a
        // handshake against something. The mapping is one line of the same match arm, and
        // `describe()` already covers the wording.
        assert_eq!(failure_kind(&auth), FailureKind::Auth);
        assert_eq!(
            failure_kind(&MailError::Busy("database is locked".into())),
            FailureKind::Busy
        );
        assert_eq!(failure_kind(&MailError::Timeout(30)), FailureKind::Busy);
        assert_eq!(
            failure_kind(&MailError::LocalDb("user.db 打不开".into())),
            FailureKind::Local
        );
        assert_eq!(
            failure_kind(&MailError::Cache("cache.db 打不开".into())),
            FailureKind::Local
        );
        assert_eq!(
            failure_kind(&MailError::Imap("命令被拒".into())),
            FailureKind::Server
        );
        assert_eq!(
            failure_kind(&MailError::Smtp("550 拒收".into())),
            FailureKind::Server
        );
        assert_eq!(
            failure_kind(&MailError::Config("端口写错".into())),
            FailureKind::Server
        );

        // And the two that must never collapse into each other.
        assert_ne!(
            failure_kind(&MailError::Connect {
                host: "h".into(),
                port: 993,
                source: std::io::Error::new(std::io::ErrorKind::TimedOut, "x")
            }),
            FailureKind::Auth,
            "a dead network is not a bad credential"
        );
    }

    /// **V2-C1.** The version shown is the version that was compiled in.
    ///
    /// Not a tautology: it pins that the command reads the manifest rather than a second copy of the
    /// number that could drift, which is the failure that would make a bug report say the wrong
    /// build. It also matches the workspace `Cargo.toml`, so a forgotten version bump shows up here
    /// rather than in a user's screenshot.
    #[test]
    fn app_version_matches_the_manifest() {
        assert_eq!(app_version(), env!("CARGO_PKG_VERSION"));
        assert!(!app_version().is_empty());
    }

    /// A `SyncFailure` prints the sentence and nothing else: the kind is for the machine half of the
    /// contract, and putting it in the prose is how a user ends up reading "auth" in a message.
    #[test]
    fn a_sync_failure_prints_only_the_sentence() {
        let failure = SyncFailure {
            kind: FailureKind::Auth,
            detail: "imap.qq.com 认证失败".into(),
        };
        assert_eq!(failure.to_string(), "imap.qq.com 认证失败");
        assert!(
            !failure.to_string().contains("auth"),
            "the kind must stay out of the text"
        );
    }
}
