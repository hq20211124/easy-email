/**
 * The conversation view, checked against the running app's real cache.
 *
 * What it is really testing is not the looks but the invariants the design rests on:
 *   1. a conversation is assembled **across folders** — so at least one bubble can come from a folder
 *      other than the one being listed (the user's own reply, which lives in Sent);
 *   2. exactly **one** body surface exists on screen, and it is inside the expanded bubble. Two would
 *      mean "显示图片" could leak from one message of a conversation to another, which is the security
 *      boundary `check.ps1` pins for the single view;
 *   3. the interaction the user asked for (2026-10-06): every bubble starts collapsed, clicking one
 *      expands it, clicking its head collapses it again, and the reply box at the bottom names the
 *      address it would answer **before** the button is pressed.
 *
 * It deliberately never presses 发送: the reply would go to a real correspondent. What is checked is
 * that the box is wired and refuses an empty message — the enqueue path itself is the composer's, and
 * `tools/probe-outbox.mjs` already drives it end to end.
 *
 * Main frame only; timers work here (unlike the sandboxed body frame).
 */
(async () => {
  const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
  const q = (sel) => document.querySelector(sel);
  const log = {};

  // The view mode is a user preference, so the probe drives it explicitly and puts it back at the end.
  const originalMode = localStorage.getItem('ee.readerMode');
  log.modeButtonBefore = q('#btn-view-mode')?.textContent?.trim();

  // Start from the mail list, not from a search or a local view.
  const searchClear = q('#btn-search-clear');
  if (searchClear && !searchClear.hidden) searchClear.click();
  await sleep(300);

  const list = q('#message-list');
  log.listRows = list.querySelectorAll('.msg').length;

  // A conversation row is the one the list already folded: it carries the message count.
  const rows = [...list.querySelectorAll('.msg')];
  const conversationRow = rows.find((row) => row.querySelector('.msg-count'));
  log.conversationRows = rows.filter((row) => row.querySelector('.msg-count')).length;
  if (!conversationRow) {
    return { ...log, skipped: '这个文件夹里没有多封会话，无法验证对话模式' };
  }
  log.rowCount = conversationRow.querySelector('.msg-count').textContent;

  conversationRow.click();
  // Leave time for: the body fetch, `list_thread`, and the first paint.
  await sleep(2500);

  // ---- the default: everything collapsed
  const view = q('#thread-view');
  log.threadViewVisible = view ? !view.hidden : false;
  log.readerBodyHidden = q('#reader-body').hidden;

  let bubbles = [...document.querySelectorAll('#thread-bubbles .bubble')];
  log.bubbles = bubbles.length;
  log.expandedAtFirstPaint = bubbles.filter((b) => b.classList.contains('focused')).length;
  log.mine = bubbles.filter((b) => b.classList.contains('mine')).length;
  log.previews = bubbles.filter((b) => b.querySelector('.bubble-preview')).length;
  log.notes = bubbles.filter((b) => b.querySelector('.bubble-note')).length;
  log.chevrons = bubbles.map((b) => b.querySelector('.chevron')?.textContent ?? null).slice(0, 4);
  log.hintWhileAllCollapsed = q('#thread-meta')?.textContent?.includes('点任意一封展开') ?? false;

  // The invariant: one surface on screen, parked in the hidden reader while nothing is expanded.
  log.surfaces = document.querySelectorAll('#body-surface').length;
  log.frames = document.querySelectorAll('#body-frame').length;
  log.surfaceParkedWhenCollapsed =
    document.querySelector('#body-surface')?.parentElement?.id === 'reader-body';

  // ---- the reply box, before anything is expanded
  log.replyBox = {
    present: !!q('#thread-reply-text'),
    visible: q('#thread-reply') ? q('#thread-reply').getBoundingClientRect().height > 0 : false,
    target: q('#thread-reply-target')?.textContent?.trim() ?? null,
    sendLabel: q('#btn-thread-send')?.textContent?.trim() ?? null,
  };

  // An empty box must refuse to queue anything — which also proves the button is wired at all.
  q('#btn-thread-send').click();
  await sleep(600);
  log.emptySend = {
    toast: q('#toast-root')?.textContent?.trim().slice(0, 40) ?? null,
    textareaStillEmpty: q('#thread-reply-text').value === '',
    outboxBadge: q('#outbox-count')?.textContent?.trim() ?? null,
  };
  q('#toast-root')?.replaceChildren();
  await sleep(200);

  // ---- expanding one bubble
  const first = bubbles[0];
  first.querySelector('.bubble-head').click();
  await sleep(2500);
  bubbles = [...document.querySelectorAll('#thread-bubbles .bubble')];
  const expanded = bubbles.filter((b) => b.classList.contains('focused'));
  log.afterExpand = {
    expandedCount: expanded.length,
    expandedKey: expanded[0]?.dataset.key ?? null,
    matchesClicked: expanded[0]?.dataset.key === first.dataset.key,
    chevron: expanded[0]?.querySelector('.chevron')?.textContent ?? null,
    ariaExpanded: expanded[0]?.querySelector('.bubble-head')?.getAttribute('aria-expanded') ?? null,
    surfaces: document.querySelectorAll('#body-surface').length,
    frames: document.querySelectorAll('#body-frame').length,
    surfaceInsideExpanded:
      !!expanded[0] &&
      document.querySelector('#body-surface')?.parentElement ===
        expanded[0].querySelector('.bubble-body'),
    frameHeight: Math.round(q('#body-frame')?.getBoundingClientRect().height ?? 0),
    replyTarget: q('#thread-reply-target')?.textContent?.trim() ?? null,
    hintGone: !(q('#thread-meta')?.textContent?.includes('点任意一封展开') ?? false),
  };

  // Clicking the *body* must not collapse it: that is a live document being read and selected.
  q('#body-frame')?.dispatchEvent(new MouseEvent('click', { bubbles: true }));
  await sleep(400);
  log.clickInsideBodyKeepsItExpanded =
    document.querySelectorAll('#thread-bubbles .bubble.focused').length === 1;

  // ---- collapsing it again, from its head
  document.querySelector('#thread-bubbles .bubble.focused .bubble-head')?.click();
  await sleep(1200);
  log.afterCollapse = {
    expandedCount: document.querySelectorAll('#thread-bubbles .bubble.focused').length,
    surfaces: document.querySelectorAll('#body-surface').length,
    frames: document.querySelectorAll('#body-frame').length,
    surfaceParked: document.querySelector('#body-surface')?.parentElement?.id === 'reader-body',
    hintBack: q('#thread-meta')?.textContent?.includes('点任意一封展开') ?? false,
  };

  // ---- and expanding a *different* one collapses the first (one sandboxed renderer)
  if (bubbles.length > 1) {
    document.querySelectorAll('#thread-bubbles .bubble')[0].querySelector('.bubble-head').click();
    await sleep(2500);
    document.querySelectorAll('#thread-bubbles .bubble')[1].querySelector('.bubble-head').click();
    await sleep(2500);
    const all = [...document.querySelectorAll('#thread-bubbles .bubble')];
    const only = all.filter((b) => b.classList.contains('focused'));
    log.switchingBubbles = {
      expandedCount: only.length,
      expandedKey: only[0]?.dataset.key ?? null,
      surfaces: document.querySelectorAll('#body-surface').length,
      frames: document.querySelectorAll('#body-frame').length,
      surfaceInsideExpanded:
        !!only[0] &&
        document.querySelector('#body-surface')?.parentElement === only[0].querySelector('.bubble-body'),
    };
  }

  // And back to the single view, which must put the surface back where it belongs.
  q('#btn-view-mode').click();
  await sleep(800);
  log.singleView = {
    label: q('#btn-view-mode')?.textContent?.trim(),
    threadHidden: q('#thread-view')?.hidden,
    readerVisible: !q('#reader-body')?.hidden,
    surfaces: document.querySelectorAll('#body-surface').length,
    surfaceInsideReader: document.querySelector('#body-surface')?.parentElement?.id === 'reader-body',
  };

  // Restore the user's own preference.
  if (originalMode === null) localStorage.removeItem('ee.readerMode');
  else localStorage.setItem('ee.readerMode', originalMode);
  q('#btn-view-mode').click();
  await sleep(300);
  log.restoredLabel = q('#btn-view-mode')?.textContent?.trim();

  return log;
})()
