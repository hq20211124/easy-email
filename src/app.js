/* Easy Email — renderer.
 *
 * Plain ES2020, no bundler: Tauri loads this directory directly as the frontend dist, which
 * keeps the build to a single `cargo build`.
 *
 * The one security-critical piece is renderSandboxedHtml(). Mail is hostile input. Bodies are
 * never inserted into this document; they go into an iframe with `sandbox` (no scripts, no
 * forms, opaque origin) plus a CSP that forbids every network fetch. Remote images are blocked
 * by default because loading them is how tracking pixels report that you opened a message.
 */

const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

/**
 * Payload shapes for the commands that take a struct (`send_mail`, `save_draft`).
 *
 * Defined in payload.js so `tools/check.ps1` can compare them against the Rust structs: a
 * camelCase field there is a runtime `missing field` and nothing else catches it.
 */
const payload = window.__easyEmailPayload;

/** Conversation grouping; pure functions in threads.js so `tools/thread_keys_test.mjs` can pin them. */
const threads = window.__easyEmailThreads;

/**
 * Who a conversation reply answers, and the headers that keep it in the thread.
 *
 * Pure functions in reply.js, for the same reason: "reply to the newest message" addresses the user
 * themselves whenever they replied last, and a reply without the `References` chain arrives as a brand
 * new conversation. `tools/reply_test.mjs` pins both.
 */
const reply = window.__easyEmailReply;

/**
 * Which source the next page of older mail comes from (cache first, then the server, then "no more").
 *
 * Pure, in history.js, because the wrong order is invisible on screen — both branches end with more
 * rows in the list — and it silently skipped everything the cache held beyond the first page.
 */
const history = window.__easyEmailHistory;

const PAGE_SIZE = 50;

/** How many local search hits to ask for. Higher than a page because results span all folders. */
const SEARCH_LIMIT = 200;

/** Typing pause before a search runs. Search is local, but it still hits SQLite on every keystroke. */
const SEARCH_DEBOUNCE_MS = 250;

/** Typing pause before the open composer writes itself to `user.db` (design/07 §3). */
const DRAFT_AUTOSAVE_MS = 2000;

/**
 * How long a queued message can still be called back before the worker sends it (design/07 §5.2).
 *
 * The same number the backend defaults to, and shown to the user while it counts down: an undo that
 * has already silently expired would be worse than no undo at all.
 */
const UNDO_SECONDS = 5;

const state = {
  providers: [],
  accounts: [],
  accountId: null,
  mailboxes: [],
  mailbox: null,
  /**
   * What the middle pane is showing: 'mail' (a folder, or search hits), 'drafts' or 'outbox'.
   *
   * Kept in state rather than inferred from `mailbox`, because neither local list is a folder: they
   * have no UID, they survive the server being unreachable, and clicking a row opens a composer or a
   * retry rather than a reader.
   */
  view: 'mail',
  /** Local drafts for the current account, most recently edited first. */
  drafts: [],
  /** Queued and finished sends for the current account, newest first. */
  outbox: [],
  /** The sync window the user chose; `null` until it has been read. */
  syncWindow: null,
  /**
   * Bytes this account may send, or `null` before it has been read.
   *
   * Read from the backend rather than kept here as a constant: the cap is a policy about providers
   * (`design/07` §6), and a second copy in the renderer is a second thing to get wrong.
   */
  sendCap: null,
  /** Decoded mailbox name for display; `mailbox` stays in wire form for commands. */
  mailboxLabel: null,
  /**
   * Unread message count per folder (wire name -> count), from the cache.
   *
   * A folder with nothing unread is absent rather than zero, so a badge only ever appears when it
   * carries information — a row of zeroes is decoration, and a badge that is always there stops
   * being read. Absent for a folder that has never been synced, which is why an account added five
   * seconds ago shows no badges at all.
   */
  unreadCounts: {},
  messages: [],
  /**
   * The list's windowed content: one descriptor per conversation row, plus one per message of each
   * expanded conversation (`design/15` §6 — only the viewport ± 8 rows is ever in the DOM).
   *
   * Descriptors rather than elements because the window changes on every scroll: rebuilding
   * descriptions would throw away the fold state and the selection every frame.
   */
  listRows: [],
  /** Which view owns `#message-list` right now; the scroll handler only acts on `'mail'`. */
  listOwner: null,
  /** The window currently in the DOM, so a scroll can be skipped when it changes nothing. */
  listWindow: null,
  /**
   * Set once the server says there is nothing older than what we already have.
   *
   * Without it the "load more" button would keep asking forever on a folder whose oldest message
   * has been reached — each click a wasted round trip, and no way for the user to tell "still
   * loading" from "this is everything".
   */
  historyDone: false,
  selectedUid: null,
  /**
   * Folder the selected message lives in.
   *
   * Tracked separately from `mailbox` because a search result can be opened without leaving search
   * mode, and two folders can hold the same UID — highlighting on UID alone would light up the
   * wrong row.
   */
  selectedFolder: null,
  /**
   * The summary of the message the reader is showing, if any.
   *
   * Held rather than looked up in `state.messages`, because the reader's message does not have to be
   * in the list being displayed: a conversation spans folders, and a search hit is not in the folder
   * on screen either. Everything that acts on "the open message" reads this.
   */
  selectedMessage: null,
  detail: null,
  /**
   * The conversation the reader is showing, oldest first; `[]` when the open message is not part of one.
   *
   * Read from the cache by the backend, across folders — the user's own replies live in the Sent
   * folder, and a conversation assembled from one folder would look complete while showing only half
   * of it.
   */
  threadMembers: [],
  /** The `References` root of the open conversation, or `null`. Used to keep the list row highlighted. */
  threadKey: null,
  /** `folder|uid` of the bubble whose body is rendered in full, or `null` when all are collapsed. */
  threadFocus: null,
  /** Bodies already fetched for the open conversation, keyed `folder|uid`. */
  threadBodies: new Map(),
  /**
   * Which reading view the user chose — one switch for the whole app, remembered across restarts.
   *
   * Global on purpose: the user asked for "模式可以切换，选了一个模式就全是这个模式", and a per-thread
   * memory would make the same list behave differently from one row to the next. Filled in by
   * `loadReaderMode()` at boot; the literal here is only the shape.
   */
  readerMode: 'single',
  bodyMode: 'html',
  /** Per-message opt-in; reset every time a different message is opened. */
  allowRemoteImages: false,
  /**
   * Conversations the user has opened in the list, by thread key.
   *
   * A fold is a view state, not a preference: it is cleared when the list is rebuilt for a different
   * folder or a search, because the same key in another folder is a different set of messages.
   */
  expandedThreads: new Set(),  /** Non-empty means the list is showing cache hits rather than the current folder. */
  searchQuery: '',
  searchHits: [],
  /**
   * Queries this account asked to keep (`design/09` §1 goal 7). Each carries its `account_id`
   * through to the renderer because some are global — shown for every account — and the sidebar has
   * to label those rather than let them look like they followed the user.
   */
  savedSearches: [],
};

/* ---------------------------------------------------------------- utilities */

const $ = (id) => document.getElementById(id);

function escapeHtml(value) {
  return String(value ?? '')
    .replace(/&/g, '&amp;')
    .replace(/</g, '&lt;')
    .replace(/>/g, '&gt;')
    .replace(/"/g, '&quot;')
    // The single quote as well. Every interpolation today sits in a double-quoted attribute, so this
    // is latent rather than a live hole — but the next person who writes title='${escapeHtml(x)}'
    // should not be the one who finds out.
    .replace(/'/g, '&#39;');
}

function toast(message, kind = 'info', ttl = 5000, action = null) {
  const el = document.createElement('div');
  el.className = `toast ${kind}`;
  el.textContent = message;

  // An action turns a notification into a decision. The undo window is the reason this exists: the
  // message is already committed to the outbox, and for a few seconds the user can still call it
  // back — but only if the button is in front of them while the window is open.
  if (action) {
    const button = document.createElement('button');
    button.type = 'button';
    button.className = 'btn ghost toast-action';
    button.textContent = action.label;
    button.addEventListener('click', () => {
      el.remove();
      action.onClick();
    });
    el.appendChild(button);
  }

  $('toast-root').appendChild(el);
  setTimeout(() => el.remove(), ttl);
  return el;
}

/** Unwrap a Tauri command error into a readable string. */
function errText(error) {
  if (typeof error === 'string') return error;
  if (error && typeof error.message === 'string') return error.message;
  return String(error);
}

/** 99+ once a folder is deep enough that the exact number stops being useful. */
function formatUnread(count) {
  return count > 99 ? '99+' : String(count);
}

function formatDate(unixSeconds) {
  if (!unixSeconds) return '';
  const date = new Date(unixSeconds * 1000);
  const now = new Date();
  const sameDay = date.toDateString() === now.toDateString();
  if (sameDay) {
    return date.toLocaleTimeString('zh-CN', { hour: '2-digit', minute: '2-digit' });
  }
  if (date.getFullYear() === now.getFullYear()) {
    return date.toLocaleDateString('zh-CN', { month: 'numeric', day: 'numeric' });
  }
  return date.toLocaleDateString('zh-CN', { year: 'numeric', month: 'numeric', day: 'numeric' });
}

function formatBytes(bytes) {
  if (!bytes) return '';
  const units = ['B', 'KB', 'MB', 'GB'];
  let value = bytes;
  let unit = 0;
  while (value >= 1024 && unit < units.length - 1) {
    value /= 1024;
    unit += 1;
  }
  return `${value.toFixed(unit === 0 ? 0 : 1)} ${units[unit]}`;
}

function senderLabel(message) {
  return message.from_name || message.from_addr || '(未知发件人)';
}

/** Wrap a command so a failure always surfaces as a toast rather than a silent no-op. */
async function run(label, fn) {
  try {
    return await fn();
  } catch (error) {
    toast(`${label}：${errText(error)}`, 'err', 9000);
    return null;
  }
}

/* ---------------------------------------------------------------- boot */

async function boot() {
  state.readerMode = loadReaderMode();
  state.providers = (await run('读取内置邮箱配置', () => invoke('list_providers'))) ?? [];
  await refreshSyncWindow();
  await refreshAccounts();
  wireGlobalEvents();
  syncThemeButton();
  syncViewModeButton();
  // The saved-search rows are part of the sidebar from the first paint: on a fresh install they are
  // the empty-state line that tells the user the feature exists, rather than an absence.
  await refreshSavedSearches();
  await wirePushEvents();
  // What the last update attempt actually did, once per launch. A silent failure here is how
  // somebody presses "restart to update" forever.
  await reportUpdateOutcome();
  // One timer for the whole window, rather than one per open composer or per outbox row: it is a
  // text update on whatever countdown is on screen, and it does nothing when none is.
  setInterval(updateUndoCountdowns, 1000);
  // The version is compile-time in Rust; showing it here means the one number a bug report needs
  // is on screen without anyone having to open a dialog.
  $('about-version').textContent =
    (await run('读取版本号', () => invoke('app_version'))) ?? '';

  if (state.accounts.length === 0) {
    openAccountWizard();
  } else {
    await warnIfUserDataUnavailable();
  }
}

/**
 * Say so when `user.db` could not be opened (`design/03` §11 反驳 5).
 *
 * That fallback is the only one where the user's own data is not being read — the drafts are on disk,
 * this version simply refuses to misread them. Without this the window shows an empty 草稿箱, which
 * reads as "my drafts are gone": the opposite of what happened, and the one thing that would make
 * somebody stop trusting the app.
 */
async function warnIfUserDataUnavailable() {
  const status = await invoke('sync_status', { accountId: state.accountId }).catch(() => null);
  if (status?.user_store_error) {
    toast(`草稿与发件箱这次没有打开：${status.user_store_error}`, 'err', 20000);
  }
}

/**
 * React to the backend's IMAP IDLE push.
 *
 * The backend owns the connection and the credentials; this only decides what the window does about
 * it. A notification has already been shown by then, so this is about the list staying current
 * without the user pressing 刷新.
 */
async function wirePushEvents() {
  await listen('mail:new', (event) => {
    const update = event.payload ?? {};
    const where = folderLabel(update.mailbox);

    if (update.account_id === state.accountId && update.mailbox === state.mailbox) {
      // The folder on screen: pull it in. `loadMessages` re-reads the cache, which the backend has
      // already updated, so this costs no extra network round trip.
      loadMessages();
    }

    toast(
      `新邮件：${where} 收到 ${update.new_messages} 封（未读 ${update.unread}）`,
      'info',
      8000
    );
  });

  // Tray menu → "立即检查新邮件".
  await listen('mail:check', () => {
    if (state.accountId && state.mailbox) loadMessages({ reset: true });
  });

  // The outbox worker → "something in the queue moved".
  //
  // The message may have been sent, failed, or given up after a retry, and the window has no way to
  // know which: it re-reads `user.db` and shows what is actually there. Drafts come along because a
  // successful send deletes the draft it came from.
  await listen('outbox:changed', () => {
    refreshOutbox();
    refreshDrafts();
  });
}

/**
 * Point the toggle at the theme it would switch to.
 *
 * The icon is applied before first paint by theme.js; this only syncs the button to whatever that
 * decided, so the two can never disagree after a restart.
 */
function syncThemeButton() {
  const button = $('btn-theme');
  const theme = window.__easyEmailTheme.current();
  const label = window.__easyEmailTheme.labelFor(theme);
  button.textContent = window.__easyEmailTheme.iconFor(theme);
  button.title = label;
  button.setAttribute('aria-label', label);
}

/* ---------------------------------------------------------------- reading view mode */

const READER_MODE_KEY = 'ee.readerMode';

/**
 * The reading view the user last chose.
 *
 * `localStorage`, for the same reason the theme lives there: it is a view preference, not account data,
 * and it belongs to this installation rather than to a mailbox. As with the theme, the "never chosen"
 * state is left unwritten on startup, so a default can change later without overriding a real choice.
 *
 * The default is **普通模式** (user's request, 2026-10-06): the conversation view is opt-in, and the
 * literal here is what a fresh installation gets.
 */
function loadReaderMode() {
  try {
    const stored = localStorage.getItem(READER_MODE_KEY);
    if (stored === 'single' || stored === 'thread') return stored;
  } catch {
    // A locked-down profile can refuse storage; the default is a fine answer.
  }
  return 'single';
}

function setReaderMode(mode) {
  if (mode !== 'single' && mode !== 'thread') return;
  state.readerMode = mode;
  try {
    localStorage.setItem(READER_MODE_KEY, mode);
  } catch {
    // Not fatal: the choice just will not survive a restart.
  }
  syncViewModeButton();
}

/** The button says which mode is *active*, not what clicking will do — the title says the rest. */
function syncViewModeButton() {
  const button = $('btn-view-mode');
  const thread = state.readerMode === 'thread';
  button.textContent = thread ? '💬 对话模式' : '✉ 普通模式';
  button.title = thread
    ? '当前是对话模式：整个会话按气泡显示，自己的邮件在右边，默认全部折叠。点一下改成普通模式。'
    : '当前是普通模式：一次只看一封。点一下改成对话模式（整个会话按气泡显示）。';
}

/** Switch the view, and re-read the conversation when switching *into* it. */
async function toggleReaderMode() {
  setReaderMode(state.readerMode === 'thread' ? 'single' : 'thread');
  const message = state.selectedMessage;
  if (message && state.readerMode === 'thread') {
    await loadConversation(message);
  }
  renderReader();
}

function wireGlobalEvents() {
  // The mail list is windowed: rows exist only for the visible window, so scrolling has to rebuild
  // that window. One listener here (not one per render), and one commit per animation frame.
  wireListScrolling();
  // The search-history panel is a dropdown, so a click elsewhere means "not this".
  wireSearchHistoryDismiss();
  $('btn-theme').addEventListener('click', () => {
    window.__easyEmailTheme.toggle();
    syncThemeButton();
  });
  $('btn-add-account').addEventListener('click', () => openAccountWizard());
  $('btn-view-mode').addEventListener('click', toggleReaderMode);
  $('btn-refresh').addEventListener('click', () => {
    // Refresh means "re-read what this pane is showing": for a local list that is `user.db`, not the
    // server, so it must not silently jump back to the folder.
    if (state.view === 'drafts') showDrafts();
    else if (state.view === 'outbox') showOutbox();
    else loadMessages({ reset: true });
  });
  $('btn-drafts').addEventListener('click', showDrafts);
  $('btn-outbox').addEventListener('click', showOutbox);
  $('btn-sync-window').addEventListener('click', openSyncWindowPicker);
  $('btn-about').addEventListener('click', openAbout);
  $('btn-log').addEventListener('click', openLog);
  $('btn-storage').addEventListener('click', openStoragePanel);
  $('btn-load-more').addEventListener('click', loadOlder);
  $('btn-compose').addEventListener('click', () => openComposer({}));
  $('btn-reply').addEventListener('click', () => openComposer({ mode: 'reply' }));
  $('btn-forward').addEventListener('click', () => openComposer({ mode: 'forward' }));
  // The conversation view has its own toolbar and acts on the **focused** message — which is exactly
  // what `state.detail` and `state.selectedFolder` hold in both views. Wiring them to the same
  // handlers is what keeps the two toolbars from drifting apart.
  $('btn-thread-reply').addEventListener('click', () => openComposer({ mode: 'reply' }));
  $('btn-thread-forward').addEventListener('click', () => openComposer({ mode: 'forward' }));
  $('btn-thread-toggle-read').addEventListener('click', toggleRead);
  $('btn-thread-delete').addEventListener('click', deleteSelected);
  $('btn-thread-send').addEventListener('click', () => {
    void sendThreadReply();
  });
  $('thread-reply-text').addEventListener('keydown', (event) => {
    // Ctrl+Enter sends; a bare Enter is a new line, because a reply is usually a paragraph rather than
    // a one-liner — and this box is the one place where a stray Enter would send mail.
    if (event.key === 'Enter' && (event.ctrlKey || event.metaKey)) {
      event.preventDefault();
      void sendThreadReply();
    }
  });
  $('search-input').addEventListener('input', () => {
    updateSearchSaveButton();
    scheduleSearch();
  });
  $('search-input').addEventListener('keydown', (event) => {
    if (event.key !== 'Escape') return;
    closeSearchHistory();
    $('search-input').value = '';
    exitSearchMode();
  });
  $('btn-search-clear').addEventListener('click', () => {
    $('search-input').value = '';
    $('search-input').focus();
    exitSearchMode();
  });
  $('btn-search-save').addEventListener('click', openSaveSearchModal);
  $('btn-search-history').addEventListener('click', () => {
    // The panel is closed by anything that means "not this"; a second press on the button is one of
    // those things, so it toggles rather than re-rendering what is already open.
    const open = !$('search-history').hidden;
    closeSearchHistory();
    if (!open) toggleSearchHistory();
  });
  $('btn-delete').addEventListener('click', deleteSelected);
  $('btn-toggle-read').addEventListener('click', toggleRead);
  $('tab-html').addEventListener('click', () => setBodyMode('html'));
  $('tab-text').addEventListener('click', () => setBodyMode('text'));
  $('btn-show-remote').addEventListener('click', () => {
    state.allowRemoteImages = true;
    applyBodyMode();
  });
  // Deliberately **not** closing on a click outside the dialog. It used to, and it destroyed work: a
  // user typing an address, a password or an authorization code who clicked a few pixels wide lost
  // everything — and in the wizard `closeModal` also deletes the half-created account (M1a-66, a real
  // user's report: "我点一下旁边填写的界面就消失了"). Esc and the two explicit buttons remain.
  document.addEventListener('keydown', (event) => {
    if (event.key === 'Escape') closeModal();
  });
}

/* ---------------------------------------------------------------- accounts */

async function refreshAccounts() {
  state.accounts = (await run('读取账号列表', () => invoke('list_accounts'))) ?? [];

  const list = $('account-list');
  list.innerHTML = '';

  if (state.accounts.length === 0) {
    list.innerHTML = '<div class="empty">还没有账号，点击下方按钮添加。</div>';
    state.accountId = null;
    state.mailboxes = [];
    state.messages = [];
    renderMailboxes();
    renderMessages();
    return;
  }

  for (const account of state.accounts) {
    const button = document.createElement('button');
    button.className = 'row' + (account.id === state.accountId ? ' active' : '');
    // OAuth accounts have no password on this machine, so the one thing the row owes the user is a way
    // back in when the refresh token dies — the error they will see is "请重新登录一次" (A1-F34).
    const reauth =
      account.auth === 'oauth2'
        ? '<span class="reauth" title="用浏览器重新登录（授权可能已过期）">↻</span>'
        : '';
    button.innerHTML = `
      <span class="label" title="${escapeHtml(account.email)}">${escapeHtml(account.email)}</span>
      ${reauth}
      <span class="notify" title="新邮件通知">🔔</span>
      <span class="count" title="移除账号">✕</span>`;
    button.querySelector('.label').addEventListener('click', () => selectAccount(account.id));
    button.querySelector('.count').addEventListener('click', (event) => {
      event.stopPropagation();
      removeAccount(account);
    });

    const reauthButton = button.querySelector('.reauth');
    if (reauthButton) {
      reauthButton.addEventListener('click', async (event) => {
        event.stopPropagation();
        toast('已打开系统浏览器，请在其中完成登录（两分钟内）', 'info', 9000);
        const tokens = await run('重新登录', () =>
          invoke('oauth_relogin', { accountId: account.id })
        );
        if (!tokens) return;
        toast(`${account.email} 已重新授权`, 'ok');
      });
    }

    // Per account, because a noisy work mailbox and a quiet personal one are a normal pair. The bell
    // is read from the backend (the preference lives in `user.db`), not guessed from a local flag.
    const bell = button.querySelector('.notify');
    bell.addEventListener('click', async (event) => {
      event.stopPropagation();
      const next = bell.textContent !== '🔔';
      const saved = await run('保存通知设置', async () => {
        await invoke('set_notify_enabled', { accountId: account.id, enabled: next });
        return true;
      });
      if (!saved) return;
      bell.textContent = next ? '🔔' : '🔕';
      bell.title = next ? '新邮件通知：开' : '新邮件通知：关';
      toast(`${account.email} 的新邮件通知已${next ? '打开' : '关闭'}`, 'ok');
    });
    invoke('get_notify_enabled', { accountId: account.id })
      .then((enabled) => {
        bell.textContent = enabled ? '🔔' : '🔕';
        bell.title = enabled ? '新邮件通知：开' : '新邮件通知：关';
      })
      .catch(() => {});
    list.appendChild(button);
  }

  const stillThere = state.accounts.some((a) => a.id === state.accountId);
  if (!stillThere) {
    state.accountId = state.accounts[0].id;
  }
  await selectAccount(state.accountId);
}

async function selectAccount(accountId) {
  if (!accountId) return;
  // The cap belongs to the account's provider; a second account may have a different one.
  state.sendCap = null;
  const changed = state.accountId !== accountId;
  state.accountId = accountId;

  for (const row of document.querySelectorAll('#account-list .row')) {
    row.classList.toggle('active', row.querySelector('.label').title === accountId);
  }

  if (changed || state.mailboxes.length === 0) {
    // Both local lists belong to one account, so switching accounts has to re-read them — and it
    // must not fall back to the folder on the way, or the sidebar would disagree with the pane.
    const wasShowing = state.view;
    await loadMailboxes();
    if (wasShowing === 'drafts') await showDrafts();
    else if (wasShowing === 'outbox') await showOutbox();
  } else if (state.view === 'drafts') {
    await showDrafts();
  } else if (state.view === 'outbox') {
    await showOutbox();
  } else {
    // Still refresh the badges: they belong to this account whatever the pane shows.
    await refreshDrafts();
    await refreshOutbox();
  }
  // The saved searches are per account too, and the global ones ride along — which is what the
  // "所有账号" label in the sidebar says out loud.
  await refreshSavedSearches();
  await refreshSearchHistory();
}

async function removeAccount(account) {
  if (!confirm(`移除账号 ${account.email}？\n保存在 Windows 凭据管理器中的密码也会被删除。`)) {
    return;
  }
  const ok = await run('移除账号', async () => {
    await invoke('delete_account', { accountId: account.id });
    return true;
  });
  if (ok) {
    toast(`已移除 ${account.email}`, 'ok');
    state.accountId = null;
    state.mailboxes = [];
    state.messages = [];
    state.detail = null;
    renderReader();
    await refreshAccounts();
  }
}

/* ---------------------------------------------------------------- mailboxes */

async function loadMailboxes() {
  const accountId = state.accountId;
  if (!accountId) return;

  const mailboxes = await run('读取文件夹', () =>
    invoke('list_mailboxes', { accountId })
  );
  state.mailboxes = (mailboxes ?? []).filter((box) => box.selectable);
  renderMailboxes();

  // Badges come from the cache, so an offline start still shows what was unread when the app was
  // last online. Best effort: a failure here must not stop the folder list from rendering.
  await loadUnreadCounts();

  if (state.mailboxes.length === 0) {
    state.messages = [];
    renderMessages();
    return;
  }

  const inbox = state.mailboxes.find((box) => box.name.toUpperCase() === 'INBOX');
  selectMailbox(inbox ?? state.mailboxes[0]);
  renderMailboxes();
  await loadMessages({ reset: true });
  // After the first page is on screen, and never before: the top-up is worth a round trip only when
  // there is a list to add to.
  backfillColdStart();
}

/**
 * Point the app at a mailbox.
 *
 * `name` is the wire form (IMAP modified UTF-7) and is what every command must send back;
 * `display_name` is the decoded form and is the only one a human should ever see. Keeping both
 * in state is what stops a Chinese folder from being sent back as mojibake — or shown as
 * `&UXZO1mWHTvZZOQ-`.
 */
function selectMailbox(box) {
  // Picking a folder is leaving search; otherwise the list would keep showing hits while the
  // sidebar said something else.
  exitSearchMode({ reload: false });
  $('search-input').value = '';
  state.mailbox = box.name;
  state.mailboxLabel = box.display_name || box.name;
}

/* ---------------------------------------------------------------- local search */

let searchTimer = null;

/** The display name of a wire folder, for tagging a search hit with where it came from. */
function folderLabel(wireName) {
  const known = state.mailboxes.find((box) => box.name === wireName);
  return known ? known.display_name || known.name : wireName;
}

function scheduleSearch() {
  clearTimeout(searchTimer);
  searchTimer = setTimeout(runSearch, SEARCH_DEBOUNCE_MS);
}

/**
 * Search the local cache.
 *
 * Entirely local — `search_messages` never touches the network — so it works offline and cannot
 * fail because the server is unreachable. What it can miss is mail that has never been synced, and
 * body text of messages that have never been opened; the status line says so rather than letting an
 * empty result read as "not in your mailbox".
 */
async function runSearch() {
  const query = $('search-input').value.trim();

  if (!query) {
    exitSearchMode();
    return;
  }

  const hits = await run('本地搜索', () =>
    invoke('search_messages', { accountId: state.accountId, query, limit: SEARCH_LIMIT })
  );
  if (hits === null) return;

  // A slower earlier query must not overwrite a newer one.
  if ($('search-input').value.trim() !== query) return;

  state.searchQuery = query;
  state.searchHits = hits;
  // The save button appears exactly now: there is something in the box worth keeping, and it
  // disappears the moment the box is cleared or a newer query is on its way.
  updateSearchSaveButton();
  // And the query is history (`design/09` §4.1). Here rather than in the keystroke handler: the
  // guard above has already established that this query is the newest one, so no half-typed or
  // superseded query reaches the list.
  invoke('history_search_push', { accountId: state.accountId, query }).catch(() => {});
  // Search covers cached mail, so it takes the pane out of the drafts list — leaving it in drafts
  // would swallow the results the user just asked for.
  state.view = 'mail';
  syncMailboxRows();
  $('btn-search-clear').hidden = false;
  // "Why does search not find that mail?" is asked right here, so the index's answer belongs here
  // too — once per search, not on a timer, because the user is the one who knows to ask.
  checkIndexHealth();

  const status = $('search-status');
  status.hidden = false;
  status.replaceChildren();

  const coverage = await run('读取索引覆盖率', () =>
    invoke('index_coverage', { accountId: state.accountId })
  );
  // The count is what turns "no results" from a dead end into a solvable one: without it, a user
  // whose bodies were never fetched cannot tell "not in your mailbox" from "not indexed yet".
  state.bodyCoverage = coverage;
  const cover = coverage
    ? `正文已索引 ${coverage.indexed}/${coverage.total} 封`
    : '正文索引覆盖率未知';

  // A hint about the query itself, when the backend can see the user meant something it did not get
  // (`design/09` §2: a syntax mistake must produce a hint, not an empty result). Asked only when
  // there is a colon in the query, so a plain word costs no extra call.
  if (state.searchQuery.includes(':')) {
    const hints = await run('读取查询提示', () => invoke('search_hints', { query: state.searchQuery }));
    if (hints?.length) {
      const line = document.createElement('div');
      line.className = 'search-hint';
      line.textContent = hints.join(' ');
      status.append(line);
    }
  }

  if (hits.length) {
    status.append(document.createTextNode(`本地缓存命中 ${hits.length} 条 · ${cover}`));
  } else {
    status.append(
      document.createTextNode(`本地缓存里没有匹配。搜索只覆盖已同步的邮件，${cover}。`)
    );
  }

  // Only offered while there is something to index: a button that does nothing is worse than none.
  if (coverage && coverage.indexed < coverage.total) {
    const button = document.createElement('button');
    button.type = 'button';
    button.className = 'btn ghost small';
    button.textContent = '现在索引';
    button.title = '把已同步但还没读过的邮件正文加进搜索索引（每批 20 封）';
    button.addEventListener('click', () => indexBodiesNow());
    status.append(document.createTextNode(' '), button);
  }

  status.append(document.createTextNode(' '), rebuildIndexButton());

  renderMessages();
}

/** How many index rows one rebuild step rewrites. */
const REBUILD_BATCH = 500;

/**
 * One rebuild at a time, as a module-level fact rather than a property of the button.
 *
 * The button lives in the search status line, which any new search re-renders — so "is a rebuild
 * running?" cannot be answered by looking at it. And the answer has to exist: a second `begin`
 * while the first loop is mid-way would clear the index the first loop is walking, and the two would
 * interleave steps over the same rows. A lost button is recoverable; that is not.
 */
let rebuildInFlight = false;

/**
 * 「重建索引」 — throw the local search index away and build it again.
 *
 * Two clicks on purpose. The operation deletes the whole index before rebuilding it, so there is a
 * window in which search answers *less* than before; a single click on a button whose label is two
 * characters long is not enough consent for that. The confirm state lives in this button, so
 * re-rendering the status line (which any search does) quietly cancels it.
 *
 * It also never touches the network: the rebuild re-derives what is already cached, so mail whose
 * body was never fetched comes back searchable by subject only — and the pending body pass
 * (`indexBodiesNow`) is what fills those in afterwards.
 */
function rebuildIndexButton() {
  const button = document.createElement('button');
  button.type = 'button';
  button.className = 'btn ghost small';
  button.textContent = '重建索引';
  button.title = '丢掉并重建本地搜索索引（只重建本机已有的内容，不联网；随时可停）';
  button.addEventListener('click', () => {
    if (rebuildInFlight) {
      toast('索引正在重建中，等它跑完（或点「停止」）再来。', 'info');
      return;
    }
    if (button.dataset.armed === '1') {
      button.removeAttribute('data-armed');
      button.disabled = true;
      rebuildSearchIndex(button);
      return;
    }
    button.dataset.armed = '1';
    button.textContent = '确认重建？';
    button.title = '点一次确认：索引会先被清空，再分批重建。中途可以停，已重建的部分照常可搜。';
  });
  return button;
}

/** Drive the rebuild in bounded steps, showing progress, until it finishes or the user stops it. */
async function rebuildSearchIndex(button) {
  const status = $('search-status');
  rebuildInFlight = true;
  const started = await run('清空索引', () => invoke('rebuild_search_begin'));
  if (started === null) {
    rebuildInFlight = false;
    button.disabled = false;
    button.textContent = '重建索引';
    return;
  }

  const note = document.createElement('span');
  const stop = document.createElement('button');
  stop.type = 'button';
  stop.className = 'btn ghost small';
  stop.textContent = '停止';
  stop.title = '不再进行下一批。已重建的部分照常可搜，随时可以再点「重建索引」接着来。';
  let stopped = false;
  stop.addEventListener('click', () => {
    stopped = true;
  });
  status.append(note, stop);

  let done = 0;
  let total = 0;
  let remaining = 1;
  let failed = false;
  while (remaining > 0 && !stopped) {
    // Yield between batches so the window keeps painting and the stop button stays clickable;
    // a rebuild that freezes the UI cannot be cancelled by definition.
    await new Promise((resolve) => setTimeout(resolve, 0));
    const progress = await run('重建索引', () =>
      invoke('rebuild_search_step', { limit: REBUILD_BATCH })
    );
    if (progress === null) {
      // `run` has already toasted the real message. The rebuild is left as it is: partially built,
      // resumable, and not pretended to be finished.
      failed = true;
      break;
    }
    done = progress.done;
    total = progress.total;
    remaining = progress.remaining;
    note.textContent = ` 正在重建索引 ${done}/${total}`;
  }

  note.remove();
  stop.remove();
  rebuildInFlight = false;
  button.disabled = false;
  button.textContent = '重建索引';
  if (!failed) {
    toast(
      remaining > 0
        ? `索引重建已停止（${done}/${total}），已重建的部分可以搜索；再点一次接着来。`
        : `索引已重建完成（${done}/${total}）。`,
      remaining > 0 ? 'info' : 'ok'
    );
  }
  // The hits on screen came from the old index; re-run so what is shown matches what is stored.
  await runSearch();
}

/** Read bodies the user has not opened, so search can see them. One bounded pass per press. */
async function indexBodiesNow() {
  const report = await run('索引正文', () =>
    invoke('index_bodies', { accountId: state.accountId })
  );
  if (report === null) return;
  // Re-run the search so the new coverage lands in the status line, and any hit that only exists
  // in a body that was just fetched shows up.
  await runSearch();
}

/* ---------------------------------------------------------------- index health */

/**
 * Ask whether the search index is still whole, and say so where the user looked.
 *
 * Where the user looked is the status line under the search box: the question "why does search not
 * find that mail" is asked there, so that is where the answer has to be. A check that reports
 * somewhere else is a check the user meets only by accident.
 *
 * The repair is **not** automatic. The rebuild clears the index first, so a search during it
 * answers with less than before — the same reason the round-72 rebuild asks for confirmation. An
 * index that quietly rebuilds itself while somebody is searching is a worse problem than the one it
 * fixed.
 */
async function checkIndexHealth() {
  if (!state.accountId) return;
  const health = await run('检查索引', () =>
    invoke('index_health', { accountId: state.accountId })
  );
  if (health === null) return;
  renderIndexHealth(health);
}

function renderIndexHealth(health) {
  const host = $('index-health');
  const broken = !health.fts_ok || health.indexed < health.messages || health.orphan_bodies > 0;
  if (!broken) {
    host.hidden = true;
    host.replaceChildren();
    return;
  }
  host.hidden = false;
  host.replaceChildren();

  const what = [];
  if (!health.fts_ok) what.push('索引自检未通过');
  if (health.indexed < health.messages) {
    what.push(`${health.messages - health.indexed} 封邮件不在索引里`);
  }
  if (health.orphan_bodies > 0) what.push(`${health.orphan_bodies} 条无用的正文缓存`);

  const line = document.createElement('span');
  line.textContent = `索引可能不完整（${what.join('，')}）：`;
  host.appendChild(line);

  const fix = document.createElement('button');
  fix.className = 'btn ghost small';
  fix.type = 'button';
  fix.textContent = '修复索引';
  fix.title = '重建索引。重建期间搜索范围会变小，所以需要你确认（与「重建索引」同一个理由）。';
  fix.addEventListener('click', () => {
    // Same confirmation as the round-72 rebuild, for its reason: the rebuild clears the index first.
    if (!confirm('要重建搜索索引吗？\n重建期间搜索能搜到的邮件会变少。')) return;
    healIndex(fix);
  });
  host.appendChild(fix);
}

async function healIndex(button) {
  button.disabled = true;
  button.textContent = '正在修复…';
  const report = await run('修复索引', () =>
    invoke('index_heal', { accountId: state.accountId, step: 500 })
  );
  button.disabled = false;
  if (report === null) {
    button.textContent = '修复索引';
    return;
  }
  toast(`索引已修复：重建 ${report.rebuilt} 封，清理 ${report.orphans_deleted} 条`, 'ok');
  // Ask again rather than assume: the line disappears only when the index really says it is whole.
  checkIndexHealth();
  runSearch();
}

function exitSearchMode({ reload = true } = {}) {
  const wasSearching = Boolean(state.searchQuery) || state.searchHits.length > 0;
  state.searchQuery = '';
  state.searchHits = [];
  clearTimeout(searchTimer);
  $('search-status').hidden = true;
  $('btn-search-clear').hidden = true;
  $('btn-search-save').hidden = true;
  if (wasSearching && reload) loadMessages({ reset: true });
}

/* ---------------------------------------------------------------- saved searches */

/**
 * The rows in the sidebar, and the buttons that act on them.
 *
 * One rule holds this whole block together: **showing a saved search is the same thing as running
 * it.** The row fills the search box and calls the one search path (`runSearch`), so a saved search
 * cannot drift from what the user would have typed — which is what would happen if there were two
 * ways to run a query in this app and only one of them kept up with the coverage line, the hint, and
 * the "a newer query already answered" guard.
 */
async function refreshSavedSearches() {
  if (!state.accountId) {
    state.savedSearches = [];
    renderSavedSearches();
    return;
  }
  const list = await run('读取保存的搜索', () =>
    invoke('saved_search_list', { accountId: state.accountId })
  );
  if (list === null) return;
  state.savedSearches = list;
  renderSavedSearches();
}

/** Draw the rows. Global ones are labelled, for the reason `splitByScope` explains. */
function renderSavedSearches() {
  const host = $('saved-searches');
  const { account, global } = window.__easyEmailSavedSearch.splitByScope(state.savedSearches);
  host.replaceChildren();

  const rows = [
    ...account.map((item) => ({ item, scope: '' })),
    ...global.map((item) => ({ item, scope: '所有账号' })),
  ];

  if (rows.length === 0) {
    const empty = document.createElement('p');
    empty.className = 'saved-empty';
    empty.textContent = '还没有保存的搜索。在搜索框里输入条件后点「保存」。';
    host.appendChild(empty);
    return;
  }

  for (const { item, scope } of rows) {
    const row = document.createElement('div');
    row.className = 'saved-row';

    const run = document.createElement('button');
    run.className = 'row';
    run.type = 'button';
    run.title = item.query;
    // The active one is the row whose query is in the box right now — the only state a user can see
    // for free, and the difference between "my click did nothing" and "it is already showing".
    if (item.query === $('search-input').value.trim()) run.classList.add('active');
    run.append(nameSpan(item), scopeSpan(scope));
    run.addEventListener('click', () => runSavedSearch(item));

    const rename = smallButton('✎', '改名', () => renameSavedSearch(item));
    const remove = smallButton('✕', '删除这个保存的搜索', () => deleteSavedSearch(item));
    row.append(run, rename, remove);
    host.appendChild(row);
  }
}

function nameSpan(item) {
  const span = document.createElement('span');
  span.className = 'label';
  span.textContent = item.name;
  if (item.query !== item.name) {
    // The query, faintly: a saved search whose name is not the query is the one people re-type wrong.
    const hint = document.createElement('span');
    hint.className = 'saved-hint';
    hint.textContent = item.query;
    span.append(hint);
  }
  return span;
}

function scopeSpan(scope) {
  const span = document.createElement('span');
  span.className = 'count';
  span.textContent = scope;
  return span;
}

function smallButton(text, title, onClick) {
  const button = document.createElement('button');
  button.className = 'saved-act';
  button.type = 'button';
  button.textContent = text;
  button.title = title;
  button.addEventListener('click', (event) => {
    // A click on ✕ must not also run the search behind it.
    event.stopPropagation();
    onClick();
  });
  return button;
}

/** Offer to keep the query in the box. The name is suggested, not demanded. */
function openSaveSearchModal() {
  const query = $('search-input').value.trim();
  if (!query) return;
  const suggested = window.__easyEmailSavedSearch.suggestName(query);
  openModal({
    title: '保存搜索',
    bodyHtml: `
      <label class="field">
        <span class="field-label">名字</span>
        <input id="save-name" type="text" maxlength="60" value="${escapeHtml(suggested)}" />
      </label>
      <p class="about-note">
        保存的是「${escapeHtml(query)}」这个条件。同名再保存会替换原来的，
        侧栏里点一下就能重新搜。
      </p>`,
    confirmLabel: '保存',
    onReady: (modal) => {
      const input = modal.querySelector('#save-name');
      input.focus();
      input.select();
    },
    onConfirm: async () => {
      const name = modalName();
      if (!name) return;
      const saved = await run('保存搜索', () =>
        invoke('saved_search_save', { accountId: state.accountId, name, query })
      );
      if (saved === null) return;
      await refreshSavedSearches();
    },
  });
}

/** The name in the save dialog, read through the modal's own DOM. */
function modalName() {
  const input = $('modal-root').querySelector('#save-name');
  return input ? input.value.trim() : '';
}

async function renameSavedSearch(item) {
  openModal({
    title: '改名',
    bodyHtml: `
      <label class="field">
        <span class="field-label">新名字</span>
        <input id="rename-name" type="text" maxlength="60" value="${escapeHtml(item.name)}" />
      </label>`,
    confirmLabel: '保存',
    onReady: (modal) => {
      const input = modal.querySelector('#rename-name');
      input.focus();
      input.select();
    },
    onConfirm: async () => {
      const name = $('modal-root').querySelector('#rename-name').value.trim();
      if (!name || name === item.name) return;
      const renamed = await run('改名', () =>
        invoke('saved_search_rename', { accountId: state.accountId, id: String(item.id), name })
      );
      if (renamed === null) return;
      await refreshSavedSearches();
    },
  });
}

async function deleteSavedSearch(item) {
  // The browser's confirm, the same one removing an account uses: a dialog of our own for a
  // one-line question is more machinery than the question deserves.
  if (!confirm(`删除保存的搜索「${item.name}」？\n这个操作不可撤销。`)) return;
  const removed = await run('删除保存的搜索', () =>
    invoke('saved_search_delete', { accountId: state.accountId, id: String(item.id) })
  );
  if (removed === null) return;
  await refreshSavedSearches();
}

/** Run a saved search: fill the box, then let the one search path do the rest. */
async function runSavedSearch(item) {
  $('search-input').value = item.query;
  await runSearch();
  renderSavedSearches();
}

/** Show the save button exactly when there is something in the box to keep. */
function updateSearchSaveButton() {
  $('btn-search-save').hidden = !$('search-input').value.trim();
}

/* ---------------------------------------------------------------- search history */

/** How many recent queries the dropdown shows, and how many the store keeps. */
const HISTORY_SHOWN = 12;

/**
 * Recent queries, and the panel that shows them (`design/09` §4.1).
 *
 * The panel appears when asked and disappears on anything that means "not this" — another search, a
 * click outside, Escape. That is the whole UX decision: a popup on focus would make "I only want to
 * change one word" begin with closing something.
 */
async function refreshSearchHistory() {
  if (!state.accountId) return;
  const list = await run('读取搜索历史', () =>
    invoke('history_search_list', { accountId: state.accountId, limit: HISTORY_SHOWN })
  );
  if (list === null) return;
  renderSearchHistory(list);
}

function renderSearchHistory(list) {
  const panel = $('search-history');
  if (!list || list.length === 0) {
    panel.hidden = true;
    panel.replaceChildren();
    // An empty panel that still says "历史" is how a feature looks broken rather than unused.
    $('btn-search-history').hidden = true;
    return;
  }
  $('btn-search-history').hidden = false;
  if (panel.hidden) return; // Refreshed in the background; do not pop open what is not open.

  panel.replaceChildren();
  for (const query of list) {
    const row = document.createElement('button');
    row.className = 'row';
    row.type = 'button';
    const label = document.createElement('span');
    label.className = 'label saved-hint';
    label.textContent = query;
    row.append(label);
    row.addEventListener('click', () => {
      closeSearchHistory();
      $('search-input').value = query;
      updateSearchSaveButton();
      runSearch();
    });
    panel.appendChild(row);
  }

  const clear = document.createElement('button');
  clear.className = 'row saved-act-full';
  clear.type = 'button';
  clear.textContent = '清空历史';
  clear.title = '忘掉这个账号最近搜过的查询';
  clear.addEventListener('click', async () => {
    if (!confirm('清空这个账号的搜索历史？\n已保存的搜索不受影响。')) return;
    const ok = await run('清空搜索历史', () =>
      invoke('history_search_clear', { accountId: state.accountId })
    );
    if (ok === null) return;
    closeSearchHistory();
    await refreshSearchHistory();
  });
  panel.appendChild(clear);
}

function toggleSearchHistory() {
  const panel = $('search-history');
  if (panel.hidden) {
    // Drawn from the store each time it opens, so a query searched from another row is already
    // here — the one thing a panel that renders once gets wrong.
    refreshSearchHistory().then(() => {
      panel.hidden = false;
    });
    return;
  }
  panel.hidden = true;
}

function closeSearchHistory() {
  $('search-history').hidden = true;
}

/**
 * Closed by a click anywhere that is not the panel or its button.
 *
 * Unlike the modal — which deliberately ignores outside clicks, because closing it destroyed typed
 * work (M1a-66) — this panel holds nothing, so the usual dropdown rule is the right one: a click
 * elsewhere means "not this".
 */
function wireSearchHistoryDismiss() {
  document.addEventListener('click', (event) => {
    const panel = $('search-history');
    if (panel.hidden) return;
    if (event.target.closest('#search-history') || event.target.closest('#btn-search-history')) return;
    closeSearchHistory();
  });
}

/** Open a search hit, switching folder first when the hit lives somewhere else. */
async function openHit(hit) {
  if (hit.folder !== state.mailbox) {
    state.mailbox = hit.folder;
    state.mailboxLabel = folderLabel(hit.folder);
    $('mailbox-title').textContent = state.mailboxLabel;
    renderMailboxes();
  }
  // `fetch_message` goes through the cache first, so opening a hit is normally instant.
  await openMessage({
    uid: hit.uid,
    subject: hit.subject,
    from_name: hit.from_name,
    from_addr: hit.from_addr,
    date: hit.date,
    seen: hit.seen,
    size: null,
    flagged: false,
  });
}

/** Refresh the sidebar badges. Failures leave the previous counts on screen rather than blanking them. */
async function loadUnreadCounts() {
  if (!state.accountId) return;
  const counts = await run('读取未读数', () =>
    invoke('folder_unread_counts', { accountId: state.accountId })
  );
  state.unreadCounts = counts ?? {};
  renderMailboxes();
}

function renderMailboxes() {
  const list = $('mailbox-list');
  list.innerHTML = '';
  if (state.mailboxes.length === 0) {
    list.innerHTML = '<div class="empty">选择账号后显示文件夹。</div>';
    syncMailboxRows();
    return;
  }
  for (const box of state.mailboxes) {
    const label = box.display_name || box.name;
    const button = document.createElement('button');
    button.className = 'row';
    // The wire name, not the display name: two folders can share a decoded label, and the
    // highlight must follow the one the commands are actually pointed at.
    button.dataset.mailbox = box.name;
    const unread = state.unreadCounts[box.name] ?? 0;
    button.innerHTML = `<span class="label" title="${escapeHtml(label)}">${escapeHtml(label)}</span>${
      unread > 0
        ? `<span class="unread-badge" title="${escapeHtml(label)}：${unread} 封未读">${escapeHtml(
            formatUnread(unread)
          )}</span>`
        : ''
    }`;
    button.addEventListener('click', () => {
      // Choosing a folder is what leaves the drafts list; `selectMailbox` itself is also called
      // while loading an account, where jumping out of drafts would be exactly wrong.
      state.view = 'mail';
      selectMailbox(box);
      renderMailboxes();
      loadMessages({ reset: true });
    });
    list.appendChild(button);
  }
  syncMailboxRows();
}

/**
 * Keep the two local sidebar rows honest about which one the pane is showing.
 *
 * Neither local list is a mailbox, so `renderMailboxes` cannot own their highlight; without this the
 * folder you left would stay lit while the pane showed drafts or the outbox.
 */
function syncMailboxRows() {
  $('btn-drafts').classList.toggle('active', state.view === 'drafts');
  $('btn-outbox').classList.toggle('active', state.view === 'outbox');
  // Empty rather than "0": a badge reading 0 is noise on a list that is already visibly empty.
  $('drafts-count').textContent = state.drafts.length > 0 ? String(state.drafts.length) : '';
  // Only the still-open sends: the table keeps sent and cancelled rows as history, and counting
  // those would leave a badge on screen forever.
  const open = state.outbox.filter((item) => item.state === 'queued' || item.state === 'sending');
  $('outbox-count').textContent = open.length > 0 ? String(open.length) : '';

  for (const row of document.querySelectorAll('#mailbox-list .row')) {
    row.classList.toggle('active', state.view === 'mail' && row.dataset.mailbox === state.mailbox);
  }
}

/* ---------------------------------------------------------------- sync window */

/** How the stored window reads to a person. */
function windowLabel(window) {
  if (!window) return '';
  switch (window.kind) {
    case 'all':
      return '全部';
    case 'years':
      return `最近 ${window.value} 年`;
    case 'count':
      return `最近 ${window.value} 封`;
    case 'on_demand':
      return '按需';
    default:
      // A window this version does not know about is still a stored preference; showing the raw
      // kind beats showing nothing.
      return String(window.kind);
  }
}

async function refreshSyncWindow() {
  const window = await invoke('get_sync_window').catch(() => null);
  if (!window) return;
  state.syncWindow = window;

  // The row is where the *state* of syncing belongs, not just the setting: when the daily quota is
  // spent or the server has asked us to slow down, "还在往回取" and "今天不会再取了" must not look
  // the same from the sidebar (`design/04` §5.7's honesty, one line earlier than the dialog).
  const quota = await invoke('quota_status', { accountId: state.accountId }).catch(() => null);
  state.quota = quota;
  $('sync-window-value').textContent = quotaPaused(quota)
    ? `${windowLabel(window)} · ${quotaPaused(quota)}`
    : windowLabel(window);
}

/**
 * Why the sync is not running right now, or an empty string when it may.
 *
 * Kept separate from the dialog's sentence: this one has to fit a sidebar row, and it is a state
 * ("今天不再取") rather than an explanation.
 */
function quotaPaused(quota) {
  if (!quota) return '';
  if (quota.throttled_until && quota.throttled_until * 1000 > Date.now()) return '服务器限流中';
  if (quota.daily_budget && quota.bytes_today >= quota.daily_budget) return '今日额度已用完';
  return '';
}

/**
 * Let the user choose how far back the client keeps mail.
 *
 * The options are the four the backend can store (`design/04`); "全部" is honest about being
 * unbounded, because that walk only stops when history ends (the quota ledger that paces it is not
 * built yet). Narrowing never deletes what is already cached — it only stops walking further back,
 * which is what the hint says, because a setting that silently removed mail would be a trap.
 */
async function openSyncWindowPicker() {
  const current = state.syncWindow ?? { kind: 'years', value: 2 };
  // The ledger decides when the sync stops; showing the same number here is what keeps "今天不再
  // 往回取" from looking like "网络坏了" (`design/04` §5.7 asks for exactly this honesty).
  const quota = await invoke('quota_status', { accountId: state.accountId }).catch(() => null);
  const checked = (kind) => (current.kind === kind ? 'checked' : '');
  const years = current.kind === 'years' ? current.value : 2;
  const count = current.kind === 'count' ? current.value : 2000;

  const body = `
    <div class="quota-status">${escapeHtml(quotaLine(quota))}</div>
    <div class="field sync-options">
      <label class="radio-row">
        <input type="radio" name="sync-window" value="years" ${checked('years')} />
        <span>最近 <input id="sync-years" type="number" min="1" max="50" value="${years}" class="inline-number" /> 年</span>
      </label>
      <label class="radio-row">
        <input type="radio" name="sync-window" value="count" ${checked('count')} />
        <span>每个文件夹最近 <input id="sync-count" type="number" min="50" max="100000" value="${count}" class="inline-number" /> 封</span>
      </label>
      <label class="radio-row">
        <input type="radio" name="sync-window" value="all" ${checked('all')} />
        <span>全部邮件（一直往回取，直到服务器没有更早的）</span>
      </label>
      <label class="radio-row">
        <input type="radio" name="sync-window" value="on_demand" ${checked('on_demand')} />
        <span>只按需：不主动往回取，打开哪封取哪封</span>
      </label>
    </div>
    <span class="hint">收窄窗口不会删除已经缓存的邮件，只是停止继续往回取。</span>`;

  openModal({
    title: '同步窗口',
    bodyHtml: body,
    confirmLabel: '保存',
    onConfirm: async () => {
      const picked = document.querySelector('input[name="sync-window"]:checked')?.value;
      const window = windowFromForm(picked);
      if (!window) {
        toast('请填一个有效的年数或封数。', 'err');
        return false;
      }

      const saved = await run('保存同步窗口', async () => {
        await invoke('set_sync_window', { window });
        return true;
      });
      if (!saved) return false;

      state.syncWindow = window;
      $('sync-window-value').textContent = windowLabel(window);
      toast(`同步窗口：${windowLabel(window)}`, 'ok');
      return true;
    },
  });
}

/** Today's download allowance, in the words the user needs when the sync stops on its own. */
function quotaLine(quota) {
  if (!quota) return '';
  const used = formatBytes(quota.bytes_today) || '0 B';
  if (quota.throttled_until && quota.throttled_until * 1000 > Date.now()) {
    return `服务器暂时限制了同步频率，会在 ${new Date(
      quota.throttled_until * 1000
    ).toLocaleTimeString('zh-CN', { hour: '2-digit', minute: '2-digit' })} 后自动重试。`;
  }
  // The observed floor, when there is one: the oldest mail this account can actually reach. Saying it
  // is the difference between a server policy and "同步坏了" (`design/04` §5.7 item 4).
  const floor = quota.server_floor
    ? `服务器只提供到 ${new Date(quota.server_floor * 1000).toLocaleDateString('zh-CN')} 的邮件。`
    : '';
  const lead = quota.daily_budget
    ? `今日已下载 ${used}，额度 ${formatBytes(quota.daily_budget)}（剩余 ${formatBytes(
        Math.max(quota.daily_budget - quota.bytes_today, 0)
      )}）；用完后会自动停到明天。`
    : `今日已下载 ${used}（这个服务商没有公开的每日下载上限）。`;
  return floor ? `${lead} ${floor}` : lead;
}
/** Read the picker's form into the wire shape serde expects (`{kind, value}`). */
function windowFromForm(kind) {
  switch (kind) {
    case 'all':
      return { kind: 'all' };
    case 'on_demand':
      return { kind: 'on_demand' };
    case 'years': {
      const years = Number(document.getElementById('sync-years')?.value);
      if (!Number.isFinite(years) || years < 1 || years > 50) return null;
      return { kind: 'years', value: Math.round(years) };
    }
    case 'count': {
      const count = Number(document.getElementById('sync-count')?.value);
      if (!Number.isFinite(count) || count < 50 || count > 100000) return null;
      return { kind: 'count', value: Math.round(count) };
    }
    default:
      return null;
  }
}

/** Accounts whose cold-start backfill has already run in this window session. */
const backfilledAccounts = new Set();

/**
 * Walk one page towards the chosen window, once per account per session (`design/04` §5.7's
 * "冷启动自动回填一页").
 *
 * One page, not "until the window is satisfied": the window can be "all mail", and a client that
 * starts a multi-thousand-message download the moment it opens is the behaviour this bound exists
 * to prevent. A failure is ignored on purpose — the user asked to read mail, not to see an error
 * about a background top-up, and the sync status area already reports being offline.
 */
async function backfillColdStart() {
  const accountId = state.accountId;
  if (!accountId || !state.mailbox || backfilledAccounts.has(accountId)) return;
  backfilledAccounts.add(accountId);

  const report = await invoke('backfill_to_window', {
    accountId,
    mailbox: state.mailbox,
    pages: 1,
  }).catch(() => null);
  if (!report) return;

  // Mail cached before `thread_key` existed has no conversation key, and no future sync will fill it
  // (an incremental sync only asks for UIDs above the cursor). One header re-fetch fixes the rows the
  // list actually shows, once per account per session, and only when the list can see they are
  // missing — otherwise every startup would pay for a round trip nobody needs.
  if (state.messages.some((message) => !message.thread_key)) {
    const filled = await invoke('refresh_thread_keys', {
      accountId,
      mailbox: state.mailbox,
    }).catch(() => 0);
    if (filled > 0) {
      await loadMessages({ reset: true });
    }
  }

  // The daily quota is a fact about the account, not a failure of this walk — and saying *why* the
  // client suddenly stopped is the difference between a policy and a bug the user blames on us.
  if (report.stop === 'quota_spent') {
    toast(quotaMessage(), 'info', 12000);
    // The sidebar row says "今日额度已用完" from here on, without opening the dialog.
    refreshSyncWindow();
    return;
  }

  // "No older mail" is normal (the mailbox is small) — except on the services that cap how far back
  // a third-party client may read, where it looks exactly like our own bug. The backend knows which
  // ones those are and words the hint; an empty answer means say nothing.
  if (report.stop === 'history_exhausted' && state.syncWindow?.kind !== 'on_demand') {
    const hint = await invoke('range_cap_hint', { accountId }).catch(() => null);
    if (hint) toast(hint, 'info', 15000);
  }

  if (!report.fetched) return;

  toast(`按同步窗口补齐了 ${report.fetched} 封更早的邮件`, 'info');
  loadMessages({ reset: true });
}

/** What to say when the provider's daily download quota is what stopped the sync. */
function quotaMessage() {
  const account = state.accounts.find((a) => a.id === state.accountId);
  const provider = state.providers.find((p) => p.id === account?.provider_id);
  const whose = provider ? `（${provider.display_name} 对每天下载量有限制）` : '';
  return `今日同步额度已用完${whose}。明天自动继续，也可以在侧栏「同步窗口」里改成更小的范围。`;
}

/* ---------------------------------------------------------------- drafts */

/**
 * Show the drafts list for the current account.
 *
 * Read straight from `user.db` and never from the network: a draft exists precisely for the
 * moments when the server cannot be reached (design/07 §3). Failures are reported but leave the
 * previous list on screen, because an empty list would read as "your drafts are gone".
 */
async function showDrafts() {
  if (!state.accountId) {
    toast('请先添加一个邮箱账号。', 'err');
    return;
  }

  exitSearchMode({ reload: false });
  $('search-input').value = '';
  $('message-list').innerHTML = '<div class="empty"><span class="spinner"></span>正在读取草稿…</div>';

  const drafts = await run('读取草稿', () =>
    invoke('list_drafts', { accountId: state.accountId })
  );
  if (drafts === null) {
    // Put back whatever the pane was showing — a failed read must not leave it claiming to be the
    // (empty) drafts list.
    renderMessages();
    return;
  }

  state.view = 'drafts';
  state.drafts = drafts;
  state.selectedUid = null;
  state.detail = null;
  $('mailbox-title').textContent = '草稿箱';
  renderReader();
  syncMailboxRows();
  renderMessages();
}

/** Re-read just the count and the open list, for after a save or a discard. */
async function refreshDrafts() {
  if (!state.accountId) return;
  const drafts = await invoke('list_drafts', { accountId: state.accountId }).catch(() => null);
  if (drafts === null) return;

  state.drafts = drafts;
  syncMailboxRows();
  if (state.view === 'drafts') renderMessages();
}

function renderDrafts(list) {
  $('list-footer').hidden = true;

  if (state.drafts.length === 0) {
    list.innerHTML = '<div class="empty">没有草稿。写信时会自动保存到这里。</div>';
    return;
  }

  for (const draft of state.drafts) {
    const row = document.createElement('div');
    row.className = 'msg';
    row.dataset.draftId = draft.id;
    // A div rather than a button, because the row contains a button of its own ("丢弃") and a
    // button inside a button is invalid — the browser hoists it out and the layout breaks.
    row.setAttribute('role', 'button');
    row.tabIndex = 0;
    row.innerHTML = `
      <div class="msg-top">
        <span class="unread-dot read" aria-hidden="true"></span>
        <span class="msg-from">${escapeHtml(draft.to_addrs || '(未填收件人)')}</span>
        <span class="msg-date">${escapeHtml(formatDate(draft.updated_at))}</span>
      </div>
      <div class="msg-subject">${escapeHtml(draft.subject || '(无主题)')}</div>
      <div class="draft-actions">
        <span class="muted">${
          draft.in_reply_to ? '回复草稿' : draft.body_html ? '富文本草稿' : '纯文本草稿'
        }</span>
        <button type="button" class="btn ghost" data-role="discard">丢弃</button>
      </div>`;

    row.addEventListener('click', (event) => {
      if (event.target?.dataset?.role === 'discard') {
        discardDraft(draft);
        return;
      }
      openComposer({ mode: 'draft', draft });
    });
    row.addEventListener('keydown', (event) => {
      if (event.key === 'Enter' || event.key === ' ') {
        event.preventDefault();
        openComposer({ mode: 'draft', draft });
      }
    });
    list.appendChild(row);
  }
}

async function discardDraft(draft) {
  const what = draft.subject || '(无主题)';
  if (!confirm(`丢弃草稿「${what}」？\n这会永久删除，不能撤销。`)) return;

  const removed = await run('丢弃草稿', () => invoke('delete_draft', { id: draft.id }));
  if (removed === null) return;

  if (!removed) {
    // The row was already gone (another window, or the composer saved and deleted it). Refresh so
    // the list matches the database rather than keeping a row that no longer exists.
    toast('这条草稿已经不在了', 'info');
  } else {
    toast('草稿已丢弃', 'ok');
  }
  await refreshDrafts();
}

/* ---------------------------------------------------------------- outbox */

/** How the stored state reads to a person. */
const OUTBOX_LABELS = {
  queued: '等待发送',
  sending: '正在发送',
  sent: '已发送',
  failed: '发送失败',
  cancelled: '已撤销',
  needs_attention: '需要你决定',
};

/**
 * Show what the outbox is holding for the current account.
 *
 * Everything here comes from `user.db`, so a message queued offline is visible immediately, and the
 * list still works with the network down — which is precisely when a user wants to know whether
 * their mail left.
 */
async function showOutbox() {
  if (!state.accountId) {
    toast('请先添加一个邮箱账号。', 'err');
    return;
  }

  exitSearchMode({ reload: false });
  $('search-input').value = '';
  $('message-list').innerHTML = '<div class="empty"><span class="spinner"></span>正在读发件箱…</div>';

  const items = await run('读取发件箱', () => invoke('list_outbox', { accountId: state.accountId }));
  if (items === null) {
    renderMessages();
    return;
  }

  state.view = 'outbox';
  state.outbox = items;
  state.selectedUid = null;
  state.detail = null;
  $('mailbox-title').textContent = '发件箱';
  renderReader();
  syncMailboxRows();
  renderMessages();
}

/** Re-read the outbox and keep the sidebar badge honest. */
async function refreshOutbox() {
  if (!state.accountId) return;
  const items = await invoke('list_outbox', { accountId: state.accountId }).catch(() => null);
  if (items === null) return;

  state.outbox = items;
  syncMailboxRows();
  if (state.view === 'outbox') renderMessages();
}

function renderOutbox(list) {
  $('list-footer').hidden = true;

  if (state.outbox.length === 0) {
    list.innerHTML = '<div class="empty">发件箱是空的。发送的邮件会先排在这里，再一封封送出去。</div>';
    return;
  }

  for (const item of state.outbox) {
    const state_ = item.state;
    const preview = outboxPreview(item);
    const row = document.createElement('div');
    row.className = 'msg' + (state_ === 'failed' || state_ === 'needs_attention' ? ' outbox-trouble' : '');
    row.dataset.outboxId = String(item.id);
    row.setAttribute('role', 'button');
    row.tabIndex = 0;

    const undo = undoCountdown(item);
    row.innerHTML = `
      <div class="msg-top">
        <span class="unread-dot read" aria-hidden="true"></span>
        <span class="msg-from">${escapeHtml(preview.destinations)}</span>
        <span class="msg-date">${escapeHtml(formatDate(item.created_at))}</span>
      </div>
      <div class="msg-subject">${escapeHtml(preview.subject)}</div>
      <div class="draft-actions">
        <span class="muted outbox-state">${escapeHtml(OUTBOX_LABELS[state_] ?? state_)}${
          item.attempts > 0 ? ` · 已试 ${item.attempts} 次` : ''
        }</span>
        <span class="outbox-buttons"></span>
      </div>
      ${
        undo
          ? `<div class="outbox-note" data-undo="1" data-until="${item.undo_until}">${escapeHtml(undo)}</div>`
          : ''
      }
      ${
        item.last_error && (state_ === 'failed' || state_ === 'needs_attention')
          ? `<div class="outbox-error">${escapeHtml(item.last_error)}</div>`
          : ''
      }`;

    const buttons = row.querySelector('.outbox-buttons');
    // Undo exists only while the message has not been attempted; afterwards the honest offer is
    // "try again", not a button that pretends the send can be taken back (design/07 §5.2).
    if (state_ === 'queued') {
      buttons.appendChild(rowButton('撤销', () => cancelSend(item.id)));
    } else if (state_ === 'failed' || state_ === 'needs_attention' || state_ === 'cancelled') {
      buttons.appendChild(rowButton('重试', () => retrySend(item.id)));
    } else if (state_ === 'sent') {
      // The copy is on the server, not in this list: the button opens it where it lives.
      buttons.appendChild(rowButton('查看副本', () => openSentCopy(item)));
    }
    if (state_ !== 'sending') {
      buttons.appendChild(rowButton('删除', () => discardOutboxItem(item.id)));
    }

    // The row itself is not clickable: an outbox item has no reader to open. Keeping it a div with
    // a role means the buttons inside are the only targets.
    list.appendChild(row);
  }

  updateUndoCountdowns();
}

function rowButton(label, onClick) {
  const button = document.createElement('button');
  button.type = 'button';
  button.className = 'btn ghost';
  button.textContent = label;
  button.addEventListener('click', (event) => {
    event.stopPropagation();
    onClick();
  });
  return button;
}

/** "N 秒内可撤销", or nothing when the window has already passed. */
function undoCountdown(item) {
  if (item.state !== 'queued') return '';
  const remaining = Math.ceil(item.undo_until - nowSeconds());
  if (remaining <= 0) return '';
  return `${remaining} 秒内可撤销`;
}

/**
 * What to show for one outbox row.
 *
 * The row stores the payload it was built from rather than a subject and a recipient list, so the
 * preview is read back out of it. That is on purpose — one copy of the message, the one that will
 * actually be sent — and it is why a row whose payload cannot be parsed must still render.
 */
function outboxPreview(item) {
  try {
    const mail = JSON.parse(item.payload_json);
    const destinations = [...(mail.to ?? []), ...(mail.cc ?? [])].join(', ');
    return {
      destinations: destinations || '(未填收件人)',
      subject: mail.subject || '(无主题)',
    };
  } catch {
    return { destinations: '(内容读不出来)', subject: '(内容读不出来)' };
  }
}

/**
 * Keep the countdown honest without re-rendering the list.
 *
 * Re-building the rows once a second would replace the node under the pointer, so the click that
 * was about to land on 撤销 would miss. Only the text of the countdown is touched.
 */
function updateUndoCountdowns() {
  if (state.view !== 'outbox') return;
  for (const element of document.querySelectorAll('#message-list [data-undo]')) {
    const remaining = Math.ceil(Number(element.dataset.until) - nowSeconds());
    element.textContent = remaining > 0 ? `${remaining} 秒内可撤销` : '正在发送…';
  }
}

async function cancelSend(id) {
  const cancelled = await run('撤销发送', () => invoke('cancel_send', { id }));
  if (cancelled === null) return;

  toast(cancelled ? '已撤销，这封信没有发出去' : '已经交给服务器了，撤不回来了', cancelled ? 'ok' : 'err');
  await refreshOutbox();
}

/**
 * Open the Sent copy of a delivered message.
 *
 * The link (folder + UID) lives in the cache's `sent_index`, keyed by the frozen `Message-ID` — the
 * same identity the send path used, and the reason the outbox row does not need to remember it. A
 * `null` UID is a documented outcome (the server does not always return one on APPEND), so the
 * folder is opened and the user is told rather than being shown a button that does nothing.
 */
async function openSentCopy(item) {
  const preview = outboxPreview(item);
  const messageId = item.sent_message_id;
  if (!messageId) {
    toast('这条记录里没有 Message-ID，找不到对应的副本。', 'err');
    return;
  }

  const record = await run('查找已发送副本', () =>
    invoke('sent_copy', { accountId: state.accountId, messageId })
  );
  if (record === null) return;

  if (!record) {
    // Two honest reasons, and today they are indistinguishable from here: the copy may never have
    // been filed (the server refused a copy this large — `design/07` §5.4's `SkippedTooLarge`), or the
    // cache that held the link was rebuilt. Saying "可能" beats a confident wrong answer.
    toast(
      '这封信没有登记副本：可能是副本太大没能归档，也可能是本地缓存被清理过。' +
        '可以在「文件夹 → 已发送」里按主题找。',
      'info',
      9000
    );
    return;
  }

  const box = state.mailboxes.find((mailbox) => mailbox.name === record.folder);
  if (!box) {
    toast(`找不到文件夹「${record.folder}」，它可能已被删除或改名。`, 'err');
    return;
  }
  if (record.uid === undefined || record.uid === null) {
    toast(`副本在「${box.display_name || box.name}」里，但服务器没有给它的编号，请在那里按主题查找。`, 'info', 9000);
    return;
  }

  state.view = 'mail';
  exitSearchMode({ reload: false });
  selectMailbox(box);
  renderMailboxes();
  await loadMessages({ reset: true });

  // The reader fetches the body by (folder, UID), so the summary handed in only has to be enough to
  // paint until the detail arrives.
  await openMessage({
    uid: record.uid,
    subject: preview.subject === '(无主题)' ? '' : preview.subject,
    from_name: null,
    from_addr: null,
    date: null,
    seen: true,
    size: null,
    flagged: false,
  });
}

async function retrySend(id) {  const queued = await run('重新发送', () => invoke('retry_send', { id }));
  if (queued === null) return;
  toast(queued ? '已重新排队' : '这封信的状态不允许重试', queued ? 'ok' : 'err');
  await refreshOutbox();
}

async function discardOutboxItem(id) {
  if (!confirm('删除这条发件记录？\n删除后这封信不会再尝试发送。')) return;
  const removed = await run('删除发件记录', () => invoke('discard_outbox', { id }));
  if (removed === null) return;
  toast('已删除', 'ok');
  await refreshOutbox();
}

/* ---------------------------------------------------------------- messages */

/**
 * Fetch the next page of older mail: **from the cache first**, then from the server.
 *
 * The two sources are not interchangeable and the order is the whole point. The list shows one page
 * (`PAGE_SIZE`) while the first sync stores a whole window (500) and every history fetch adds to the
 * cache too — so the cache almost always holds more than the list is showing. Going straight to the
 * server (which is what this did until 2026-10-06) meant that on a mailbox with nothing older on the
 * server, the button announced "没有更早的邮件了" while 142 already-downloaded messages were never
 * shown; `src/history.js` owns the rule and `tools/history_test.mjs` pins it.
 *
 * The backend does the "which UIDs" work for the server leg (`load_more_messages` ->
 * `fetch_older_headers`), and neither leg touches the incremental cursor: `last_uid` keeps pointing at
 * the newest stored message, so scrolling into history can never make the updater skip mail.
 */
async function loadOlder() {
  if (state.historyDone || !state.accountId || !state.mailbox) return;

  const button = $('btn-load-more');
  button.disabled = true;
  button.textContent = '正在加载更早的邮件…';

  const page = await run('读取本地缓存', () =>
    invoke('message_page', {
      accountId: state.accountId,
      mailbox: state.mailbox,
      // A page, not "everything so far plus one more": the offset is the number of rows already on
      // screen, so each press transfers one page instead of re-reading the whole visible window
      // (round 75). No refresh happens on this path — asking to see older mail must not cost a sync.
      offset: state.messages.length,
      limit: PAGE_SIZE,
    })
  );
  if (page === null) {
    // Failed: put the button back so the user can retry, and leave the list alone.
    button.disabled = false;
    renderHistoryButton();
    return;
  }

  const step = history.historyStep({
    pageCount: page.length,
    serverDone: state.historyDone,
  });

  if (step === 'show-cached') {
    // Append. `list_summaries` orders by date desc, uid desc and the offset is what is already on
    // screen, so the page continues the list — no re-sorting the whole window, which is what made
    // every press O(rows shown). Dedup by UID stays as a guard: a sync that inserted new mail
    // between two presses can shift the window by one row.
    state.messages = appendUnique(state.messages, page);
    renderMessages();
    button.disabled = false;
    renderHistoryButton();
    return;
  }

  if (step === 'done') {
    state.historyDone = true;
    button.disabled = false;
    renderHistoryButton();
    return;
  }

  const older = await run('加载更早的邮件', () =>
    invoke('load_more_messages', {
      accountId: state.accountId,
      mailbox: state.mailbox,
      limit: PAGE_SIZE,
    })
  );

  if (older === null) {
    button.disabled = false;
    renderHistoryButton();
    return;
  }

  if (older.messages.length > 0) {
    state.messages = appendUnique(state.messages, older.messages);
    renderMessages();
  }

  if (older.done || older.messages.length === 0) {
    state.historyDone = true;
  }
  button.disabled = false;
  renderHistoryButton();
}

/**
 * Append rows that are not already listed, keeping the newest-first order.
 *
 * The order argument: the server returns rows already sorted newest-first (the cache query is
 * `order by date desc, uid desc`) and every append continues below what is on screen, so the
 * concatenation stays ordered. Rows are still de-duplicated by UID — the window can shift between
 * two presses if a sync inserted mail in between, and a repeated row in the list is a bug a user
 * would notice ("the same mail twice") but a skipped row is one they might not.
 */
function appendUnique(existing, incoming) {
  const known = new Set(existing.map((m) => m.uid));
  return existing.concat(incoming.filter((m) => !known.has(m.uid)));
}

/** Put the load-more button into the state the current knowledge implies. */
function renderHistoryButton() {
  const button = $('btn-load-more');
  if (!button) return;
  button.disabled = state.historyDone;
  button.textContent = state.historyDone ? '没有更早的邮件了' : '加载更多';
}

async function loadMessages({ reset = false } = {}) {
  if (!state.accountId || !state.mailbox) return;
  // The local lists are not backed by the server: fetching a folder here would both waste a round
  // trip and paint mail over the list the user is looking at.
  if (state.view !== 'mail') return;
  if (reset) {
    state.selectedUid = null;
    state.selectedMessage = null;
    state.detail = null;
    // A conversation belongs to the message that was open, not to the folder or the search that comes
    // next; keeping it would paint the previous thread's bubbles over the new list.
    state.threadMembers = [];
    state.threadFocus = null;
    state.threadKey = null;
    state.threadBodies = new Map();
    // Folds belong to the rows on screen; a different folder or a search is a different list.
    state.expandedThreads.clear();
    // A new folder (or a refresh) starts over: whatever we learned about the bottom of the previous
    // one does not apply here.
    state.historyDone = false;
    renderHistoryButton();
    renderReader();
  }

  $('mailbox-title').textContent = state.mailboxLabel || state.mailbox;

  const list = $('message-list');
  list.innerHTML = '<div class="empty"><span class="spinner"></span>正在收取邮件…</div>';

  const messages = await run('收取邮件', () =>
    invoke('list_messages', {
      accountId: state.accountId,
      mailbox: state.mailbox,
      // The first page only. 「加载更多」 pages from here by offset (`message_page`), so this number
      // is a page size, not a growing window — `state.limit` used to be the growing one and is gone.
      limit: PAGE_SIZE,
    })
  );

  if (messages === null) {
    list.innerHTML = '<div class="empty">收取失败，请查看右下角提示。</div>';
    return;
  }

  // Newest first: the server returns ascending UIDs and people want the latest on top.
  state.messages = messages.slice().sort((a, b) => b.uid - a.uid);
  renderMessages();
  // Tell the user whether this list is fresh or cached, and why. The refresh just ran, so this is
  // the one moment the answer is current; doing it before the list would paint a stale banner over
  // a fresh one.
  void refreshSyncBanner();
  // The badges count the cache, and this listing just changed it. Not awaited: the list on screen
  // is the thing the user asked for, and a slow badge must not hold it up.
  void loadUnreadCounts();
}

/**
 * Show (or hide) "this is cached data" and say why, in the words that fit the failure.
 *
 * The wording comes from the backend's `failure_kind`, not from the error text: an expired
 * authorization code and a dead network both stop a refresh, but "check your network" is useless
 * advice to someone whose 授权码 needs regenerating (`design/20-v0.2-plan.md` C3). It disappears on
 * the next successful refresh rather than lingering, because a banner that never clears teaches
 * people to ignore it.
 */
async function refreshSyncBanner() {
  const banner = $('sync-banner');
  const status = await invoke('sync_status', { accountId: state.accountId }).catch(() => null);
  if (!status) {
    banner.hidden = true;
    return;
  }
  state.syncStatus = status;

  if (!status.offline) {
    banner.hidden = true;
    banner.replaceChildren();
    return;
  }

  const words = {
    network: '现在连不上服务器，下面显示的是上次同步的内容。',
    auth: '服务器拒绝了登录凭据（授权码可能已过期），点「重试」或重新添加账号。',
    quota: '服务商的当日用量到顶了，今天不再同步，下面的内容是最近一次取回的。',
    server: '同步没成功，下面的内容是上次同步的。',
    local: '本地数据库出错，收发已暂停；邮件本身没有丢。',
    busy: '本地数据库正忙，稍等一下会自动重试。',
  };
  banner.replaceChildren(document.createTextNode(words[status.failure_kind] || status.last_error || '同步未成功。'));

  const retry = document.createElement('button');
  retry.type = 'button';
  retry.className = 'btn ghost small';
  retry.textContent = '重试';
  retry.addEventListener('click', () => loadMessages({ reset: true }));

  banner.className = 'sync-banner' + (status.failure_kind === 'auth' ? ' auth' : '');
  banner.append(retry);
  banner.hidden = false;
}

function renderMessages() {
  const list = $('message-list');
  list.innerHTML = '';

  // While searching, the same list element shows hits from every folder instead of one folder.
  if (state.searchQuery) {
    renderSearchHits(list);
    return;
  }

  // Same element again, third role: the local drafts list.
  if (state.view === 'drafts') {
    renderDrafts(list);
    return;
  }

  // And fourth: the outbox, which is also local-only.
  if (state.view === 'outbox') {
    renderOutbox(list);
    return;
  }

  if (state.messages.length === 0) {
    list.innerHTML = '<div class="empty">这个文件夹里还没有邮件。</div>';
    state.listOwner = null;
    state.listRows = [];
    state.listWindow = null;
    $('list-footer').hidden = true;
    return;
  }

  // One row per conversation rather than one per message (`design/07` §2). The key is the
  // `References` root the backend computed, falling back to subject+sender for rows that predate it
  // (`src/threads.js`) — and the count on the row says how many messages were folded, so nothing is
  // hidden behind a single subject.
  const conversations = threads.groupConversations(state.messages);
  state.listRows = conversationRows(conversations);
  state.listOwner = 'mail';
  state.listWindow = null;

  // Windowed (`design/15` §6): the scrollbar gets the whole list's height from a sizer, and only the
  // rows of the current window are in the DOM. Without this the DOM held one button per conversation
  // forever, so every scroll re-laid-out every row — the exact cost P3 exists to catch.
  const sizer = document.createElement('div');
  sizer.className = 'vlist-sizer';
  const window_ = document.createElement('div');
  window_.className = 'vlist-window';
  sizer.appendChild(window_);
  list.replaceChildren(sizer);
  state.listSizer = sizer;
  state.listWindowEl = window_;
  renderListWindow();

  $('list-footer').hidden = state.historyDone;
}

/**
 * Row descriptors for the mail list: one per conversation, plus one per message of each expanded
 * conversation, in the order they appear on screen.
 *
 * Kept separate from DOM building because the window changes on every scroll and rebuilding
 * descriptions would lose the fold state and the selection a dozen times per second. `unread` and
 * `selected` are computed per row, exactly where they are needed.
 */
function conversationRows(conversations) {
  const rows = [];
  for (const conversation of conversations) {
    const message = conversation.messages[0];
    const extra = conversation.messages.length - 1;
    const folded = state.expandedThreads.has(conversation.key);
    rows.push({
      kind: 'conversation',
      key: conversation.key,
      message,
      extra,
      // A conversation is unread when any of its messages is: hiding an unread one behind a read
      // subject is how a list starts lying about what needs attention.
      unread: conversation.messages.some((entry) => !entry.seen),
      folded,
    });
    if (folded) {
      // The rest of the conversation, oldest last. Each is a real row the user can open: a fold that
      // hides messages without a way to reach them is just a shorter list.
      for (const entry of conversation.messages.slice(1)) {
        rows.push({ kind: 'child', key: conversation.key, entry, unread: !entry.seen });
      }
    }
  }
  return rows;
}

/** Build one conversation row. The same markup as before windowing, per row. */
function buildConversationRow(row) {
  const { message, extra, unread, folded } = row;
  const button = document.createElement('button');
  // Highlighted by conversation as well as by message: in the conversation view the reader can be
  // focused on a reply that lives in another folder, and the row on screen is still the one the user
  // is reading. Without this the list would look unselected while a message from it is open.
  const selected =
    (message.uid === state.selectedUid && state.mailbox === state.selectedFolder) ||
    (state.threadKey !== null && message.thread_key === state.threadKey);
  button.className = 'msg' + (unread ? ' unread' : '') + (selected ? ' selected' : '');
  button.dataset.uid = String(message.uid);
  button.innerHTML = `
    <div class="msg-top">
      ${
        unread
          ? '<span class="unread-dot" role="img" aria-label="未读" title="此会话有未读邮件"></span>'
          : '<span class="unread-dot read" aria-hidden="true"></span>'
      }
      <span class="msg-from">${
        extra > 0
          ? `<span class="msg-caret" role="button" title="${
              folded ? '收起这个会话' : '展开这个会话'
            }">${folded ? '▾' : '▸'}</span>`
          : ''
      }${escapeHtml(senderLabel(message))}</span>
      <span class="msg-date">${escapeHtml(formatDate(message.date))}</span>
    </div>
    <div class="msg-subject">${escapeHtml(message.subject)}${
      extra > 0 ? `<span class="msg-count" title="这个会话里的邮件数">${extra + 1}</span>` : ''
    }</div>`;

  // The caret toggles the fold; the rest of the row opens the newest message. Keeping those two
  // apart matters: "read the newest" is what a click on a row means everywhere else in the app.
  button.addEventListener('click', (event) => {
    if (extra > 0 && event.target?.classList?.contains('msg-caret')) {
      event.stopPropagation();
      toggleThread(row.key);
      return;
    }
    openMessage(message);
  });
  return button;
}

/** Build one row of an expanded conversation. */
function buildChildRow(row) {
  const entry = row.entry;
  const child = document.createElement('button');
  child.className = 'msg thread-child' + (entry.seen ? '' : ' unread');
  child.dataset.uid = String(entry.uid);
  child.innerHTML = `
    <div class="msg-top">
      ${
        entry.seen
          ? '<span class="unread-dot read" aria-hidden="true"></span>'
          : '<span class="unread-dot" role="img" aria-label="未读" title="未读"></span>'
      }
      <span class="msg-from">${escapeHtml(senderLabel(entry))}</span>
      <span class="msg-date">${escapeHtml(formatDate(entry.date))}</span>
    </div>
    <div class="msg-subject">${escapeHtml(entry.subject)}</div>`;
  child.addEventListener('click', () => openMessage(entry));
  return child;
}

/**
 * Render the current window of the mail list.
 *
 * Called from a `requestAnimationFrame` after a scroll (one commit per frame, `design/15` §6) and
 * directly whenever the rows change. The cheap exit matters: a wheel gesture fires more scroll events
 * than there are frames, and most of them land in a window that is already on screen.
 */
function renderListWindow() {
  if (state.listOwner !== 'mail' || !state.listSizer || !state.listWindowEl) return;
  const list = $('message-list');
  const rows = state.listRows;
  const next = vlist.computeWindow({
    scrollTop: list.scrollTop,
    viewportHeight: list.clientHeight,
    count: rows.length,
  });
  if (!vlist.windowChanged(state.listWindow, next)) return;
  state.listWindow = next;

  state.listSizer.style.height = `${next.totalHeight}px`;
  state.listWindowEl.style.top = `${next.offsetTop}px`;

  const built = [];
  for (let index = next.start; index < next.end; index += 1) {
    const row = rows[index];
    built.push(row.kind === 'child' ? buildChildRow(row) : buildConversationRow(row));
  }
  state.listWindowEl.replaceChildren(...built);
}

/** How many rows the scroll handler produced since it was armed (one edit per frame, not per event). */
function wireListScrolling() {
  const list = $('message-list');
  let scheduled = false;
  list.addEventListener(
    'scroll',
    () => {
      if (scheduled) return;
      scheduled = true;
      requestAnimationFrame(() => {
        scheduled = false;
        renderListWindow();
      });
    },
    { passive: true }
  );
}

/**
 * Bring one row of the windowed list into view.
 *
 * `scrollIntoView` cannot be used: the row that is wanted is very often not in the DOM at all, which
 * is the point of windowing. So the offset is computed from the row height instead.
 */
function scrollToRow(index) {
  const list = $('message-list');
  const top = vlist.scrollTopForRow(index, {
    scrollTop: list.scrollTop,
    viewportHeight: list.clientHeight,
  });
  list.scrollTop = top;
  renderListWindow();
}

/**
 * Which windowed row belongs to a message (by UID) or a conversation (by key), or -1.
 *
 * UID alone for messages, because the windowed list always belongs to the mailbox it is showing —
 * a summary row carries no `folder` to compare against. The key form is what the fold needs: a
 * conversation's key is not any of its UIDs.
 */
function rowIndexOf({ uid = null, key = null } = {}) {
  return state.listRows.findIndex((row) => {
    if (key !== null) return row.kind === 'conversation' && row.key === key;
    const entry = row.kind === 'child' ? row.entry : row.message;
    return Boolean(entry) && entry.uid === uid;
  });
}

/** Open or close one conversation in the list. */
function toggleThread(key) {
  const expanding = !state.expandedThreads.has(key);
  if (expanding) {
    state.expandedThreads.add(key);
  } else {
    state.expandedThreads.delete(key);
  }
  renderMessages();
  // Expanding changes the row count, so the fold's rows may well be below the window. Keep them
  // where the user is looking instead of re-rendering from the top.
  if (expanding) {
    const index = rowIndexOf({ key });
    if (index >= 0) scrollToRow(index);
  }
}

/** Search results, each tagged with the folder it came from. */
function renderSearchHits(list) {
  $('list-footer').hidden = true;

  if (state.searchHits.length === 0) {
    list.innerHTML = '<div class="empty">没有匹配的邮件。</div>';
    return;
  }

  for (const hit of state.searchHits) {
    const button = document.createElement('button');
    const selected = hit.uid === state.selectedUid && hit.folder === state.selectedFolder;
    button.className = 'msg' + (hit.seen ? '' : ' unread') + (selected ? ' selected' : '');
    // The same marker the folder list uses: a search hit is mail, and unread mail has to look
    // unread wherever it appears. The `seen: hit.seen` line two branches below reads it too.
    const dot = hit.seen
      ? '<span class="unread-dot read" aria-hidden="true"></span>'
      : '<span class="unread-dot" role="img" aria-label="未读" title="未读"></span>';
    // Tag only the hits that come from somewhere other than the folder on screen — the tag exists to
    // answer "why is this here?", and while everything is in the current folder that is noise.
    const tag =
      hit.folder === state.mailbox
        ? ''
        : `<span class="msg-folder">${escapeHtml(folderLabel(hit.folder))}</span>`;
    button.innerHTML = `
      <div class="msg-top">
        ${dot}
        <span class="msg-from">${tag}${escapeHtml(senderLabel(hit))}</span>
        <span class="msg-date">${escapeHtml(formatDate(hit.date))}</span>
      </div>
      <div class="msg-subject">${escapeHtml(hit.subject)}</div>`;
    button.addEventListener('click', () => openHit(hit));
    list.appendChild(button);
  }
}

async function openMessage(message, folder = state.mailbox) {
  state.selectedUid = message.uid;
  // The folder travels with the message, not with the list. They used to be assumed equal, which held
  // only because `openHit` rewrites `state.mailbox` to a search hit's folder — a coincidence, and not
  // one a conversation view can keep, because a thread spans folders. Everything that acts on "the
  // open message" (read, delete, attachments, fetching the body) now asks `state.selectedFolder`,
  // so the message being looked at and the message being modified cannot drift apart.
  state.selectedFolder = folder;
  state.selectedMessage = message;
  state.detail = null;
  state.bodyMode = 'html';
  // Blocking is the default for every message; an opt-in must not leak into the next one.
  state.allowRemoteImages = false;
  // A different message is a different conversation until proven otherwise.
  state.threadMembers = [];
  state.threadFocus = null;
  state.threadKey = null;
  state.threadBodies = new Map();
  renderMessages();

  $('reader-placeholder').hidden = true;
  $('reader-body').hidden = false;
  $('reader-subject').textContent = message.subject;
  $('reader-from-name').textContent = senderLabel(message);
  $('reader-from-addr').textContent = message.from_addr ? `<${message.from_addr}>` : '';
  $('reader-date').textContent = message.date
    ? new Date(message.date * 1000).toLocaleString('zh-CN')
    : '';
  $('reader-to').textContent = '';
  $('attachment-bar').hidden = true;
  $('body-frame').srcdoc = '';
  $('body-text').textContent = '正在读取正文…';

  const detail = await run('读取邮件正文', () =>
    invoke('fetch_message', {
      accountId: state.accountId,
      mailbox: folder,
      uid: message.uid,
    })
  );
  if (!detail) return;

  state.detail = detail;
  // The clicked message is the one the user is reading, so its body is already in hand for the
  // conversation view — expanding its bubble never waits for a round trip.
  state.threadBodies.set(`${folder}|${message.uid}`, detail);

  await loadConversation(message);
  renderReader();
  markSeen(message, folder);
}

/**
 * Work out whether the open message belongs to a conversation, and load its members if it does.
 *
 * Two sources, in this order, and the difference matters:
 *
 * 1. **The exact key**, answered by the backend across folders. This is the one that can show the
 *    user's own replies, because those live in the Sent folder rather than in the list on screen.
 * 2. **The list's own grouping** (subject + counterpart), used only when the row predates
 *    `thread_key`. It can only see the folder on display, so it is an approximation — and the bubble
 *    view does not pretend otherwise: it simply contains what it has.
 *
 * A conversation of one is not a conversation: nothing is loaded, and the reader stays on the single
 * message, so an ordinary email does not change appearance for the sake of the feature.
 */
async function loadConversation(message) {
  state.threadMembers = [];
  state.threadFocus = null;
  state.threadKey = null;
  if (state.readerMode !== 'thread') return;

  if (message.thread_key) {
    const members = await run('读取会话', () =>
      invoke('list_thread', {
        accountId: state.accountId,
        threadKey: message.thread_key,
      })
    );
    if (members && members.length >= 1) {
      state.threadMembers = members;
      state.threadKey = message.thread_key;
      // Everything starts **collapsed** (user's request, 2026-10-06), so opening a conversation shows
      // the shape of it — who said what, when — rather than one message in full. The opened message is
      // still what `state.selectedMessage`/`state.detail` point at, so the toolbar keeps working on it.
      state.threadFocus = null;
      // Not awaited: the conversation paints from headers immediately and fills in as bodies arrive.
      void prefetchThreadBodies(members);
      return;
    }
  }

  // Only rows with no exact key reach here, and only from a real folder listing: search hits come from
  // several folders at once, so "the conversation" is not a thing this approximation can see.
  //
  // It groups the page exactly the way the list does (`threads.groupConversations`, which unions the
  // chain with the sender-and-subject heuristic) rather than by one key, so a row the backend cannot
  // answer for and the row it is drawn next to cannot disagree about how many messages there are.
  if (state.view !== 'mail' || state.searchQuery) return;
  const group = threads
    .groupConversations(state.messages)
    .find((entry) => entry.messages.some((member) => member.uid === message.uid));
  const siblings = group?.messages ?? [];
  if (siblings.length < 1) return;

  state.threadMembers = siblings
    .slice()
    .sort((a, b) => (a.date ?? 0) - (b.date ?? 0))
    .map((summary) => ({ folder: state.mailbox, summary }));
  state.threadFocus = null;
  void prefetchThreadBodies(state.threadMembers);
}

/**
 * Move the (single) body rendering surface into whichever view is showing it.
 *
 * There is exactly one, and it holds the only sandboxed iframe on screen. Two consequences, both
 * wanted: "显示图片" can never leak from one message of a conversation to the next, and a twenty-message
 * thread is not twenty copies of the renderer.
 */
function mountBodySurface(host) {
  const surface = $('body-surface');
  if (surface.parentElement !== host) host.appendChild(surface);
  return surface;
}

/** Reading a message marks it \Seen; reflect that locally so the list agrees with the server. */
function markSeen(summary, folder) {
  if (!summary || summary.seen) return;
  summary.seen = true;
  renderMessages();
  // One fewer unread in this folder. Re-read rather than decrement: the same message could be open
  // from a conversation that spans folders, and the count belongs to the folder it was fetched in.
  void loadUnreadCounts();
  run('标记已读', () =>
    invoke('mark_seen', {
      accountId: state.accountId,
      mailbox: folder,
      uid: summary.uid,
      seen: true,
    })
  );
}

function renderReader() {
  const detail = state.detail;
  // 对话模式 applies to **every** message, including one that is not part of a thread (user's request,
  // 2026-10-06): a single message is a conversation of one, and showing it as a bubble keeps the view
  // from changing shape depending on what happens to be in the mailbox. `threadMembers` is empty only
  // when the mode is 普通模式, or for a search hit / local view the conversation cannot be assembled for.
  const conversation = state.readerMode === 'thread' && state.threadMembers.length >= 1;

  if (conversation) {
    $('reader-placeholder').hidden = true;
    $('reader-body').hidden = true;
    $('thread-view').hidden = false;
    renderThreadReader();
    return;
  }

  $('thread-view').hidden = true;

  if (!detail) {
    $('reader-placeholder').hidden = false;
    $('reader-body').hidden = true;
    return;
  }

  $('reader-placeholder').hidden = true;
  $('reader-body').hidden = false;
  mountBodySurface($('reader-body'));

  $('reader-subject').textContent = detail.subject;
  $('reader-from-name').textContent = detail.from_name || detail.from_addr || '(未知发件人)';
  $('reader-from-addr').textContent = detail.from_addr ? `<${detail.from_addr}>` : '';
  // The header clamps both of these with an ellipsis, so keep the full text reachable on hover.
  $('reader-from-name').title = $('reader-from-name').textContent;
  $('reader-from-addr').title = detail.from_addr || '';
  $('reader-date').textContent = detail.date
    ? new Date(detail.date * 1000).toLocaleString('zh-CN')
    : '';

  const recipients = [];
  if (detail.to.length) recipients.push(`收件人：${detail.to.join(', ')}`);
  if (detail.cc.length) recipients.push(`抄送：${detail.cc.join(', ')}`);
  $('reader-to').textContent = recipients.join('　');

  renderAttachmentBar(detail);
  applyBodyMode();
}

/** The attachment chips for one message. Shared by both views, because the surface is shared. */
function renderAttachmentBar(detail) {
  const bar = $('attachment-bar');
  bar.innerHTML = '';
  if (detail.attachments.length) {
    bar.hidden = false;
    for (const attachment of detail.attachments) {
      // A button, not a span: clicking saves the file to the downloads folder.
      const chip = document.createElement('button');
      chip.className = 'attachment';
      chip.title = `保存到下载文件夹：${attachment.name}`;
      chip.innerHTML = `<span class="name">📎 ${escapeHtml(
        attachment.name
      )}</span><span class="muted">${escapeHtml(formatBytes(attachment.size))}</span>`;
      chip.addEventListener('click', () => saveAttachment(chip, attachment));
      bar.appendChild(chip);
    }
  } else {
    bar.hidden = true;
  }
}

/**
 * The conversation, drawn as bubbles (`design/08` §2/§5.2).
 *
 * One bubble per message with the user's own on the right. **Every bubble starts collapsed** and
 * clicking one expands it; clicking its head again collapses it (user's request, 2026-10-06). At most
 * one bubble is expanded at a time, and that is not a shortcut: there is exactly one body surface and
 * therefore exactly one sandboxed renderer, which is what stops "显示图片" from leaking between the
 * messages of a conversation (`check.ps1` pins the single surface). Expanding a second bubble
 * therefore collapses the first.
 */
function renderThreadReader() {
  const members = state.threadMembers;
  const focusedKey = state.threadFocus;
  const newest = members[members.length - 1];

  $('thread-subject').textContent = newest.summary.subject || '(无主题)';
  const people = [...new Set(members.map((member) => member.summary.from_addr).filter(Boolean))];
  $('thread-meta').textContent =
    `共 ${members.length} 封　${people.join('、')}` +
    (focusedKey ? '' : '　·　点任意一封展开');

  // Park the shared surface in the reader *before* clearing the list: it lives inside the expanded
  // bubble, and wiping the list would throw the element — and the live iframe in it — away.
  const surface = $('body-surface');
  if (surface && !$('reader-body').contains(surface)) $('reader-body').appendChild(surface);

  const list = $('thread-bubbles');
  list.innerHTML = '';

  let focusedBubble = null;
  for (const member of members) {
    const key = bubbleKey(member);
    const bubble = renderBubble(member, key === focusedKey);
    if (key === focusedKey) focusedBubble = bubble;
    list.appendChild(bubble);
  }

  renderThreadReplyTarget();

  if (focusedBubble) {
    mountBodySurface(focusedBubble.querySelector('.bubble-body'));
    renderAttachmentBar(state.detail);
    applyBodyMode();
    // Deferred by one frame: the bubble's height comes from the body surface that was just mounted into
    // it, and scrolling before that layout pass measured a shorter element — which left the focused
    // message half below the fold. Measured, not assumed.
    requestAnimationFrame(() => focusedBubble.scrollIntoView({ block: 'nearest' }));
  }
}

const bubbleKey = (member) => `${member.folder}|${member.summary.uid}`;

function renderBubble(member, expanded) {
  const summary = member.summary;
  const key = bubbleKey(member);
  const detail = state.threadBodies.get(key);

  const bubble = document.createElement('div');
  bubble.className =
    'bubble' + (isMine(summary.from_addr) ? ' mine' : '') + (expanded ? ' focused' : '');
  bubble.dataset.key = key;

  const head = document.createElement('div');
  head.className = 'bubble-head';
  const chevron = document.createElement('span');
  chevron.className = 'chevron';
  chevron.textContent = expanded ? '▾' : '▸';
  const who = document.createElement('span');
  who.className = 'who';
  who.textContent = senderLabel(summary);
  who.title = summary.from_addr || '';
  const when = document.createElement('time');
  when.textContent = summary.date ? new Date(summary.date * 1000).toLocaleString('zh-CN') : '';
  head.append(chevron, who, when);
  head.setAttribute('role', 'button');
  head.setAttribute('aria-expanded', expanded ? 'true' : 'false');
  head.title = expanded ? '收起这一封' : '展开这一封';
  // The head toggles in **both** directions. On an expanded bubble it is the only clickable part: the
  // body below is a live document the user is reading and selecting text in.
  //
  // `stopPropagation` is not optional: a collapsed bubble also listens on itself (so that clicking
  // anywhere on it works), and without this the one click would toggle twice — expand, then collapse —
  // which is exactly how this read on the running app: "点了一点反应都没有". Measured, not guessed.
  head.addEventListener('click', (event) => {
    event.stopPropagation();
    void toggleBubble(key);
  });
  bubble.appendChild(head);

  if (expanded) {
    const body = document.createElement('div');
    body.className = 'bubble-body';
    bubble.appendChild(body);
    return bubble;
  }

  // A collapsed bubble shows what we already have and says so when we have nothing. It is always
  // `textContent`, never `innerHTML`: the sandboxed iframe stays the only place mail HTML is rendered.
  const preview = document.createElement('div');
  if (detail) {
    preview.className = 'bubble-preview';
    preview.textContent = previewText(detail);
  } else {
    preview.className = 'bubble-note';
    preview.textContent = summary.subject ? `${summary.subject} —— 点开查看正文` : '点开查看正文';
  }
  bubble.appendChild(preview);
  // A collapsed bubble is clickable anywhere — the whole thing is the target, not just the head. Its
  // head click stops propagation (see above), so this never fires twice for one click.
  bubble.addEventListener('click', () => {
    void toggleBubble(key);
  });
  return bubble;
}

/** The body of one message of the conversation, fetched once and kept for the life of the thread. */
async function ensureThreadDetail(key) {
  if (state.threadBodies.has(key)) return state.threadBodies.get(key);
  const member = state.threadMembers.find((entry) => bubbleKey(entry) === key);
  if (!member) return null;

  const detail = await run('读取邮件正文', () =>
    invoke('fetch_message', {
      accountId: state.accountId,
      mailbox: member.folder,
      uid: member.summary.uid,
    })
  );
  if (!detail) return null;
  state.threadBodies.set(key, detail);
  return detail;
}

/**
 * Expand one message of the conversation, or collapse it again if it is the expanded one.
 *
 * Expanding a different bubble collapses the current one — see the note on `renderThreadReader`.
 */
async function toggleBubble(key) {
  if (state.threadFocus === key) {
    state.threadFocus = null;
    renderThreadReader();
    return;
  }

  const member = state.threadMembers.find((entry) => bubbleKey(entry) === key);
  if (!member) return;

  state.threadFocus = key;
  state.selectedUid = member.summary.uid;
  state.selectedFolder = member.folder;
  state.selectedMessage = member.summary;
  // Per message, exactly as in the single view: unlocking images in one bubble must not unlock the next.
  state.allowRemoteImages = false;
  state.bodyMode = 'html';

  const detail = await ensureThreadDetail(key);
  if (!detail) return;

  state.detail = detail;
  renderThreadReader();
  renderMessages();
  markSeen(member.summary, member.folder);
}

/**
 * Fetch the rest of the conversation's bodies, newest first, bounded and one at a time.
 *
 * Bounded because "a conversation" has no upper bound in the data and every fetch is a round trip;
 * sequential because this is background work the user did not ask for, sharing a small connection pool.
 * Each arrival fills one preview **in place** — a full re-render would move the body surface and reload
 * the iframe the user is reading.
 */
async function prefetchThreadBodies(members) {
  const queue = members
    .slice()
    .reverse()
    .filter((member) => !state.threadBodies.has(bubbleKey(member)))
    .slice(0, THREAD_PREFETCH_MAX);

  for (const member of queue) {
    // The user moved on: stop doing work for a conversation nobody is looking at. Keyed on the member
    // list itself rather than on the expanded bubble — the whole point of this pass is the *collapsed*
    // previews, and expanding one must not abandon the rest of them.
    if (state.threadMembers !== members) return;

    let detail = null;
    try {
      detail = await invoke('fetch_message', {
        accountId: state.accountId,
        mailbox: member.folder,
        uid: member.summary.uid,
      });
    } catch {
      // Background work must not raise a toast: the bubble keeps saying "点开查看正文", and clicking it
      // fetches in the foreground where a failure is explained.
      return;
    }
    if (!detail) return;
    state.threadBodies.set(bubbleKey(member), detail);

    // Addressed by class, not by position: `> div:last-child` used to be the preview, but on a bubble
    // the user has since expanded it is the `.bubble-body` that holds the live body surface, and
    // overwriting that would destroy the message being read.
    const preview = $('thread-bubbles').querySelector(
      `[data-key="${bubbleKey(member)}"] .bubble-preview, [data-key="${bubbleKey(member)}"] .bubble-note`
    );
    if (preview) {
      preview.className = 'bubble-preview';
      preview.textContent = previewText(detail);
    }
  }
}

/** How many bodies a conversation fetches in the background. */
const THREAD_PREFETCH_MAX = 8;
/** How much of a message a collapsed bubble shows. */
const PREVIEW_CHARS = 220;

/**
 * A one-glance version of a message body, for a collapsed bubble.
 *
 * Plain text on purpose — the caller writes it with `textContent` — so nothing in an untrusted body
 * can become markup here.
 */
function previewText(detail) {
  const source = (detail.body_text && detail.body_text.trim()) || detail.body_html || '';
  const text = decodeEntities(stripTags(source)).replace(/\s+/g, ' ').trim();
  return text.slice(0, PREVIEW_CHARS) || '(空邮件)';
}

/**
 * The handful of entities that actually turn up in mail, decoded once for text that is about to be
 * *read as text* rather than parsed. `&amp;` goes last so `&amp;lt;` decodes to `&lt;` and not `<`.
 */
function decodeEntities(text) {
  return text
    .replace(/&nbsp;/gi, ' ')
    .replace(/&lt;/gi, '<')
    .replace(/&gt;/gi, '>')
    .replace(/&quot;/gi, '"')
    .replace(/&#39;|&apos;/gi, "'")
    .replace(/&amp;/gi, '&');
}

/** Whether an address is one of the user's own — which is what puts a bubble on the right. */
function isMine(addr) {
  const value = String(addr ?? '')
    .trim()
    .toLowerCase();
  if (!value) return false;
  if (state.accountId && state.accountId.toLowerCase() === value) return true;
  return state.accounts.some((account) => String(account.email).toLowerCase() === value);
}

/* ---------------------------------------------------------------- replying inside a conversation */

/**
 * The message a reply typed at the bottom of the conversation will answer.
 *
 * `src/reply.js` owns the rule; this only hands it the account list it deliberately does not know
 * about, and the expanded bubble (the message the user is pointing at).
 */
function threadReplyTarget() {
  const focused =
    state.threadMembers.find((member) => bubbleKey(member) === state.threadFocus) ?? null;
  return reply.replyTarget(state.threadMembers, focused, isMine);
}

/** Say who the reply goes to *before* the button is pressed — never after. */
function renderThreadReplyTarget() {
  const line = $('thread-reply-target');
  if (!line) return;
  const target = threadReplyTarget();
  const address = target ? target.summary.reply_to || target.summary.from_addr : '';
  line.textContent = address
    ? `回复给 ${address}`
    : '这个会话里只有你自己的邮件，没有可回复的对方地址';
}

/**
 * Send what is in the conversation's reply box.
 *
 * Deliberately not routed through `openComposer`: the whole point of this box is that answering costs
 * one line of typing and no dialog. Everything that matters is still shared with the composer — the
 * payload builder (`payload.outgoingMail`), the threading headers (`reply.replyHeaders`) and the
 * outbox, which is what keeps the undo window and the delivery guarantees identical.
 */
async function sendThreadReply() {
  if (!state.accountId) {
    toast('请先添加一个邮箱账号。', 'err');
    return;
  }
  const box = $('thread-reply-text');
  const text = box.value.trim();
  if (!text) {
    toast('先写点内容再发送。', 'err');
    box.focus();
    return;
  }

  const target = threadReplyTarget();
  if (!target) {
    toast('这个会话里只有你自己的邮件。请用上面的「回复」打开完整写信窗口填写收件人。', 'err', 9000);
    return;
  }

  const detail = await ensureThreadDetail(bubbleKey(target));
  if (!detail) return;
  const headers = reply.replyHeaders(detail);
  if (!headers.to) {
    toast('这封邮件没有可用的发件人地址，请用上面的「回复」打开完整写信窗口。', 'err', 9000);
    return;
  }

  const outgoing = payload.outgoingMail({
    to: [headers.to],
    cc: [],
    bcc: [],
    subject: headers.subject,
    bodyText: text,
    // Plain text, exactly like the composer: the send path builds the alternative part from it.
    bodyHtml: null,
    inReplyTo: headers.inReplyTo,
    references: headers.references,
    attachments: [],
  });

  const ok = await run('加入发件箱', async () => {
    // Through the outbox like every other send: that is what the undo window and the "survives a
    // crash mid-transfer" guarantee come from. The worker owns delivery from here.
    await invoke('enqueue_send', {
      accountId: state.accountId,
      mail: outgoing,
      // No draft: this box is a reply, not a document to come back to. The outbox keeps the copy.
      draftId: null,
      undoSeconds: UNDO_SECONDS,
    });
    return true;
  });
  if (!ok) return;

  box.value = '';
  toast(`已加入发件箱（${UNDO_SECONDS} 秒内可撤销）`, 'ok', 6000, {
    label: '撤销',
    onClick: () => cancelNewestSend(),
  });
  refreshOutbox();
}

/**
 * Save one attachment into the downloads folder.
 *
 * The backend owns the filename: an attachment name is chosen by the sender, so it is sanitised
 * there. Disabling the chip while the save runs matters because the download refetches the message,
 * which on a large attachment is not instant.
 */
async function saveAttachment(chip, attachment) {
  const detail = state.detail;
  if (!detail) return;

  chip.disabled = true;
  try {
    const saved = await run('保存附件', () =>
      invoke('save_attachment', {
        accountId: state.accountId,
        mailbox: state.selectedFolder,
        uid: detail.uid,
        part: attachment.part,
        name: attachment.name,
      })
    );
    if (saved) toast(`已保存：${saved}`, 'info', 12000);
  } finally {
    chip.disabled = false;
  }
}

function setBodyMode(mode) {
  state.bodyMode = mode;
  applyBodyMode();
}

function applyBodyMode() {
  const detail = state.detail;
  if (!detail) return;

  const hasHtml = Boolean(detail.body_html && detail.body_html.trim());
  const hasText = Boolean(detail.body_text && detail.body_text.trim());
  const htmlTab = $('tab-html');
  const textTab = $('tab-text');

  htmlTab.disabled = !hasHtml;
  textTab.disabled = !hasText;
  if (state.bodyMode === 'html' && !hasHtml) state.bodyMode = 'text';
  if (state.bodyMode === 'text' && !hasText) state.bodyMode = 'html';

  htmlTab.classList.toggle('active', state.bodyMode === 'html');
  textTab.classList.toggle('active', state.bodyMode === 'text');

  const frame = $('body-frame');
  const pre = $('body-text');

  if (state.bodyMode === 'html') {
    frame.hidden = false;
    pre.hidden = true;
    const html = resolveInlineImages(
      detail.body_html || '<p>(无正文)</p>',
      detail.inline_images || []
    );
    updateRemoteImageBar(html);
    updateLinkBar(html);
    frame.srcdoc = buildSandboxedDocument(html, state.allowRemoteImages);
  } else {
    frame.hidden = true;
    pre.hidden = false;
    $('remote-image-bar').hidden = true;
    $('link-bar').hidden = true;
    pre.textContent = detail.body_text || '(无正文)';
  }
}

/**
 * The links in this message, as `{ href, text }`.
 *
 * `data-ee-href` rather than `href` because the body has no followable `href` at all: the reading
 * pane is a sandboxed frame that could otherwise navigate itself to a page of the sender's choosing
 * (`sanitize::sanitize_mail_html` strips them, and that is the point). Parsed with `DOMParser`, which
 * has no browsing context — reading the links must not fetch them.
 */
function linksInHtml(html) {
  const parsed = new DOMParser().parseFromString(String(html ?? ''), 'text/html');
  const seen = new Set();
  const links = [];
  for (const anchor of parsed.querySelectorAll('a[data-ee-href], area[data-ee-href]')) {
    const href = anchor.getAttribute('data-ee-href') || '';
    if (!href || seen.has(href)) continue;
    seen.add(href);
    const text = (anchor.textContent || '').trim().slice(0, 60);
    links.push({ href, text });
  }
  return links;
}

/** Render one chip per link, labelled with the host it will really visit. */
function updateLinkBar(html) {
  const bar = $('link-bar');
  const links = linksInHtml(html);
  if (!links.length) {
    bar.hidden = true;
    bar.innerHTML = '';
    return;
  }

  bar.hidden = false;
  bar.innerHTML = `<span class="link-bar-title">邮件里的链接（${links.length}）</span>`;
  for (const link of links) {
    const button = document.createElement('button');
    button.className = 'link-chip';
    button.type = 'button';
    // `textContent`, never `innerHTML`: this is mail-derived text.
    button.textContent = link.text ? `${link.text} — ${hostOf(link.href)}` : hostOf(link.href);
    button.title = link.href;
    button.addEventListener('click', () => askAboutLink(link));
    bar.appendChild(button);
  }
}

/** The host as it will actually be connected to, or the raw string when it is not a URL. */
function hostOf(href) {
  try {
    return new URL(href).host || href;
  } catch {
    return href;
  }
}

/**
 * Classify first, open second.
 *
 * Nothing but an ordinary link opens on the first click. A different domain in the text, a punycode
 * host or a shortener all put the verdict in front of the user *before* the browser is launched —
 * which is the whole reason the link bar exists rather than a clickable `href`.
 */
async function askAboutLink(link) {
  const verdict = await invoke('classify_link', { url: link.href, text: link.text }).catch((error) => {
    toast(errText(error), 'bad');
    return null;
  });
  if (!verdict) return;

  if (verdict.risk === 'safe') {
    await openClassifiedLink(link, verdict);
    return;
  }

  const bar = $('link-bar');
  bar.hidden = false;
  bar.innerHTML = '';

  const warning = document.createElement('span');
  warning.className = 'link-warning';
  // The reason comes from Rust and is already a sentence; it names the real host.
  warning.textContent = `${verdict.reason} 完整地址：${verdict.full}`;

  const confirm = document.createElement('button');
  confirm.className = 'btn ghost';
  confirm.type = 'button';
  confirm.textContent = verdict.risk === 'dangerous_scheme' ? '不打开' : '仍要打开';
  confirm.addEventListener('click', () => {
    if (verdict.risk === 'dangerous_scheme') {
      updateLinkBar(state.detail?.body_html || '');
      return;
    }
    openClassifiedLink(link, verdict);
  });

  const cancel = document.createElement('button');
  cancel.className = 'btn ghost';
  cancel.type = 'button';
  cancel.textContent = '取消';
  cancel.addEventListener('click', () => updateLinkBar(state.detail?.body_html || ''));

  bar.append(warning, confirm, cancel);
}

async function openClassifiedLink(link, verdict) {
  try {
    await invoke('open_link', { url: link.href, text: link.text });
    toast(`已在浏览器打开 ${verdict.host || verdict.full}`, 'info');
  } catch (error) {
    toast(errText(error), 'bad');
  }
}

/** Remote images in the body, i.e. the ones that would phone home when rendered. */
function countRemoteImages(html) {
  const remote = /https?:\/\//i;
  let count = 0;

  // `srcset` is a second way to name a URL on the same element, and the engine picks whichever
  // candidate it likes — counting only `src` reported "0 blocked" for a body that had a tracking
  // pixel in `srcset`, which took away the warning bar and with it the opt-in affordance.
  for (const tag of html.match(/<img\b[^>]*>/gi) || []) {
    const src = tag.match(/\bsrc\s*=\s*["']?\s*([^"'\s>]+)/i);
    if (src && remote.test(src[1])) count += 1;
    const srcset = tag.match(/\bsrcset\s*=\s*["']([^"']*)["']/i);
    if (srcset && remote.test(srcset[1])) count += 1;
  }

  // CSS-referenced images (`background: url(https://…)`) are blocked by the same `img-src` and are
  // just as good a beacon, so they belong in the number the user is shown.
  count += (html.match(/url\(\s*["']?\s*https?:\/\//gi) || []).length;

  return count;
}

function updateRemoteImageBar(html) {
  const bar = $('remote-image-bar');
  const blocked = countRemoteImages(html);

  // Nothing remote, or the user already opted in for this message: no bar.
  if (blocked === 0 || state.allowRemoteImages) {
    bar.hidden = true;
    return;
  }

  bar.hidden = false;
  $('remote-image-text').textContent =
    `已拦截 ${blocked} 张远程图片 —— 加载它们等于告诉发件人你打开了这封邮件。`;
}

/**
 * Swap `cid:` references for `data:` URIs.
 *
 * HTML mail almost always embeds its logos and charts as MIME parts referenced by Content-ID.
 * Those bytes travelled with the message, so rendering them fetches nothing and leaks nothing —
 * there is no reason to block them, and leaving them unresolved is what produces broken images.
 *
 * Matching is done on the normalised id (no angle brackets) because the header carries
 * `<logo@x>` while the HTML says `cid:logo@x`.
 */
function resolveInlineImages(html, inlineImages) {
  if (!inlineImages.length || !html.includes('cid:')) return html;

  const byId = new Map(inlineImages.map((img) => [img.content_id, img]));

  return html.replace(
    /(<img\b[^>]*?\bsrc\s*=\s*)(["']?)cid:([^"'\s>]+)\2/gi,
    (whole, prefix, quote, rawId) => {
      // Percent-encoding is common in Content-IDs; decode before looking up.
      let id = rawId;
      try {
        id = decodeURIComponent(rawId);
      } catch {
        /* keep the raw form */
      }
      const image = byId.get(id) || byId.get(rawId);
      if (!image) return whole;
      return `${prefix}${quote}data:${image.mime};base64,${image.data_base64}${quote}`;
    }
  );
}

/**
 * Wrap a mail body so it cannot do anything.
 *
 * Two independent layers, because either one alone is insufficient:
 *  1. `sandbox` on the iframe (set in index.html) — no scripts, no forms, no same-origin access,
 *     no top-level navigation. The body cannot touch the app.
 *  2. This CSP — `default-src 'none'` blocks every fetch. Inline styles stay allowed so the
 *     message keeps its layout; `data:` images stay allowed so embedded (`cid:`) images show.
 *
 * Remote images are blocked unless the user explicitly opts in for this one message, because
 * loading them is exactly how a tracking pixel reports that the mail was opened.
 */
function buildSandboxedDocument(html, allowRemoteImages) {
  const csp = [
    "default-src 'none'",
    allowRemoteImages ? 'img-src data: blob: https: http:' : 'img-src data: blob:',
    "style-src 'unsafe-inline'",
    "font-src data:",
    "media-src 'none'",
    "script-src 'none'",
    "form-action 'none'",
    "base-uri 'none'",
  ].join('; ');

  return `<!DOCTYPE html>
<html><head>
<meta charset="utf-8">
<meta http-equiv="Content-Security-Policy" content="${csp}">
<style>
  html, body { margin: 0; padding: 16px; background: #fff; color: #16203a; }
  body { font: 14px/1.6 "Segoe UI", "Microsoft YaHei", system-ui, sans-serif; word-break: break-word; }

  /*
    Fit a fixed-width newsletter into the reading pane.

    Real mail is laid out for a 600-700px desktop window; the reading pane here is ~400px. Without
    these rules an HTML newsletter overflows sideways and gets cropped — measured on a real 广发银行
    message: content 752px in a 406px pane, 142 elements past the right edge, because the markup
    pins widths with inline style="width:720px" and width="720px" attributes.

    NOTE: no backticks anywhere in this comment. This whole block is inside a JS template literal,
    and a stray backtick terminates the string and kills the entire renderer with a SyntaxError —
    which looks exactly like "the UI is blank" while every Rust build stays green. It has happened
    twice now; tools/check.ps1 runs node --check on src/*.js to catch it.

    max-width alone does NOT fix a table: a table's width is driven by its cells' minimum content
    width, so it stayed at 720px. The fixed widths have to be neutralised, which needs !important
    because the widths are inline styles — and that is exactly where the risk is.

    width:auto applied broadly collapses a table with width="100%" to 6% of the pane, which would
    wreck ordinary mail. So the percentage forms are explicitly excluded and only pixel widths are
    touched. Verified with a fixture: width="100%" stays 1.00 of the pane, width="50%" stays 0.50,
    inline width:100% stays 1.00, while a 720px newsletter goes 752px to 406px with no overflow.

    No height cap on purpose: capping an 800x3765 banner at 420px shrinks it to 89px WIDE (the
    aspect ratio is preserved), i.e. an unreadable sliver. Capping width is the whole fix.
  */
  img { max-width: 100% !important; height: auto !important; }

  table[width]:not([width$="%"]),
  td[width]:not([width$="%"]),
  th[width]:not([width$="%"]) { width: auto !important; }

  table[style*="width:"]:not([style*="%"]),
  td[style*="width:"]:not([style*="%"]),
  th[style*="width:"]:not([style*="%"]),
  div[style*="width:"]:not([style*="%"]),
  p[style*="width:"]:not([style*="%"]) { width: auto !important; }

  table, td, th, div, p { max-width: 100% !important; }

  blockquote { margin: 8px 0; padding-left: 12px; border-left: 2px solid #dfe3ee; color: #6b7691; }
</style>
</head><body>${html}</body></html>`;
}

/* ---------------------------------------------------------------- message actions */

async function deleteSelected() {
  if (!state.detail) return;
  // The open message, not a lookup in the list: a conversation can be reading a message that is not in
  // the folder being listed, so searching `state.messages` would find nothing (and, for a UID that
  // exists in both folders, find the wrong one).
  const message = state.selectedMessage;
  if (!confirm(`删除这封邮件？\n\n${message ? message.subject : ''}`)) return;

  const ok = await run('删除邮件', async () => {
    await invoke('delete_message', {
      accountId: state.accountId,
      mailbox: state.selectedFolder,
      uid: state.selectedUid,
    });
    return true;
  });

  if (ok) {
    toast('邮件已删除', 'ok');
    state.messages = state.messages.filter(
      (m) => !(m.uid === state.selectedUid && state.selectedFolder === state.mailbox)
    );
    state.selectedUid = null;
    state.selectedMessage = null;
    state.detail = null;
    renderMessages();
    renderReader();
  }
}

async function toggleRead() {
  if (!state.detail) return;
  const message = state.selectedMessage;
  if (!message) return;
  const nextSeen = !message.seen;

  const ok = await run('修改已读状态', async () => {
    await invoke('mark_seen', {
      accountId: state.accountId,
      mailbox: state.selectedFolder,
      uid: message.uid,
      seen: nextSeen,
    });
    return true;
  });

  if (ok) {
    message.seen = nextSeen;
    $('btn-toggle-read').textContent = nextSeen ? '标为未读' : '标为已读';
    renderMessages();
  }
}

/* ---------------------------------------------------------------- composer */

/**
 * The composer: new mail, a reply, a forward, or a draft being picked up again.
 *
 * Everything it writes goes through `save_draft` on a 2 s debounce and once more on close, and the
 * draft row is deleted only after the send succeeded. That ordering is the whole "never lose what
 * the user typed" promise (design/07 §3): the text reaches `user.db` whether the window is closed
 * on purpose, the app is killed, or the machine loses power.
 */
function openComposer({ mode = 'new', draft = null } = {}) {
  if (!state.accountId) {
    toast('请先添加一个邮箱账号。', 'err');
    return;
  }
  // Pinned at open time: the composer belongs to the account it was opened for, even if the
  // sidebar switches accounts behind the dialog.
  const accountId = state.accountId;
  const account = state.accounts.find((a) => a.id === accountId);
  const detail = state.detail;

  const draftId = draft ? draft.id : payload.newDraftId();
  const createdAt = draft ? draft.created_at : nowSeconds();
  // `body_html` is only ever set by a draft that already had one — this composer is plain text.
  // Carrying it through the save keeps a rich draft from being silently downgraded to text.
  const bodyHtml = draft ? draft.body_html : null;

  // Has this composer ever written itself to disk? Until it has, closing an untouched one must not
  // leave an empty "草稿" row behind.
  let persisted = Boolean(draft);
  let sent = false;
  let saveTimer = null;
  /** The 30 s server-mirror timer (`design/07` §5.1); cleared when the composer closes. */
  let mirrorTimer = null;
  /** Removes the window-focus listener the mirror needs; also cleared on close. */
  let stopMirrorOnBlur = null;
  /** The save in flight, so a send can wait for it before deleting the draft. */
  let inFlight = Promise.resolve();

  let to = '';
  let cc = '';
  let bcc = '';
  let subject = '';
  let bodyText = '';
  let inReplyTo = draft ? draft.in_reply_to : null;
  let references = draft ? parseMessageIdList(draft.references) : [];

  if (draft) {
    to = draft.to_addrs;
    cc = draft.cc_addrs;
    bcc = draft.bcc_addrs;
    subject = draft.subject;
    bodyText = draft.body_text;
  } else if (mode === 'reply' && detail) {
    to = detail.reply_to || detail.from_addr || '';
    // The prefix rule lives in reply.js so the composer and the conversation's inline reply box cannot
    // disagree about what makes a subject a reply (`Re:`, `RE:`, `回复：` all count).
    subject = reply.replySubject(detail.subject);
    inReplyTo = detail.message_id;
    references = [...detail.references, detail.message_id].filter(Boolean);
    bodyText = quoteOriginal(detail);
  } else if (mode === 'forward' && detail) {
    subject = reply.forwardSubject(detail.subject);
    bodyText = quoteOriginal(detail);
  }

  const body = `
    <div id="compose-status" class="compose-status"></div>
    <div class="field">
      <label>发件人</label>
      <input value="${escapeHtml(account.email)}" disabled />
    </div>
    <div class="field">
      <label>收件人（多个用逗号分隔）</label>
      <input id="compose-to" placeholder="someone@example.com" value="${escapeHtml(to)}" />
    </div>
    <div class="grid-2">
      <div class="field">
        <label>抄送</label>
        <input id="compose-cc" placeholder="可留空" value="${escapeHtml(cc)}" />
      </div>
      <div class="field">
        <label>密送</label>
        <input id="compose-bcc" placeholder="可留空" value="${escapeHtml(bcc)}" />
      </div>
    </div>
    <div class="field">
      <label>主题</label>
      <input id="compose-subject" value="${escapeHtml(subject)}" />
    </div>
    <div class="field">
      <label>正文</label>
      <textarea id="compose-body" placeholder="写点什么…">${escapeHtml(bodyText)}</textarea>
      <span class="hint">以纯文本发送；如果粘贴了 HTML 也会一并作为富文本正文发出。</span>
    </div>
    <div class="field">
      <label>附件</label>
      <div id="compose-attachments" class="compose-attachments" hidden></div>
      <button type="button" id="btn-add-attachment" class="btn ghost">📎 添加附件</button>
      <span class="hint">文件从磁盘读取，不会经过界面；单个附件过大时服务器可能拒收。</span>
    </div>`;

  // Kept in this closure: the list belongs to one open composer and must not survive it. The shape
  // matches what `pick_attachments` returns plus an optional `sha256` (see payload.js).
  let attachments = draft ? payload.parseAttachments(draft.attachments_json) : [];

  /** Everything the draft is built from. Read while the dialog is still in the DOM. */
  const readFields = (modal) => ({
    to: modal.querySelector('#compose-to').value,
    cc: modal.querySelector('#compose-cc').value,
    bcc: modal.querySelector('#compose-bcc').value,
    subject: modal.querySelector('#compose-subject').value,
    bodyText: modal.querySelector('#compose-body').value,
  });

  const isBlank = (fields) =>
    !fields.to.trim() &&
    !fields.cc.trim() &&
    !fields.bcc.trim() &&
    !fields.subject.trim() &&
    !fields.bodyText.trim() &&
    attachments.length === 0;

  const setStatus = (text, kind = 'info') => {
    const el = document.querySelector('#compose-status');
    if (!el) return;
    el.textContent = text;
    el.className = `compose-status ${kind}`;
  };

  /**
   * Write the draft to `user.db`.
   *
   * Upsert on the same id, so the autosave timer can fire as often as the user pauses. Never
   * throws: a failed autosave has to leave the composer usable and say so, because the text is
   * still only in the dialog and the user needs to know that before closing it.
   */
  const saveDraft = async (fields) => {
    const record = payload.draft({
      id: draftId,
      accountId,
      toAddrs: fields.to,
      ccAddrs: fields.cc,
      bccAddrs: fields.bcc,
      subject: fields.subject,
      bodyText: fields.bodyText,
      bodyHtml,
      attachmentsJson: payload.serializeAttachments(attachments),
      inReplyTo,
      referencesJson: JSON.stringify(references),
      // No identity picker yet; the send path uses the account address.
      identity: null,
      createdAt,
      updatedAt: nowSeconds(),
    });

    try {
      await invoke('save_draft', { draft: record });
    } catch (error) {
      const message = `草稿保存失败：${errText(error)}`;
      setStatus(message, 'err');
      // Closing the dialog removes the status line, so a failure on the way out needs another way
      // to reach the user — this is the one path where their text is not on disk.
      if (!document.querySelector('#compose-status')) toast(message, 'err', 12000);
      return false;
    }

    persisted = true;
    setStatus(`草稿 · 已保存 ${clockLabel(nowSeconds())}`);
    refreshDrafts();
    return true;
  };

  const queueSave = (fields) => {
    inFlight = inFlight.then(() => saveDraft(fields)).catch(() => {});
    return inFlight;
  };

  /** Debounced save: one write per pause in typing, not per keystroke. */
  const scheduleSave = (modal) => {
    clearTimeout(saveTimer);
    saveTimer = setTimeout(() => {
      saveTimer = null;
      const fields = readFields(modal);
      // An untouched composer is not a draft yet. Once it has been saved, later emptiness is the
      // user deleting their own text, and that is a state worth keeping (design/07 §9: prefer one
      // extra draft over removing something the user was writing).
      if (isBlank(fields) && !persisted) return;
      queueSave(fields);
    }, DRAFT_AUTOSAVE_MS);
  };

  const renderAttachments = (modal) => {
    const box = modal.querySelector('#compose-attachments');
    box.innerHTML = '';
    box.hidden = attachments.length === 0;

    attachments.forEach((file, index) => {
      const chip = document.createElement('span');
      chip.className = 'attachment';
      chip.innerHTML = `<span class="name">📎 ${escapeHtml(file.name)}</span><span class="muted">${escapeHtml(
        formatBytes(file.size)
      )}</span><button type="button" class="remove" data-remove="${index}" title="移除">✕</button>`;
      box.appendChild(chip);
    });
  };

  openModal({
    title: draft ? '草稿' : mode === 'reply' ? '回复' : mode === 'forward' ? '转发' : '写邮件',
    bodyHtml: body,
    confirmLabel: '发送',
    onReady: (modal) => {
      // Inside `onReady`, not before `openModal`: the status line belongs to the DOM that
      // `openModal` has just built, and writing to it earlier would address the previous dialog.
      setStatus(
        draft
          ? `草稿 · 上次保存 ${clockLabel(draft.updated_at)}`
          : `写完后自动保存（${DRAFT_AUTOSAVE_MS / 1000} 秒）`
      );

      modal.querySelector('#btn-add-attachment').addEventListener('click', async () => {
        const picked = await run('选择附件', () => invoke('pick_attachments'));
        if (!picked || picked.length === 0) return;

        // `design/15` §4 / `design/07` §6: refuse at the moment the file is chosen rather than after
        // the mail has been written. The estimate is deliberately rough (base64 inflates by about a
        // third, plus headers) and the authoritative check is still the one `enqueue_send` makes
        // before freezing the bytes — this one exists so nobody types for ten minutes first.
        const total = [...attachments, ...picked].reduce((sum, file) => sum + (file.size ?? 0), 0);
        const cap = await sendCapBytes();
        if (cap && estimatedEncodedBytes(total) > cap) {
          toast(
            `这些附件加起来约 ${formatBytes(total)}，编码后约 ${formatBytes(
              estimatedEncodedBytes(total)
            )}，超过可发送上限 ${formatBytes(cap)}。请压缩、改用云盘链接，或分两封发。`,
            'err',
            12000
          );
          return;
        }

        attachments = attachments.concat(picked);
        renderAttachments(modal);
        scheduleSave(modal);
      });

      // One listener on the container rather than one per chip: the chips are re-created on every
      // change, and per-chip listeners would leak with them.
      modal.querySelector('#compose-attachments').addEventListener('click', (event) => {
        const index = event.target?.dataset?.remove;
        if (index === undefined) return;
        attachments.splice(Number(index), 1);
        renderAttachments(modal);
        scheduleSave(modal);
      });

      // The attach list is filled in before `onReady` runs, so a resumed draft shows its files
      // straight away rather than only after the next edit.
      renderAttachments(modal);

      // One delegated listener for every field: the composer has five of them and they all mean
      // the same thing — "the draft changed".
      modal.querySelector('.modal-body').addEventListener('input', () => scheduleSave(modal));
      modal.querySelector('.modal-body').addEventListener('change', () => scheduleSave(modal));

      // `design/07` §5.1 asks for the mirror every 30 s *or* on losing focus, not only on close:
      // somebody who leaves a half-written mail open for an hour should still find it on their phone.
      // It mirrors the stored draft rather than reading the fields, so it can never send the server
      // something the local save has not already agreed to.
      mirrorTimer = setInterval(() => {
        if (!sent) mirrorDraft(draftId, accountId, persisted);
      }, MIRROR_INTERVAL_MS);

      // The design's other trigger: losing focus means the user has gone off to do something else,
      // which is exactly when they might open the same draft on another device. Listeners are added
      // per composer and removed on close — a stale one would mirror a draft that is already gone.
      const onBlur = () => {
        if (!sent) mirrorDraft(draftId, accountId, persisted);
      };
      window.addEventListener('blur', onBlur);
      stopMirrorOnBlur = () => window.removeEventListener('blur', onBlur);
    },
    onClose: (modal) => {
      clearTimeout(saveTimer);
      saveTimer = null;
      clearInterval(mirrorTimer);
      mirrorTimer = null;
      if (stopMirrorOnBlur) {
        stopMirrorOnBlur();
        stopMirrorOnBlur = null;
      }
      // A queued send kept the draft on purpose — the worker deletes it once the message is really
      // out — so there is nothing to save again here.
      if (sent) return;
      const fields = readFields(modal);
      const blank = isBlank(fields) && !persisted;
      if (!blank) queueSave(fields);
      // `design/07` §3: the local draft is the truth, the server copy is the second one. Best-effort
      // and after the local save, because a mirror failure must never cost the user what they typed.
      if (!blank) {
        inFlight = inFlight.then(() => mirrorDraft(draftId, accountId, persisted)).catch(() => {});
      }
    },
    onConfirm: async (modal) => {
      const fields = readFields(modal);
      // Any queued autosave must land before anything else touches the row.
      clearTimeout(saveTimer);
      saveTimer = null;
      await inFlight;

      const toList = splitAddresses(fields.to);
      if (toList.length === 0) {
        toast('请填写收件人。', 'err');
        return false;
      }

      // Save the draft as it stands *now*, before queueing. The row is not deleted here any more
      // (the worker does that once the message is delivered), so this is what makes 草稿箱 show the
      // last edit rather than the one from two seconds ago if the send later fails.
      await saveDraft(fields);

      const outgoing = payload.outgoingMail({
        to: toList,
        cc: splitAddresses(fields.cc),
        bcc: splitAddresses(fields.bcc),
        subject: fields.subject,
        bodyText: fields.bodyText,
        // The composer is plain text; `send` builds the alternative part from it.
        bodyHtml: null,
        inReplyTo,
        references,
        // Built through the shared builder, including `sha256`: the hash is what lets the backend
        // notice a file that was edited or deleted after it was chosen, instead of sending bytes the
        // user never approved.
        attachments: attachments.map((file) =>
          payload.attachment({
            name: file.name,
            mime: file.mime,
            path: file.path,
            size: file.size,
            sha256: file.sha256,
          })
        ),
      });

      const ok = await run('加入发件箱', async () => {
        // Through the outbox, not straight to SMTP: that is what the undo window needs, and it is
        // also what makes a send survive closing the app or losing the network mid-transfer. The
        // worker owns the actual delivery from here on.
        await invoke('enqueue_send', {
          accountId,
          mail: outgoing,
          draftId: persisted ? draftId : null,
          undoSeconds: UNDO_SECONDS,
        });
        return true;
      });
      if (!ok) return false;

      sent = true;
      toast(`已加入发件箱（${UNDO_SECONDS} 秒内可撤销）`, 'ok', 6000, {
        label: '撤销',
        onClick: () => cancelNewestSend(),
      });
      refreshOutbox();
      return true;
    },
  });
}

/**
 * Undo the message that was just queued.
 *
 * Looks up the newest open row rather than remembering an id: the composer is closed by the time
 * this runs, and the row it created is the one at the top of the queue. `cancel_send` reports false
 * when the worker got there first, which is the honest answer — the message is gone.
 */
async function cancelNewestSend() {
  const open = state.outbox
    .filter((item) => item.state === 'queued')
    .sort((a, b) => b.id - a.id);
  const newest = open[0];
  if (!newest) {
    // The badge may simply be stale: re-read before telling the user anything.
    await refreshOutbox();
    const again = state.outbox
      .filter((item) => item.state === 'queued')
      .sort((a, b) => b.id - a.id)[0];
    if (!again) {
      toast('这封信已经交给服务器了，撤不回来', 'err');
      return;
    }
    await cancelSend(again.id);
    return;
  }
  await cancelSend(newest.id);
}

/** Comma/semicolon separated addresses, both the ASCII and the full-width forms. */
function splitAddresses(value) {
  return String(value ?? '')
    .split(/[,;，；]/)
    .map((s) => s.trim())
    .filter(Boolean);
}

/** Seconds since the epoch, the unit every stored timestamp uses. */
function nowSeconds() {
  return Math.floor(Date.now() / 1000);
}

function clockLabel(unixSeconds) {
  if (!unixSeconds) return '';
  return new Date(unixSeconds * 1000).toLocaleTimeString('zh-CN', {
    hour: '2-digit',
    minute: '2-digit',
  });
}

/** The message-id list a draft stores as JSON. Unreadable JSON means "no threading", not "fail". */
function parseMessageIdList(json) {
  let parsed;
  try {
    parsed = JSON.parse(json ?? '[]');
  } catch {
    return [];
  }
  return Array.isArray(parsed) ? parsed.filter((id) => typeof id === 'string' && id) : [];
}

function quoteOriginal(detail) {
  const who = detail.from_name
    ? `${detail.from_name} <${detail.from_addr}>`
    : detail.from_addr || '';
  const when = detail.date ? new Date(detail.date * 1000).toLocaleString('zh-CN') : '';
  const original = detail.body_text || stripTags(detail.body_html || '');
  const quoted = original
    .split('\n')
    .map((line) => `> ${line}`)
    .join('\n');
  return `\n\n-------- 原始邮件 --------\n发件人：${who}\n时间：${when}\n主题：${detail.subject}\n\n${quoted}\n`;
}

function stripTags(html) {
  // `DOMParser` rather than `innerHTML` on a detached element. Both are inert as far as *scripts*
  // go — the shell CSP has `script-src 'self'` with no `unsafe-inline` — but a detached `innerHTML`
  // is parsed by the document the app lives in, whose CSP deliberately allows `img-src https:` so
  // that the per-message "显示图片" opt-in can work. A `<img src="https://tracker/…">` in the body
  // therefore became a parse-time request that no opt-in ever authorised. A `DOMParser` document has
  // no browsing context, so images in it are never fetched.
  const parsed = new DOMParser().parseFromString(String(html ?? ''), 'text/html');
  return parsed.body ? parsed.body.textContent || '' : '';
}

/**
 * Base64 is 4/3, and headers/boundaries add a little on top.
 *
 * An *estimate*, on purpose: the number that decides is the one `enqueue_send` computes from the
 * bytes it actually built. Being a little pessimistic here is the cheap direction — the user is told
 * about a problem while they can still choose a smaller file.
 */
function estimatedEncodedBytes(bytes) {
  return Math.ceil(bytes * (4 / 3)) + 4096;
}

/** The cap for the current account, read from the backend once and then remembered. */
async function sendCapBytes() {
  if (state.sendCap) return state.sendCap;
  const cap = await invoke('max_send_bytes', { accountId: state.accountId }).catch(() => null);
  if (cap) state.sendCap = cap;
  return cap;
}
/** The sentence the drafts area shows when the server copy is behind. */
const CLOUD_DRAFT_PENDING = '本地已保存（云端草稿待同步）';

/**
 * How often an open composer mirrors itself to the server (`design/07` §5.1).
 *
 * The local save is every 2 s; this one costs a network round trip and an APPEND, so it is ordered
 * of magnitude rarer — and the design's other trigger (losing focus) covers "the user walked away
 * and is now reading mail on their phone".
 */
const MIRROR_INTERVAL_MS = 30000;

/**
 * Copy a saved draft to the server's Drafts folder, best-effort (`design/07` §3).
 *
 * Never throws and never toasts an error: the local draft is already on disk, which is the promise
 * that matters. What a failure does produce is the status line the design asks for, so the user can
 * tell "saved here and on the server" from "saved here only" — without either of them being an
 * interruption.
 *
 * `persisted` is passed in because the composer's own flag is updated asynchronously; mirroring a
 * draft the local save never wrote would create a server copy with nothing behind it.
 */
async function mirrorDraft(draftId, accountId, persisted) {
  if (!persisted) return null;
  try {
    const report = await invoke('mirror_draft', { draftId });
    setComposeCloudStatus(report ? '' : CLOUD_DRAFT_PENDING);
    return report;
  } catch {
    setComposeCloudStatus(CLOUD_DRAFT_PENDING);
    return null;
  }
}

/**
 * Show (or clear) the "cloud copy is behind" note.
 *
 * Written into whichever composer surface is still on screen: the status line if the dialog is
 * open, and the drafts list afterwards — a note that only exists while the dialog is open would
 * disappear exactly when the user needs it.
 */
function setComposeCloudStatus(text) {
  const status = document.querySelector('#compose-status');
  if (status) {
    status.textContent = text;
    status.className = `compose-status ${text ? 'err' : 'info'}`;
    return;
  }
  if (text) toast(text, 'info', 8000);
}

/* ---------------------------------------------------------------- account wizard */

/**
 * Account whose credential was saved but which failed verification.
 *
 * Kept so a retry does not require retyping the authorization code. If the user gives up and
 * closes the dialog, `closeModal` removes it so a typo cannot leave a broken account behind.
 */
let pendingWizardAccountId = null;

/**
 * "Which build is this, and where do I get a newer one."
 *
 * Deliberately not an update *checker*: that needs a signing key and a hosted `latest.json`, and a
 * button that always says "checking…" and never finds anything is worse than one that opens the
 * release page (`design/20-v0.2-plan.md` V2-C1).
 */
/**
 * The diagnostics bundle, previewed then written.
 *
 * The order is the feature (`design/16` §3.3): assemble **nothing on disk**, show the user exactly
 * what a bundle contains, and only then write it. A single "export" button would put a copy of
 * someone's log and account list on disk before they had seen what was in it — which is the whole
 * reason `preview_only` exists on the backend.
 *
 * The redaction is the backend's job; what this function owes the user is the **accounting**: how
 * many things were hidden, and the path the file will land in, both before it exists.
 */
async function wireDiagnostics(modal) {
  const button = modal.querySelector('#btn-diag');
  const preview = modal.querySelector('#diag-preview');
  const status = modal.querySelector('#diag-status');
  const headers = modal.querySelector('#diag-headers');
  let plannedPath = '';

  button.addEventListener('click', async () => {
    button.disabled = true;
    status.hidden = false;
    status.textContent = '正在准备…';
    // The tick is read when the bundle is assembled, not when it is written: the check box says
    // "include this", and the preview is what shows what "this" turned out to be.
    const sample = headers.checked;
    const result = await run('生成诊断包', () =>
      invoke('diagnostics_preview', { includeSampleHeaders: sample })
    );
    if (result === null) {
      status.textContent = '准备失败，请看右下角提示。';
      button.disabled = false;
      return;
    }
    plannedPath = result.planned_path;
    preview.hidden = false;
    preview.textContent = result.preview;
    status.textContent =
      `掩掉了 ${result.manifest.redactions} 项。确认内容后点「写入文件」——` +
      `将保存到 ${plannedPath || '数据目录'}。`;
    // The same button becomes the write step: a second, differently-named action in the same row
    // would be a decision the user has to read twice.
    button.textContent = '写入文件';
    button.disabled = false;
    button.onclick = async () => {
      button.disabled = true;
      status.textContent = '正在写入…';
      // The same tick the preview was built with: writing with the box unticked after previewing with
      // it ticked would put a file on disk that differs from what the user agreed to.
      const written = await run('写入诊断包', () =>
        invoke('diagnostics_export', { includeSampleHeaders: headers.checked })
      );
      button.disabled = false;
      if (written === null) {
        status.textContent = '写入失败，请看右下角提示。';
        return;
      }
      status.textContent = `已写入 ${written}`;
      button.textContent = '已写入';
      button.disabled = true;
    };
  });
}

/**
 * The app's log, tailed.
 *
 * Two things this owes the user beyond showing text: it says the file is **capped** (so "why does
 * it stop at some point" has an answer before they ask), and it says the content is redacted —
 * because a log with addresses blanked looks broken until somebody explains the blanking.
 */
async function openLog() {
  const text = await run('读取日志', () => invoke('log_tail', { lines: 500 }));
  if (text === null) return;
  // The log view lives in the dialog's own template, so it is reached through the modal the dialog
  // hands over. A global document-wide lookup would search for an element that only exists while
  // this dialog is open — the gate caught exactly that (its "ids used in app.js must exist in
  // index.html" rule; dialog fields are reached by querySelector on the modal instead).
  openModal({
    title: '程序日志',
    bodyHtml: `
      <p class="about-note">
        最近 500 行。日志文件上限 8 MB，超出后自动轮转、保留 7 份（更早的会消失）。
        显示内容与写入时一致：邮箱、授权码等已脱敏，所以你会看到「[已脱敏]」这样的占位。
      </p>
      <pre id="log-view" class="diag-preview"></pre>`,
    confirmLabel: '关闭',
    onReady: (modal) => {
      const view = modal.querySelector('#log-view');
      view.textContent = text && text.trim() ? text : '（还没有任何日志）';
      // Tail first: the question "what just happened" is the one being asked.
      view.scrollTop = view.scrollHeight;
    },
  });
}

/* ---------------------------------------------------------------- storage & cleanup */
/**
 * The storage panel: what the cache costs, what a cleanup would do, and what the last one did.
 *
 * Two rules from `design/03` §5.4 shape this screen, and both are about consent rather than
 * mechanics:
 *
 *   1. **the dry run comes first.** The 开始清理 button stays disabled until a dry run has been
 *      shown, because the plan is what the user is agreeing to. This is the interface echo of the
 *      command taking a plan rather than a policy (round 94) — the same principle in two layers on
 *      purpose: the signature makes skipping hard, the button makes skipping visible.
 *   2. **nothing says what will not be touched.** Unread and flagged bodies are never cleaned, and
 *      the mail on the server is never touched at all. A user who does not know that reads every
 *      cleanup as a deletion.
 */
async function openStoragePanel() {
  const report = await run('读取存储占用', () => invoke('storage_report'));
  if (report === null) return;

  const helper = window.__easyEmailStorage;
  openModal({
    title: '存储与清理',
    bodyHtml: `
      <p class="about-note">
        本地缓存：<strong>${escapeHtml(helper.formatBytes(report.cache_bytes))}</strong>，
        其中已下载正文 ${escapeHtml(helper.formatBytes(report.bodies_bytes))}
        （${report.messages} 封摘要、${report.bodies} 封带正文）。
      </p>
      <label class="field">
        <span class="field-label">缓存上限（MB，只在超过时清理）</span>
        <input id="storage-limit" type="text" inputmode="numeric"
               value="${helper.DEFAULT_LIMIT_MB}" />
      </label>
      <div class="about-storage-actions">
        <button class="btn ghost small" id="btn-storage-dry" type="button">看看会清什么</button>
        <button class="btn primary small" id="btn-storage-run" type="button" disabled>开始清理</button>
      </div>
      <p id="storage-plan" class="about-note" hidden></p>
      <p class="about-note">
        清理只会删<strong>已下载正文的缓存</strong>，服务器上的邮件不受影响；
        <strong>未读的、有旗标的永远不会被清</strong>。清完仍可按主题/发件人搜索，
        正文要看时重新下载。
      </p>
      <div class="about-storage-history">
        <div class="about-storage-history-head">
          <span>最近清理</span>
          <button class="btn ghost small" id="btn-storage-clear-log" type="button">清空记录</button>
        </div>
        <p id="storage-history" class="about-note">（还没有清理过）</p>
      </div>`,
    confirmLabel: '关闭',
    onReady: async (modal) => {
      const plan = modal.querySelector('#storage-plan');
      const runButton = modal.querySelector('#btn-storage-run');
      const limitBox = modal.querySelector('#storage-limit');
      let currentPlan = null;

      modal.querySelector('#btn-storage-dry').addEventListener('click', async () => {
        const limitMb = window.__easyEmailStorage.suggestStorageLimit(limitBox.value);
        limitBox.value = String(limitMb);
        const policy = { max_bytes: limitMb * 1024 * 1024, max_age_days: null,
                         keep_unread: true, keep_flagged: true };
        const planned = await run('计算清理', () => invoke('cleanup_plan', { policy }));
        if (planned === null) return;
        currentPlan = planned;
        const summary = window.__easyEmailStorage.cleanupSummary(planned);
        plan.hidden = false;
        plan.textContent = summary.text;
        // The consent, in one control: nothing to run until something was shown.
        runButton.disabled = !summary.actionable;
      });

      runButton.addEventListener('click', async () => {
        if (!currentPlan) return;
        runButton.disabled = true;
        const policy = {
          max_bytes: window.__easyEmailStorage.suggestStorageLimit(limitBox.value) * 1024 * 1024,
          max_age_days: null,
          keep_unread: true,
          keep_flagged: true,
        };
        const done = await run('清理缓存', () =>
          invoke('cleanup_apply', { plan: currentPlan, policy })
        );
        if (done === null) {
          runButton.disabled = false;
          return;
        }
        toast(
          `已清理：释放 ${window.__easyEmailStorage.formatBytes(done.freed_bytes)}，` +
            `${done.bodies_deleted} 封正文`,
          'ok'
        );
        plan.textContent = `完成：释放 ${window.__easyEmailStorage.formatBytes(done.freed_bytes)}。`;
        currentPlan = null;
        runButton.disabled = true;
        await refreshStorageHistory(modal);
      });

      modal.querySelector('#btn-storage-clear-log').addEventListener('click', async () => {
        // The name says what it does not do: this is the record, not the cache.
        if (!confirm('清空清理记录？\n这只删记录，不动缓存。')) return;
        const ok = await run('清空清理记录', () => invoke('clear_cleanup_logs'));
        if (ok === null) return;
        await refreshStorageHistory(modal);
      });

      await refreshStorageHistory(modal);
    },
  });
}

async function refreshStorageHistory(modal) {
  const host = modal.querySelector('#storage-history');
  const log = await run('读取清理记录', () => invoke('cleanup_history'));
  if (log === null) return;
  if (log.length === 0) {
    host.textContent = '（还没有清理过）';
    return;
  }
  const helper = window.__easyEmailStorage;
  const lines = log
    .slice(0, 5)
    .map(
      (row) =>
        `${clockLabel(row.created_at)}：释放 ${helper.formatBytes(row.freed_bytes)}，` +
        `${row.bodies_deleted} 封正文`
    );
  host.textContent = lines.join('\n');
}

function openAbout() {
  const version = $('about-version').textContent || '未知';  openModal({
    title: '关于 Easy Email',
    bodyHtml: `
      <p class="about-line">当前版本 <strong>${escapeHtml(version)}</strong></p>
      <p class="about-note">
        有新版时这里会自动发现；更新包由 GitHub Releases 托管，
        带签名校验，安装前可以核对页面上的 SHA256SUMS。
      </p>
      <p id="about-update-status" class="about-note" hidden></p>
      <div class="about-diag">
        <button class="btn ghost small" id="btn-diag" type="button">生成诊断包（先预览）</button>
        <!--
          Sample message headers, off by default (design/16 §3.3: this needs an explicit tick).
          Headers only — the summaries carry no body, which is why the checkbox can say so without
          it being a promise.
        -->
        <label class="about-check">
          <input id="diag-headers" type="checkbox" />
          附带样本消息头（最近 8 封的收件箱摘要，只有头部、不含正文）
        </label>
        <p class="about-note">
          诊断包 = 最近 3 天的日志 + 账号列表，<strong>全部脱敏</strong>（邮箱保留域名、
          授权码/手机号/路径用户名等整项掩掉）。先在这里给你看内容，<strong>确认后才写盘</strong>。
        </p>
        <pre id="diag-preview" class="diag-preview" hidden></pre>
        <p id="diag-status" class="about-note" hidden></p>
      </div>`,
    confirmLabel: '检查更新',
    onReady: (modal) => {
      wireDiagnostics(modal);
    },
    onConfirm: async (modal) => {
      await checkForUpdate(modal);
      // Stays open either way: the answer (new version / up to date / could not reach the release
      // page) is shown in the dialog itself, and closing it would hide the answer to a question the
      // user just asked.
      return false;
    },
  });
}

/**
 * Ask the release page whether there is a newer build, and offer to install it.
 *
 * The whole flow lives in Rust because the renderer has no bundler: the updater plugin's JavaScript
 * API is not in this app's assets, so the progress numbers only exist inside the Rust closure
 * (`install_update`). What is left here is the question and the answer.
 *
 * A failed check is a sentence, not an error: the release page may simply not have a `latest.json`
 * yet (every release before this feature shipped), and the network may be unreachable. Neither is
 * something the user did wrong, so neither gets a red banner.
 */
async function checkForUpdate(modal) {
  const status = modal.querySelector('#about-update-status');
  const button = modal.querySelector('[data-role="confirm"]');
  if (!status || !button) return;

  status.hidden = false;
  status.className = 'about-note';
  status.textContent = '正在检查更新…';
  button.disabled = true;

  const update = await invoke('check_update').catch((error) => {
    status.textContent = `检查更新失败：${errText(error)}`;
    return undefined;
  });

  // `undefined` covers both "up to date" and "could not reach the release page": the command maps
  // the network failure onto `None` on purpose, because neither is something the user can act on
  // and both would otherwise need a different red banner.
  if (!update) {
    status.textContent = '已经是最新版本。';
    button.disabled = false;
    button.textContent = '打开下载页';
    button.onclick = async () => {
      await run('打开下载页', () => invoke('open_download_page'));
    };
    return;
  }

  const notes = update.notes ? `\n\n${update.notes}` : '';
  status.textContent = `发现新版本 ${update.version}（当前 ${update.current_version}）。下载并安装后会重启应用。${notes}`;
  button.disabled = false;
  button.textContent = `安装 ${update.version}`;

  button.onclick = async () => {
    button.disabled = true;
    status.textContent = `正在下载 ${update.version}…`;
    // Progress arrives as an event: the numbers are produced inside the Rust download closure.
    const stop = await listen('update:progress', (event) => {
      const done = event.payload?.downloaded ?? 0;
      const total = event.payload?.total;
      status.textContent = total
        ? `正在下载 ${update.version}… ${Math.round((done / total) * 100)}%`
        : `正在下载 ${update.version}…`;
    });
    try {
      await run('安装更新', () => invoke('install_update'));
      status.textContent = '更新已安装，应用正在重启。';
    } catch (error) {
      status.textContent = `安装失败，已保留当前版本：${errText(error)}`;
      stop();
      button.disabled = false;
      button.textContent = '重试安装';
    }
  };
}

/**
 * Report what the last update attempt actually did.
 *
 * The note `install_update` leaves behind is read once, at startup: same version means the update
 * landed (`design/17` §5's first sentence), a different one means the installer did not take and the
 * user deserves to hear that instead of pressing restart forever.
 */
async function reportUpdateOutcome() {
  const outcome = await invoke('take_update_outcome').catch(() => null);
  if (!outcome) return;
  const text =
    outcome.outcome === 'updated'
      ? `已更新到 ${outcome.version}。`
      : `上次的更新没有生效（尝试装到 ${outcome.expected}，实际仍在 ${outcome.running}）。当前版本可以正常使用。`;
  toast(text, outcome.outcome === 'updated' ? 'info' : 'err', 15000);
}

function openAccountWizard() {
  pendingWizardAccountId = null;

  const providerOptions = state.providers
    .map(
      (provider) =>
        `<option value="${escapeHtml(provider.id)}">${escapeHtml(provider.display_name)}${
          provider.supports_imap ? '' : '（不支持 IMAP）'
        }</option>`
    )
    .join('');

  const body = `
    <div class="field">
      <label>邮箱地址</label>
      <input id="wiz-email" placeholder="you@example.com" autocomplete="off" spellcheck="false" />
      <span class="hint">输入后会自动识别服务商，服务器地址和端口不用你查。</span>
    </div>
    <div class="grid-2">
      <div class="field">
        <label>服务商</label>
        <select id="wiz-provider">${providerOptions}</select>
      </div>
      <div class="field">
        <label>显示名称（可选）</label>
        <input id="wiz-name" placeholder="张三" autocomplete="off" spellcheck="false" />
      </div>
    </div>
    <!--
      OAuth sign-in, shown only for the providers that require it (design/05 §3.1). The client id is
      a field rather than a constant because M0-8 is still open: whether Google accepts a desktop
      client without a secret has to be answered against the real endpoint, and until then the honest
      shape is "bring your own registration" — which is also what the design's BYO wizard anticipates.
    -->
    <div id="wiz-detected" class="detected" hidden></div>
    <div id="wiz-note" class="callout" hidden></div>

    <!--
      Two ways in, and they are a *choice* rather than two stacks of fields in one form: OAuth sends
      the user to a browser and never sees a password, while a password account never sees a browser.
      Showing both at once made it unclear what had to be filled in — and the client-id boxes that
      belong to the first path looked like something the user had to understand (reported by a real
      user, M1a-66).
    -->
    <fieldset id="wiz-auth-choice" class="auth-choice" hidden>
      <legend>登录方式</legend>
      <label class="choice">
        <input type="radio" name="wiz-auth" value="oauth" />
        <span>
          <strong>用浏览器登录</strong>
          <em>不用输入密码；授权可以随时在服务商那边撤销。</em>
        </span>
      </label>
      <label class="choice">
        <input type="radio" name="wiz-auth" value="password" />
        <span>
          <strong>用密码 / 授权码</strong>
          <em>国内邮箱和自建服务器通常走这条。</em>
        </span>
      </label>
    </fieldset>

    <div id="wiz-oauth-path" hidden>
      <p class="hint">
        点下面的按钮会<b>自动打开你的默认浏览器</b>；在浏览器里点「允许」之后回到这里就会自动完成，
        不需要你自己去开浏览器。
      </p>
      <button id="wiz-oauth-go" class="btn primary" type="button">打开浏览器登录</button>
      <div id="wiz-oauth-status" class="oauth-status" hidden></div>

      <details id="wiz-oauth-advanced" class="advanced">
        <summary>高级：使用你自己的 OAuth 客户端 ID</summary>
        <p class="hint">
          已经为你填好了默认值，通常不用改。如果你有自己的 Google Cloud 注册，把下载的 JSON
          放进应用数据目录即可（会优先使用它），或直接在这里替换。
        </p>
        <input id="wiz-client-id" placeholder="client_id" autocomplete="off" spellcheck="false" />
        <input id="wiz-client-secret" placeholder="client_secret（服务商要求时才有）" autocomplete="off" spellcheck="false" />
        <span id="wiz-oauth-notice" class="hint"></span>
      </details>
    </div>

    <div id="wiz-password-path">
      <div class="field">
        <label>登录用户名（可选）</label>
        <input id="wiz-username" placeholder="默认与邮箱地址相同" autocomplete="off" spellcheck="false" />
        <span class="hint">只有登录名和邮箱地址不一样时才需要填。典型情况：@foxmail.com 别名
          需要改用 QQ 号邮箱登录，或者企业/VIP 账号下发了单独的登录名。</span>
      </div>
      <div class="field">
        <label>密码 / 授权码 / 应用专用密码</label>
        <!--
          new-password rather than off: browsers ignore "off" for password fields and would
          otherwise offer to fill a saved website password into an email account field.
        -->
        <input id="wiz-password" type="password" autocomplete="new-password" spellcheck="false" />
        <span class="hint">国内邮箱几乎都需要「授权码」，不是登录密码 —— 见上方说明。</span>
      </div>
    </div>

    <!--
      Only for a provider whose servers are the user's to fill in (the manual_endpoints flag from the
      provider table — never an id comparison here). The defaults come from the backend, not from this
      file. Avoid interpolation syntax in this comment: it lives inside a template literal.
    -->
    <div id="wiz-endpoints" hidden>
      <p class="hint">这个服务商的服务器地址不内置，需要你按服务商文档填写。</p>
      <!--
        A receive-only mail store is not a broken account. Refusing to save one because its SMTP leg
        could not be probed is the "must be complete" protection misfiring, and the only way out is
        for the user to say so out loud: with this unchecked, a mistyped host still produces no
        account (design/20-v0.2-plan.md, C8). No backticks here: this is inside a template literal.
      -->
      <label class="check">
        <input id="wiz-smtp-none" type="checkbox" />
        <span>这个服务器不提供 SMTP（只收信，不发信）</span>
      </label>
      <div class="grid-2">
        <div class="field">
          <label>IMAP 主机</label>
          <input id="wiz-imap-host" placeholder="imap.example.com" autocomplete="off" spellcheck="false" />
        </div>
        <div class="field">
          <label>IMAP 端口</label>
          <input id="wiz-imap-port" type="number" min="1" max="65535" />
        </div>
      </div>
      <div class="field">
        <label>IMAP 加密</label>
        <select id="wiz-imap-security"></select>
      </div>
      <div class="grid-2">
        <div class="field">
          <label>SMTP 主机</label>
          <input id="wiz-smtp-host" placeholder="smtp.example.com" autocomplete="off" spellcheck="false" />
        </div>
        <div class="field">
          <label>SMTP 端口</label>
          <input id="wiz-smtp-port" type="number" min="1" max="65535" />
        </div>
      </div>
      <div class="field">
        <label>SMTP 加密</label>
        <select id="wiz-smtp-security"></select>
      </div>
      <div id="wiz-endpoint-note" class="hint"></div>
    </div>
    <div id="wiz-report"></div>`;

  openModal({
    title: '添加邮箱账号',
    bodyHtml: body,
    confirmLabel: '验证并保存',
    onConfirm: async (modal) => {
      const email = modal.querySelector('#wiz-email').value.trim();
      const providerId = modal.querySelector('#wiz-provider').value;
      const displayName = modal.querySelector('#wiz-name').value.trim();
      const username = modal.querySelector('#wiz-username').value.trim();
      const password = modal.querySelector('#wiz-password').value;
      const chosenProvider = state.providers.find((p) => p.id === providerId);
      // Only a provider whose servers are the user's to fill in carries endpoints; everything else
      // sends `null` and the backend builds the account from the table. `endpointOverride` keeps the
      // wire names in one place (src/payload.js), which is what the contract gate compares against
      // the Rust struct. There is deliberately **no** validation here: the rules live in Rust
      // (`normalise_host` / `validate_endpoint`), and a second copy is a second place to be wrong.
      const endpoints =
        chosenProvider?.manual_endpoints === true
          ? window.__easyEmailPayload.endpointOverride({
              imapHost: modal.querySelector('#wiz-imap-host').value,
              imapPort: Number(modal.querySelector('#wiz-imap-port').value),
              imapSecurity: modal.querySelector('#wiz-imap-security').value,
              smtpHost: modal.querySelector('#wiz-smtp-host').value,
              smtpPort: Number(modal.querySelector('#wiz-smtp-port').value),
              smtpSecurity: modal.querySelector('#wiz-smtp-security').value,
            })
          : null;

      if (!email.includes('@')) {
        toast('请填写完整的邮箱地址。', 'err');
        return false;
      }
      // A password is required for the accounts that use one. For an OAuth2 provider the credential
      // is the token the browser flow already stored, so demanding a password here would block the
      // one path that cannot have one (design/05 §3.2).
      // The OAuth path is the one that was *chosen* and is actually available: `oauth_available` comes
      // from the provider table, so a service that has no OAuth path can never end up requiring one.
      const oauthAccount =
        modal.querySelector('#wiz-auth-choice').dataset.oauthAvailable === '1' &&
        modal.querySelector('input[name="wiz-auth"]:checked')?.value === 'oauth';
      // The authorization flag lives on the dialog element, not in a module variable: a module
      // variable outlives the dialog, so opening the wizard a second time would already look
      // "authorized" and let the user save an OAuth account that was never authorized.
      if (oauthAccount && modal.dataset.oauthAuthorized !== '1') {
        toast('请先点上面的「打开浏览器登录」完成授权。', 'err', 9000);
        return false;
      }
      if (!password && !oauthAccount) {
        toast('请填写密码或授权码。', 'err');
        return false;
      }

      const report = modal.querySelector('#wiz-report');
      report.innerHTML = '<div class="report"><span class="spinner"></span>正在用真实连接验证…</div>';

      // Saving first is what puts the credential in the vault, which verification needs. If it
      // then fails, keep the account and password in place: retrying should be one click, not a
      // fresh retype. That matters most for QQ, which rate-limits after repeated failed logins —
      // making the user re-enter everything tempts them into retrying faster, not slower.
      // If they give up and close the dialog, `closeModal` cleans up.
      const accountId = email.toLowerCase();
      let verified = false;
      // Read outside the `try` because the decision below needs it: "the server refused" and "the
      // server said nothing" both leave `verified` false and lead to opposite conclusions.
      let result = null;

      try {
        await invoke('save_account', {
          providerId,
          email,
          displayName,
          password,
          // null lets the backend fall back to the address.
          username: username || null,
          // null for every provider whose servers come from the table.
          endpoints,
          // The one case where an account is allowed to exist without a working SMTP leg; false
          // for everything else, which keeps the "must be complete" rule intact.
          smtpNone: modal.querySelector('#wiz-smtp-none')?.checked === true,
        });

        result = await invoke('verify_account', { accountId });
        renderReport(report, result);
        verified = result.imap_ok && result.smtp_ok;
      } catch (error) {
        report.innerHTML = `<div class="report bad">${escapeHtml(errText(error))}</div>`;
      }

      // What a failed probe means is decided by a pure function with a test beside it
      // (`src/wizard.js`, `tools/wizard_verdict_test.mjs`), because this spot collected three bugs
      // one at a time and the order of the checks *is* the decision. The cases:
      //
      // - a **rejection** is not the same failure as silence, and for OAuth it is the one case that
      //   must not be saved: the credential was refused, so the account can never work, and every
      //   later message about it ("网络恢复后会自动开始收信") would be false. Nothing used to compare
      //   the address typed here with the account the browser authorized.
      // - an OAuth account that was authorized is **kept** when the probe simply got no answer: the
      //   authorization is a real, stored, revocable thing the user just did in a browser, while
      //   "IMAP did not answer" is often just the network (measured: Gmail's 993 answers about two
      //   times in five from this network). Throwing the account away would make them redo the
      //   browser dance for nothing.
      // - IMAP answering while **SMTP** refuses is the Microsoft 365 shape (a tenant with SMTP AUTH
      //   switched off): receiving already works, so "网络恢复后" is the wrong sentence, and the
      //   account is worth keeping while the user is told an administrator has to change a setting.
      const verdict = window.__easyEmailWizard.verificationVerdict({
        oauthAuthorized: Boolean(oauthAccount) && modal.dataset.oauthAuthorized === '1',
        imapOk: result?.imap_ok === true,
        smtpOk: result?.smtp_ok === true,
        imapAuthRejected: result?.imap_auth_rejected === true,
        smtpAuthRejected: result?.smtp_auth_rejected === true,
        smtpNone: modal.querySelector('#wiz-smtp-none')?.checked === true,
      });

      if (verdict === 'accept-receive-only') {
        pendingWizardAccountId = null;
        // Two roads to the same verdict, and they are not the same sentence: one is the user telling
        // us there is nothing to send to, the other is a server refusing to let us.
        const because =
          modal.querySelector('#wiz-smtp-none')?.checked === true
            ? '你说过这个服务器不提供 SMTP，因此只能收信。要发信需要另配一个账号。'
            : `可以收信，但服务器拒绝了发信 —— ${result.smtp_detail}`;
        toast(`已保存 ${email}：${because}`, 'info', 20000);
        await refreshAccounts();
        return true;
      }

      if (!verified) {
        if (verdict === 'reject-address') {
          pendingWizardAccountId = accountId;
          report.innerHTML =
            '<div class="report bad"><strong>服务器拒绝了这次登录</strong><br>' +
            '浏览器授权成功了，但邮件服务器明确拒绝了这些凭据。最可能的原因是：' +
            '<b>你在浏览器里授权的账号，和上面填的地址不是同一个</b> —— ' +
            '授权是由谁完成的只有浏览器知道，应用看不到。' +
            '请把地址改成你刚才授权的那个，再点一次「打开浏览器登录」。<br>' +
            '其次的可能：该账号已被停用，或授权时没有勾选读取邮件的权限。</div>';
          const retry = modal.querySelector('[data-role="confirm"]');
          if (retry) retry.textContent = '重试';
          return false;
        }

        if (verdict === 'accept-offline') {
          pendingWizardAccountId = null;
          toast(
            `已保存 ${email}：浏览器授权成功，但这次没连上服务器。网络恢复后会自动开始收信。`,
            'info',
            15000
          );
          await refreshAccounts();
          return true;
        }

        // 'retry': a password account, where a failure is usually a wrong authorization code.
        pendingWizardAccountId = accountId;
        const retry = modal.querySelector('[data-role="confirm"]');
        if (retry) retry.textContent = '重试';
        return false;
      }

      pendingWizardAccountId = null;
      toast(`已添加 ${email}`, 'ok');
      await refreshAccounts();
      return true;
    },
  });

  // Live provider detection as the address is typed.
  const modal = $('modal-root').querySelector('.modal');
  const emailInput = modal.querySelector('#wiz-email');
  const usernameInput = modal.querySelector('#wiz-username');
  const providerSelect = modal.querySelector('#wiz-provider');

  // The manual-endpoint block and its fields. Resolved once, here, because `updateDetected` runs on
  // every keystroke and a query per field per keystroke is pure waste.
  const endpointsBlock = modal.querySelector('#wiz-endpoints');
  const imapHostInput = modal.querySelector('#wiz-imap-host');
  const imapPortInput = modal.querySelector('#wiz-imap-port');
  const imapSecuritySelect = modal.querySelector('#wiz-imap-security');
  const smtpHostInput = modal.querySelector('#wiz-smtp-host');
  const smtpPortInput = modal.querySelector('#wiz-smtp-port');
  const smtpSecuritySelect = modal.querySelector('#wiz-smtp-security');
  const endpointNote = modal.querySelector('#wiz-endpoint-note');

  /**
   * Fill the manual endpoint block for a provider whose servers are the user's to supply.
   *
   * The one-time part (the security choices and the defaults) comes from `endpoint_choices`, which
   * reads `Security::ALL` and the `custom` row of the provider table — the wire spells of the
   * security values are serde's, and a copy of them here would keep looking right after a rename.
   *
   * The per-address part only touches a host box the user has not edited: guessing `mail.example.com`
   * is helpful while the address is being typed and destructive a second later.
   */
  const fillEndpointForm = async (provider, email) => {
    if (endpointsBlock.dataset.ready !== '1') {
      endpointsBlock.dataset.ready = '1';
      try {
        const choices = await invoke('endpoint_choices');
        for (const select of [imapSecuritySelect, smtpSecuritySelect]) {
          select.innerHTML = '';
          for (const choice of choices.securities) {
            const option = document.createElement('option');
            option.value = choice.value;
            option.textContent = `${choice.label} —— ${choice.hint}`;
            select.appendChild(option);
          }
        }
        imapPortInput.value = String(choices.default_imap_port);
        imapSecuritySelect.value = choices.default_imap_security;
        smtpPortInput.value = String(choices.default_smtp_port);
        smtpSecuritySelect.value = choices.default_smtp_security;
      } catch (error) {
        // Falling back to an empty dropdown is honest: the user can still type the ports, and the
        // error is right under the fields rather than in a toast they might miss.
        endpointNote.textContent = `无法读取加密方式选项：${errText(error)}`;
      }
    }

    const domain = email.includes('@') ? email.split('@')[1] : '';
    for (const input of [imapHostInput, smtpHostInput]) {
      if (input.dataset.touched === '1') continue;
      input.value = domain ? `mail.${domain}` : '';
    }
    endpointNote.textContent = domain
      ? `已按域名预填 mail.${domain}，请按服务商文档核对或改写。`
      : '填好邮箱地址后这里会自动预填一个猜测值，可以随时改。';
  };

  const updateDetected = async () => {
    const email = emailInput.value.trim();
    const provider = state.providers.find((p) => p.id === providerSelect.value);
    if (!provider) return;

    // Show what will actually be sent as the login name, so an override is never invisible.
    const effectiveLogin = usernameInput.value.trim() || email || '(未填写)';

    const detected = modal.querySelector('#wiz-detected');
    const note = modal.querySelector('#wiz-note');

    // Two paths, chosen explicitly. Shown only where there is a real choice: a QQ account has one way
    // in, and offering a radio pair with a single usable option is noise. Gmail has both (app password
    // or OAuth), Microsoft only OAuth.
    const choice = modal.querySelector('#wiz-auth-choice');
    const oauthPath = modal.querySelector('#wiz-oauth-path');
    const passwordPath = modal.querySelector('#wiz-password-path');
    // From the provider table (`oauth_available`), never from a provider id listed here: an id list in
    // the renderer is what `design/05` §10.1 exists to prevent, and Gmail's "app password *or* OAuth"
    // is exactly the kind of exception that belongs next to the table. The answer arrives from the
    // backend with the registration, so for a moment nothing is known and the path stays hidden.
    const oauthEligible = () => choice.dataset.oauthAvailable === '1';
    const passwordEligible = provider.auth !== 'oauth2';

    const selected = () =>
      modal.querySelector('input[name="wiz-auth"]:checked')?.value ??
      (oauthEligible() && !passwordEligible ? 'oauth' : 'password');

    const applyPath = () => {
      const path = selected();
      choice.hidden = !(oauthEligible() && passwordEligible);
      oauthPath.hidden = path !== 'oauth' || !oauthEligible();
      passwordPath.hidden = path === 'oauth' && oauthEligible();
      const confirm = modal.querySelector('[data-role="confirm"]');
      if (confirm && !confirm.disabled) {
        // The button at the bottom means different things on the two paths, and saying which is the
        // difference between "what do I know now?" and a finished setup.
        confirm.textContent =
          path === 'oauth' && oauthEligible() && modal.dataset.oauthAuthorized !== '1'
            ? '先完成上面的浏览器登录'
            : '验证并保存';
      }
    };

    if (oauthEligible() && !choice.dataset.wired) {
      choice.dataset.wired = '1';
      choice.querySelectorAll('input[name="wiz-auth"]').forEach((radio) =>
        radio.addEventListener('change', applyPath)
      );
      // OAuth is the recommended path where it exists: no password typed, revocable authorization.
      choice.querySelector('input[value="oauth"]').checked = true;
    }
    applyPath();

    // The manual endpoint block: shown from the provider table's flag, never from an id here.
    if (provider.manual_endpoints === true) {
      endpointsBlock.hidden = false;
      await fillEndpointForm(provider, email);
    } else {
      endpointsBlock.hidden = true;
    }

    {
      // Asked for **every** provider, not only the ones already known to offer OAuth: the answer is
      // what tells us that (the provider table is speaking), so gating the question on the answer
      // would be a deadlock — which is exactly the state this file was in for one commit.
      //
      // The backend prefers the registration the user dropped into the app data directory and falls
      // back to the borrowed Thunderbird one with its notice. Asking it keeps one source of truth: a
      // value hard-coded here could silently disagree with what `oauth_sign_in` is configured to use.
      invoke('oauth_client_default', { providerId: provider.id })
        .then((client) => {
          if (client?.oauth_available) {
            choice.dataset.oauthAvailable = '1';
            // The answer can arrive after the provider was chosen, so the layout is re-applied here
            // rather than only where it was first computed.
            if (!choice.dataset.wired) {
              choice.dataset.wired = '1';
              choice.querySelectorAll('input[name="wiz-auth"]').forEach((radio) =>
                radio.addEventListener('change', applyPath)
              );
              choice.querySelector('input[value="oauth"]').checked = true;
            }
            applyPath();
          }
          const clientIdField = modal.querySelector('#wiz-client-id');
          const clientSecretField = modal.querySelector('#wiz-client-secret');
          if (client && client.client_id && !clientIdField.value) {
            clientIdField.value = client.client_id;
            clientSecretField.value = client.client_secret ?? '';
          }
          // Where the pre-filled value came from is the one thing the user cannot check for
          // themselves: both sources look identical in the two fields. `own` means the JSON they
          // dropped into the app data directory is already the one in use, so the honest answer to
          // "do I have to do anything here?" is visible instead of implied.
          const notice = modal.querySelector('#wiz-oauth-notice');
          notice.textContent =
            client?.source === 'own'
              ? '正在使用你自己放进应用数据目录的注册（oauth-client.json）—— 这里不用改。'
              : client?.notice ?? '';
        })
        .catch(() => {});
    }
    // Attached on the first pass, **without** asking whether this provider has an OAuth path.
    //
    // That condition used to be part of the guard, and it is an *asynchronous* answer: it only
    // becomes true when `oauth_client_default` comes back. When the last thing the user did was the
    // typing/choosing that started that request, no further `updateDetected()` ran, so the path was
    // revealed by the `.then()` while this listener was never attached — a visible button wired to
    // nothing. A real user reported exactly that: "点击打开浏览器登录没反应".
    //
    // The provider is therefore resolved at *click* time rather than captured here, because the
    // closure that attaches first is the one that survives, and it would otherwise hold whatever
    // provider was selected before the address was typed.
    const go = modal.querySelector('#wiz-oauth-go');
    if (!go.dataset.wired) {
      go.dataset.wired = '1';
      go.addEventListener('click', async () => {
        const address = emailInput.value.trim();
        if (!address) {
          toast('先填邮箱地址', 'err');
          return;
        }
        const chosen = state.providers.find((p) => p.id === providerSelect.value);
        if (!chosen) return;
        if (!oauthEligible()) {
          toast('这个服务商不走浏览器登录，请用密码 / 授权码。', 'err', 9000);
          return;
        }
        const clientId = modal.querySelector('#wiz-client-id').value.trim();
        if (!clientId) {
          toast('这类邮箱需要一个 OAuth 客户端 ID（见「高级」里的说明）', 'err', 9000);
          return;
        }
        const secret = modal.querySelector('#wiz-client-secret').value.trim();
        const status = modal.querySelector('#wiz-oauth-status');
        status.hidden = false;
        status.className = 'oauth-status';
        status.innerHTML = '<span class="spinner"></span>已打开浏览器，请在那里点「允许」（两分钟内）…';

        const tokens = await (async () => {
          try {
            return await invoke('oauth_sign_in', {
              email: address,
              providerId: chosen.id,
              clientId,
              clientSecret: secret || null,
            });
          } catch (error) {
            // In the status box *and* as a toast: this is where the user is looking, and some of these
            // messages ("浏览器里授权的账号是 X，但你填的是 Y") are ones they have to read rather than
            // catch out of the corner of an eye before a toast fades.
            const reason = errText(error);
            status.className = 'oauth-status bad';
            status.textContent = reason;
            toast(`OAuth 登录：${reason}`, 'err', 20000);
            return null;
          }
        })();
        if (!tokens) {
          if (!status.textContent) {
            status.className = 'oauth-status bad';
            status.textContent = '没有完成授权。可以再点一次「打开浏览器登录」。';
          }
          return;
        }

        modal.dataset.oauthAuthorized = '1';
        status.className = 'oauth-status ok';
        status.textContent = tokens.refresh_token
          ? '✓ 已授权，授权已保存在本机。现在点下面的「验证并保存」即可。'
          : '✓ 已登录，但服务商没有返回刷新令牌 —— 下次启动可能需要重新登录。';
        applyPath();
      });
    }

    detected.hidden = false;
    // A manual provider has no hosts to show — the table's blanks are deliberate (see `custom`), and
    // rendering them would put ":993" on screen as if it were an answer.
    const endpointCell = (host, port, security) =>
      provider.manual_endpoints
        ? '<span class="muted">由你在下方填写</span>'
        : `<span class="mono">${escapeHtml(host)}:${port} (${security})</span>`;
    detected.innerHTML = `
      <div><span class="k">IMAP</span>${endpointCell(
        provider.imap_host,
        provider.imap_port,
        provider.imap_security
      )}</div>
      <div><span class="k">SMTP</span>${endpointCell(
        provider.smtp_host,
        provider.smtp_port,
        provider.smtp_security
      )}</div>
      <div><span class="k">认证</span>${
        provider.auth === 'oauth2'
          ? '<strong>需要 OAuth2</strong>（用下面的按钮登录，或填应用专用密码）'
          : provider.auth === 'app_password'
            ? '需要授权码 / 应用专用密码'
            : '邮箱登录密码'
      }${provider.quirks.needs_id ? '　·　该服务商要求发送 IMAP ID 命令（已内置处理）' : ''}</div>
      <div><span class="k">状态</span>${
        provider.manual_endpoints
          ? '由你填写（不适用内置地址验证）'
          : provider.verified
            ? '✅ 服务器地址已实测可用'
            : '⚠️ 尚未实测验证'
      }</div>
      <div><span class="k">登录名</span><span class="mono">${escapeHtml(effectiveLogin)}</span>${
        effectiveLogin.toLowerCase() === email.toLowerCase()
          ? ''
          : '　<span class="muted">（与邮箱地址不同）</span>'
      }</div>`;

    const warnings = [provider.setup_note];
    if (!provider.supports_imap) {
      warnings.push('这个服务商不提供 IMAP，无法用于桌面客户端。');
    }
    if (provider.auth === 'oauth2') {
      warnings.push('该服务商已停用基本认证，当前版本无法登录，请见后续计划。');
    }
    if (provider.id === 'gmail') {
      warnings.push('提示：从中国大陆的网络通常无法直连 Gmail 服务器。');
    }
    // Verified behaviour: the alias shares the mailbox but only the primary QQ address can log
    // in over IMAP. Say so up front rather than letting the user hit "Login fail" first.
    if (/@(foxmail\.com|vip\.qq\.com)$/i.test(email) && !usernameInput.value.trim()) {
      warnings.push(
        '⚠️ 这是 QQ 邮箱的别名地址，和 QQ 号是同一个邮箱，但 IMAP 登录只认 QQ 号邮箱。\n' +
          '请在「登录用户名」里填你的 QQ 号邮箱（例如 123456@qq.com），邮箱地址保持别名不变。'
      );
    }
    note.hidden = false;
    note.className = provider.supports_imap && provider.auth !== 'oauth2' ? 'callout' : 'callout warn';
    note.textContent = warnings.join('\n\n');

    // Offer a sensible default when the address matches a known provider.
    void email;
  };

  emailInput.addEventListener('input', () => {
    const email = emailInput.value.trim();
    const guessed = state.providers.find(
      (p) => email.includes('@') && p.domains.some((d) => email.toLowerCase().endsWith(`@${d}`))
    );
    if (guessed) {
      providerSelect.value = guessed.id;
    }
    updateDetected();
  });
  providerSelect.addEventListener('change', updateDetected);
  usernameInput.addEventListener('input', updateDetected);
  // "Has the user edited this?" is marked once, here, and never inside `updateDetected` — a listener
  // attached there would be attached again on every keystroke, and the N-th one would win.
  [imapHostInput, smtpHostInput].forEach((input) => {
    input.addEventListener('input', () => {
      input.dataset.touched = '1';
    });
  });
  updateDetected();
  // The detected box and the setup note are added above the password field, so the body grew
  // after the dialog opened. Put the scroll position back at the top.
  scrollModalToTop(modal);
}

function renderReport(container, report) {
  const ok = report.imap_ok && report.smtp_ok;
  container.innerHTML = `
    <div class="report ${report.imap_ok ? 'ok' : 'bad'}"><strong>IMAP</strong>\n${escapeHtml(
      report.imap_detail
    )}</div>
    <div class="report ${report.smtp_ok ? 'ok' : 'bad'}" style="margin-top:8px"><strong>SMTP</strong>\n${escapeHtml(
      report.smtp_detail
    )}</div>`;
  if (ok) {
    toast(`验证通过，发现 ${report.mailboxes.length} 个文件夹`, 'ok');
  }
}

/* ---------------------------------------------------------------- modal plumbing */

/**
 * Handler to run when the open dialog is dismissed.
 *
 * Module level because exactly one dialog can exist at a time, and the handler has to be able to
 * read the dialog's fields — so `closeModal` calls it *before* it tears the DOM down.
 */
let modalOnClose = null;

function openModal({ title, bodyHtml, confirmLabel, onConfirm, onReady, onClose = null }) {
  const root = $('modal-root');
  modalOnClose = onClose ? () => onClose(root.querySelector('.modal')) : null;
  root.hidden = false;
  root.innerHTML = `
    <div class="modal">
      <div class="modal-head">
        <h3>${escapeHtml(title)}</h3>
        <button class="btn ghost" data-role="close">✕</button>
      </div>
      <div class="modal-body">${bodyHtml}</div>
      <div class="modal-foot">
        <button class="btn ghost" data-role="close">取消</button>
        <button class="btn primary" data-role="confirm">${escapeHtml(confirmLabel)}</button>
      </div>
    </div>`;

  const modal = root.querySelector('.modal');
  const confirmButton = modal.querySelector('[data-role="confirm"]');

  // Let a caller wire up its own fields. Done here rather than by querying the document afterwards,
  // so nothing has to guess whether the modal is in the DOM yet.
  if (onReady) onReady(modal);

  root.querySelectorAll('[data-role="close"]').forEach((button) => {
    button.addEventListener('click', closeModal);
  });

  confirmButton.addEventListener('click', async () => {
    confirmButton.disabled = true;
    const previous = confirmButton.textContent;
    confirmButton.innerHTML = '<span class="spinner"></span>处理中…';
    try {
      const shouldClose = await onConfirm(modal);
      if (shouldClose) closeModal();
    } finally {
      // The modal may already be gone (closeModal replaced the DOM).
      if (document.body.contains(confirmButton)) {
        confirmButton.disabled = false;
        confirmButton.textContent = previous;
      }
    }
  });

  const firstInput = modal.querySelector('input, textarea');
  if (firstInput) firstInput.focus();

  // Focusing a field can scroll the dialog body, which clips the first field. Reset it here and
  // again after any dynamic content is filled in (see openAccountWizard).
  scrollModalToTop(modal);
}

/** Reset a dialog's scroll position once layout has settled. */
function scrollModalToTop(modal) {
  const body = modal.querySelector('.modal-body');
  if (!body) return;
  body.scrollTop = 0;
  requestAnimationFrame(() => {
    body.scrollTop = 0;
  });
}

function closeModal() {
  // The user gave up on a wizard attempt that saved a credential but never verified. Remove the
  // account so a bad authorization code cannot leave a permanently broken entry behind. The
  // dialog is already being dismissed, so this must not block or throw.
  if (pendingWizardAccountId) {
    const orphan = pendingWizardAccountId;
    pendingWizardAccountId = null;
    invoke('delete_account', { accountId: orphan }).catch(() => {});
  }

  const root = $('modal-root');
  const handler = modalOnClose;
  modalOnClose = null;

  // Before the DOM goes away: the composer's handler still has to read the fields it is saving.
  if (handler) {
    try {
      handler();
    } catch (error) {
      // A dismissed dialog must not take the window down with it.
      console.error(error);
    }
  }

  root.hidden = true;
  root.innerHTML = '';
}

/* ---------------------------------------------------------------- start */

boot().catch((error) => toast(`启动失败：${errText(error)}`, 'err', 15000));
