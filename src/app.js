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
  /** Decoded mailbox name for display; `mailbox` stays in wire form for commands. */
  mailboxLabel: null,
  messages: [],
  limit: PAGE_SIZE,
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
  detail: null,
  bodyMode: 'html',
  /** Per-message opt-in; reset every time a different message is opened. */
  allowRemoteImages: false,
  /** Non-empty means the list is showing cache hits rather than the current folder. */
  searchQuery: '',
  searchHits: [],
};

/* ---------------------------------------------------------------- utilities */

const $ = (id) => document.getElementById(id);

function escapeHtml(value) {
  return String(value ?? '')
    .replace(/&/g, '&amp;')
    .replace(/</g, '&lt;')
    .replace(/>/g, '&gt;')
    .replace(/"/g, '&quot;');
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
  state.providers = (await run('读取内置邮箱配置', () => invoke('list_providers'))) ?? [];
  await refreshAccounts();
  wireGlobalEvents();
  syncThemeButton();
  await wirePushEvents();
  // One timer for the whole window, rather than one per open composer or per outbox row: it is a
  // text update on whatever countdown is on screen, and it does nothing when none is.
  setInterval(updateUndoCountdowns, 1000);
  if (state.accounts.length === 0) {
    openAccountWizard();
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

function wireGlobalEvents() {
  $('btn-theme').addEventListener('click', () => {
    window.__easyEmailTheme.toggle();
    syncThemeButton();
  });
  $('btn-add-account').addEventListener('click', () => openAccountWizard());
  $('btn-refresh').addEventListener('click', () => {
    // Refresh means "re-read what this pane is showing": for a local list that is `user.db`, not the
    // server, so it must not silently jump back to the folder.
    if (state.view === 'drafts') showDrafts();
    else if (state.view === 'outbox') showOutbox();
    else loadMessages({ reset: true });
  });
  $('btn-drafts').addEventListener('click', showDrafts);
  $('btn-outbox').addEventListener('click', showOutbox);
  $('btn-load-more').addEventListener('click', loadOlder);
  $('btn-compose').addEventListener('click', () => openComposer({}));
  $('btn-reply').addEventListener('click', () => openComposer({ mode: 'reply' }));
  $('btn-forward').addEventListener('click', () => openComposer({ mode: 'forward' }));
  $('search-input').addEventListener('input', scheduleSearch);
  $('search-input').addEventListener('keydown', (event) => {
    if (event.key !== 'Escape') return;
    $('search-input').value = '';
    exitSearchMode();
  });
  $('btn-search-clear').addEventListener('click', () => {
    $('search-input').value = '';
    $('search-input').focus();
    exitSearchMode();
  });
  $('btn-delete').addEventListener('click', deleteSelected);
  $('btn-toggle-read').addEventListener('click', toggleRead);
  $('tab-html').addEventListener('click', () => setBodyMode('html'));
  $('tab-text').addEventListener('click', () => setBodyMode('text'));
  $('btn-show-remote').addEventListener('click', () => {
    state.allowRemoteImages = true;
    applyBodyMode();
  });
  $('modal-root').addEventListener('mousedown', (event) => {
    if (event.target === $('modal-root')) closeModal();
  });
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
    button.innerHTML = `
      <span class="label" title="${escapeHtml(account.email)}">${escapeHtml(account.email)}</span>
      <span class="count" title="移除账号">✕</span>`;
    button.querySelector('.label').addEventListener('click', () => selectAccount(account.id));
    button.querySelector('.count').addEventListener('click', (event) => {
      event.stopPropagation();
      removeAccount(account);
    });
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

  if (state.mailboxes.length === 0) {
    state.messages = [];
    renderMessages();
    return;
  }

  const inbox = state.mailboxes.find((box) => box.name.toUpperCase() === 'INBOX');
  selectMailbox(inbox ?? state.mailboxes[0]);
  state.limit = PAGE_SIZE;
  renderMailboxes();
  await loadMessages({ reset: true });
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
  // Search covers cached mail, so it takes the pane out of the drafts list — leaving it in drafts
  // would swallow the results the user just asked for.
  state.view = 'mail';
  syncMailboxRows();
  $('btn-search-clear').hidden = false;

  const status = $('search-status');
  status.hidden = false;
  status.textContent = hits.length
    ? `本地缓存命中 ${hits.length} 条 · 正文只搜已打开过的邮件`
    : '本地缓存里没有匹配。搜索只覆盖已同步的邮件，正文只覆盖已打开过的。';

  renderMessages();
}

function exitSearchMode({ reload = true } = {}) {
  const wasSearching = Boolean(state.searchQuery) || state.searchHits.length > 0;
  state.searchQuery = '';
  state.searchHits = [];
  clearTimeout(searchTimer);
  $('search-status').hidden = true;
  $('btn-search-clear').hidden = true;
  if (wasSearching && reload) loadMessages({ reset: true });
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
    button.innerHTML = `<span class="label" title="${escapeHtml(label)}">${escapeHtml(label)}</span>`;
    button.addEventListener('click', () => {
      // Choosing a folder is what leaves the drafts list; `selectMailbox` itself is also called
      // while loading an account, where jumping out of drafts would be exactly wrong.
      state.view = 'mail';
      selectMailbox(box);
      state.limit = PAGE_SIZE;
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

async function retrySend(id) {
  const queued = await run('重新发送', () => invoke('retry_send', { id }));
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
 * Fetch the page of mail just older than what the cache holds, and prepend it.
 *
 * The list only ever holds the newest window, so this is the only way back into an old mailbox. The
 * backend does the "which UIDs" work (`load_more_messages` -> `fetch_older_headers`); this function
 * only decides what the user sees, and it keeps the incremental cursor out of the picture entirely.
 */
async function loadOlder() {
  if (state.historyDone || !state.accountId || !state.mailbox) return;

  const button = $('btn-load-more');
  button.disabled = true;
  button.textContent = '正在加载更早的邮件…';

  const page = await run('加载更早的邮件', () =>
    invoke('load_more_messages', {
      accountId: state.accountId,
      mailbox: state.mailbox,
      limit: PAGE_SIZE,
    })
  );

  if (page === null) {
    // Failed: put the button back so the user can retry, and leave the list alone.
    button.disabled = false;
    renderHistoryButton();
    return;
  }

  if (page.messages.length > 0) {
    const known = new Set(state.messages.map((m) => m.uid));
    const added = page.messages.filter((m) => !known.has(m.uid));
    state.messages = state.messages.concat(added).sort((a, b) => b.uid - a.uid);
    renderMessages();
  }

  if (page.done || page.messages.length === 0) {
    state.historyDone = true;
  }
  renderHistoryButton();
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
    state.limit = PAGE_SIZE;
    state.selectedUid = null;
    state.detail = null;
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
      limit: state.limit,
    })
  );

  if (messages === null) {
    list.innerHTML = '<div class="empty">收取失败，请查看右下角提示。</div>';
    return;
  }

  // Newest first: the server returns ascending UIDs and people want the latest on top.
  state.messages = messages.slice().sort((a, b) => b.uid - a.uid);
  renderMessages();
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
    $('list-footer').hidden = true;
    return;
  }

  for (const message of state.messages) {
    const button = document.createElement('button');
    const selected =
      message.uid === state.selectedUid && state.mailbox === state.selectedFolder;
    button.className = 'msg' + (message.seen ? '' : ' unread') + (selected ? ' selected' : '');
    button.innerHTML = `
      <div class="msg-top">
        <span class="msg-from">${escapeHtml(senderLabel(message))}</span>
        <span class="msg-date">${escapeHtml(formatDate(message.date))}</span>
      </div>
      <div class="msg-subject">${escapeHtml(message.subject)}</div>`;
    button.addEventListener('click', () => openMessage(message));
    list.appendChild(button);
  }

  $('list-footer').hidden = state.messages.length < state.limit;
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
    // Tag only the hits that come from somewhere other than the folder on screen — the tag exists to
    // answer "why is this here?", and while everything is in the current folder that is noise.
    const tag =
      hit.folder === state.mailbox
        ? ''
        : `<span class="msg-folder">${escapeHtml(folderLabel(hit.folder))}</span>`;
    button.innerHTML = `
      <div class="msg-top">
        <span class="msg-from">${tag}${escapeHtml(senderLabel(hit))}</span>
        <span class="msg-date">${escapeHtml(formatDate(hit.date))}</span>
      </div>
      <div class="msg-subject">${escapeHtml(hit.subject)}</div>`;
    button.addEventListener('click', () => openHit(hit));
    list.appendChild(button);
  }
}

async function openMessage(message) {
  state.selectedUid = message.uid;
  state.selectedFolder = state.mailbox;
  state.detail = null;
  state.bodyMode = 'html';
  // Blocking is the default for every message; an opt-in must not leak into the next one.
  state.allowRemoteImages = false;
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
      mailbox: state.mailbox,
      uid: message.uid,
    })
  );
  if (!detail) return;

  state.detail = detail;
  renderReader();

  // Reading a message marks it \Seen; reflect that locally so the list agrees with the server.
  if (!message.seen) {
    message.seen = true;
    renderMessages();
    run('标记已读', () =>
      invoke('mark_seen', {
        accountId: state.accountId,
        mailbox: state.mailbox,
        uid: message.uid,
        seen: true,
      })
    );
  }
}

function renderReader() {
  const detail = state.detail;
  if (!detail) {
    $('reader-placeholder').hidden = false;
    $('reader-body').hidden = true;
    return;
  }

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

  // Attachments
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

  applyBodyMode();
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
        mailbox: state.mailbox,
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
    frame.srcdoc = buildSandboxedDocument(html, state.allowRemoteImages);
  } else {
    frame.hidden = true;
    pre.hidden = false;
    $('remote-image-bar').hidden = true;
    pre.textContent = detail.body_text || '(无正文)';
  }
}

/** Remote images in the body, i.e. the ones that would phone home when rendered. */
function countRemoteImages(html) {
  const matches = html.match(/<img\b[^>]*\bsrc\s*=\s*["']?\s*https?:/gi);
  return matches ? matches.length : 0;
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
  const message = state.messages.find((m) => m.uid === state.selectedUid);
  if (!confirm(`删除这封邮件？\n\n${message ? message.subject : ''}`)) return;

  const ok = await run('删除邮件', async () => {
    await invoke('delete_message', {
      accountId: state.accountId,
      mailbox: state.mailbox,
      uid: state.selectedUid,
    });
    return true;
  });

  if (ok) {
    toast('邮件已删除', 'ok');
    state.messages = state.messages.filter((m) => m.uid !== state.selectedUid);
    state.selectedUid = null;
    state.detail = null;
    renderMessages();
    renderReader();
  }
}

async function toggleRead() {
  if (!state.detail) return;
  const message = state.messages.find((m) => m.uid === state.selectedUid);
  if (!message) return;
  const nextSeen = !message.seen;

  const ok = await run('修改已读状态', async () => {
    await invoke('mark_seen', {
      accountId: state.accountId,
      mailbox: state.mailbox,
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
    subject = detail.subject.startsWith('Re:') ? detail.subject : `Re: ${detail.subject}`;
    inReplyTo = detail.message_id;
    references = [...detail.references, detail.message_id].filter(Boolean);
    bodyText = quoteOriginal(detail);
  } else if (mode === 'forward' && detail) {
    subject = detail.subject.startsWith('Fwd:') ? detail.subject : `Fwd: ${detail.subject}`;
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
    },
    onClose: (modal) => {
      clearTimeout(saveTimer);
      saveTimer = null;
      // A queued send kept the draft on purpose — the worker deletes it once the message is really
      // out — so there is nothing to save again here.
      if (sent) return;
      const fields = readFields(modal);
      if (isBlank(fields) && !persisted) return;
      queueSave(fields);
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
        // Rebuilt field by field: the stored form also carries `sha256`, which `OutgoingAttachment`
        // does not have.
        attachments: attachments.map((file) => ({
          name: file.name,
          mime: file.mime,
          path: file.path,
          size: file.size,
        })),
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
  const div = document.createElement('div');
  div.innerHTML = html;
  return div.textContent || '';
}

/* ---------------------------------------------------------------- account wizard */

/**
 * Account whose credential was saved but which failed verification.
 *
 * Kept so a retry does not require retyping the authorization code. If the user gives up and
 * closes the dialog, `closeModal` removes it so a typo cannot leave a broken account behind.
 */
let pendingWizardAccountId = null;

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
    <div id="wiz-detected" class="detected" hidden></div>
    <div id="wiz-note" class="callout" hidden></div>
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

      if (!email.includes('@')) {
        toast('请填写完整的邮箱地址。', 'err');
        return false;
      }
      if (!password) {
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

      try {
        await invoke('save_account', {
          providerId,
          email,
          displayName,
          password,
          // null lets the backend fall back to the address.
          username: username || null,
        });

        const result = await invoke('verify_account', { accountId });
        renderReport(report, result);
        verified = result.imap_ok && result.smtp_ok;
      } catch (error) {
        report.innerHTML = `<div class="report bad">${escapeHtml(errText(error))}</div>`;
      }

      if (!verified) {
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

  const updateDetected = () => {
    const email = emailInput.value.trim();
    const provider = state.providers.find((p) => p.id === providerSelect.value);
    if (!provider) return;

    // Show what will actually be sent as the login name, so an override is never invisible.
    const effectiveLogin = usernameInput.value.trim() || email || '(未填写)';

    const detected = modal.querySelector('#wiz-detected');
    const note = modal.querySelector('#wiz-note');

    detected.hidden = false;
    detected.innerHTML = `
      <div><span class="k">IMAP</span><span class="mono">${escapeHtml(provider.imap_host)}:${
        provider.imap_port
      } (${provider.imap_security})</span></div>
      <div><span class="k">SMTP</span><span class="mono">${escapeHtml(provider.smtp_host)}:${
        provider.smtp_port
      } (${provider.smtp_security})</span></div>
      <div><span class="k">认证</span>${
        provider.auth === 'oauth2'
          ? '<strong>需要 OAuth2（当前版本尚不支持）</strong>'
          : provider.auth === 'app_password'
            ? '需要授权码 / 应用专用密码'
            : '邮箱登录密码'
      }${provider.quirks.needs_id ? '　·　该服务商要求发送 IMAP ID 命令（已内置处理）' : ''}</div>
      <div><span class="k">状态</span>${
        provider.verified ? '✅ 服务器地址已实测可用' : '⚠️ 尚未实测验证'
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
