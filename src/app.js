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

const PAGE_SIZE = 50;

/** How many local search hits to ask for. Higher than a page because results span all folders. */
const SEARCH_LIMIT = 200;

/** Typing pause before a search runs. Search is local, but it still hits SQLite on every keystroke. */
const SEARCH_DEBOUNCE_MS = 250;

const state = {
  providers: [],
  accounts: [],
  accountId: null,
  mailboxes: [],
  mailbox: null,
  /** Decoded mailbox name for display; `mailbox` stays in wire form for commands. */
  mailboxLabel: null,
  messages: [],
  limit: PAGE_SIZE,
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

function toast(message, kind = 'info', ttl = 5000) {
  const el = document.createElement('div');
  el.className = `toast ${kind}`;
  el.textContent = message;
  $('toast-root').appendChild(el);
  setTimeout(() => el.remove(), ttl);
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
  $('btn-refresh').addEventListener('click', () => loadMessages({ reset: true }));
  $('btn-load-more').addEventListener('click', () => {
    state.limit += PAGE_SIZE;
    loadMessages();
  });
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
    await loadMailboxes();
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
    return;
  }
  for (const box of state.mailboxes) {
    const label = box.display_name || box.name;
    const button = document.createElement('button');
    button.className = 'row' + (box.name === state.mailbox ? ' active' : '');
    button.innerHTML = `<span class="label" title="${escapeHtml(label)}">${escapeHtml(label)}</span>`;
    button.addEventListener('click', () => {
      selectMailbox(box);
      state.limit = PAGE_SIZE;
      renderMailboxes();
      loadMessages({ reset: true });
    });
    list.appendChild(button);
  }
}

/* ---------------------------------------------------------------- messages */

async function loadMessages({ reset = false } = {}) {
  if (!state.accountId || !state.mailbox) return;
  if (reset) {
    state.limit = PAGE_SIZE;
    state.selectedUid = null;
    state.detail = null;
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

function openComposer({ mode }) {
  if (!state.accountId) {
    toast('请先添加一个邮箱账号。', 'err');
    return;
  }
  const account = state.accounts.find((a) => a.id === state.accountId);
  const detail = state.detail;

  let to = '';
  let subject = '';
  let inReplyTo = null;
  let references = [];
  let quoted = '';

  if (mode === 'reply' && detail) {
    to = detail.reply_to || detail.from_addr || '';
    subject = detail.subject.startsWith('Re:') ? detail.subject : `Re: ${detail.subject}`;
    inReplyTo = detail.message_id;
    references = [...detail.references, detail.message_id].filter(Boolean);
    quoted = quoteOriginal(detail);
  } else if (mode === 'forward' && detail) {
    subject = detail.subject.startsWith('Fwd:') ? detail.subject : `Fwd: ${detail.subject}`;
    quoted = quoteOriginal(detail);
  }

  const body = `
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
        <input id="compose-cc" placeholder="可留空" />
      </div>
      <div class="field">
        <label>密送</label>
        <input id="compose-bcc" placeholder="可留空" />
      </div>
    </div>
    <div class="field">
      <label>主题</label>
      <input id="compose-subject" value="${escapeHtml(subject)}" />
    </div>
    <div class="field">
      <label>正文</label>
      <textarea id="compose-body" placeholder="写点什么…">${escapeHtml(quoted)}</textarea>
      <span class="hint">以纯文本发送；如果粘贴了 HTML 也会一并作为富文本正文发出。</span>
    </div>
    <div class="field">
      <label>附件</label>
      <div id="compose-attachments" class="compose-attachments" hidden></div>
      <button type="button" id="btn-add-attachment" class="btn ghost">📎 添加附件</button>
      <span class="hint">文件从磁盘读取，不会经过界面；单个附件过大时服务器可能拒收。</span>
    </div>`;

  // Kept in this closure: the list belongs to one open composer and must not survive it.
  let attachments = [];

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
    title: mode === 'reply' ? '回复' : mode === 'forward' ? '转发' : '写邮件',
    bodyHtml: body,
    confirmLabel: '发送',
    onReady: (modal) => {
      modal.querySelector('#btn-add-attachment').addEventListener('click', async () => {
        const picked = await run('选择附件', () => invoke('pick_attachments'));
        if (!picked || picked.length === 0) return;
        attachments = attachments.concat(picked);
        renderAttachments(modal);
      });

      // One listener on the container rather than one per chip: the chips are re-created on every
      // change, and per-chip listeners would leak with them.
      modal.querySelector('#compose-attachments').addEventListener('click', (event) => {
        const index = event.target?.dataset?.remove;
        if (index === undefined) return;
        attachments.splice(Number(index), 1);
        renderAttachments(modal);
      });
    },
    onConfirm: async (modal) => {
      const splitAddresses = (value) =>
        value
          .split(/[,;，；]/)
          .map((s) => s.trim())
          .filter(Boolean);

      const toList = splitAddresses(modal.querySelector('#compose-to').value);
      if (toList.length === 0) {
        toast('请填写收件人。', 'err');
        return false;
      }

      const outgoing = {
        to: toList,
        cc: splitAddresses(modal.querySelector('#compose-cc').value),
        bcc: splitAddresses(modal.querySelector('#compose-bcc').value),
        subject: modal.querySelector('#compose-subject').value,
        bodyText: modal.querySelector('#compose-body').value,
        bodyHtml: null,
        inReplyTo,
        references,
        attachments,
      };

      const sent = await run('发送邮件', async () => {
        await invoke('send_mail', { accountId: state.accountId, mail: outgoing });
        return true;
      });
      if (sent) toast('邮件已发送', 'ok');
      return sent;
    },
  });
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

function openModal({ title, bodyHtml, confirmLabel, onConfirm, onReady }) {
  const root = $('modal-root');
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
  root.hidden = true;
  root.innerHTML = '';
}

/* ---------------------------------------------------------------- start */

boot().catch((error) => toast(`启动失败：${errText(error)}`, 'err', 15000));
