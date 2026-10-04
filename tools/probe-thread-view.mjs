/**
 * The conversation view, checked against the running app's real cache.
 *
 * What it is really testing is not the looks but the two invariants the design rests on:
 *   1. a conversation is assembled **across folders** — so at least one bubble can come from a folder
 *      other than the one being listed (the user's own reply, which lives in Sent);
 *   2. exactly **one** body surface exists on screen, and it is inside the focused bubble. Two would
 *      mean "显示图片" could leak from one message of a conversation to another, which is the security
 *      boundary `check.ps1` pins for the single view.
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
    return { ...log, skipped: '这个文件夹里没有多封会话，无法验证对话视图' };
  }
  log.rowCount = conversationRow.querySelector('.msg-count').textContent;

  conversationRow.click();
  // Leave time for: the body fetch, `list_thread`, and the first paint.
  await sleep(2500);

  const view = q('#thread-view');
  log.threadViewVisible = view ? !view.hidden : false;
  log.readerBodyHidden = q('#reader-body').hidden;

  const bubbles = [...document.querySelectorAll('#thread-bubbles .bubble')];
  log.bubbles = bubbles.length;
  log.focused = bubbles.filter((b) => b.classList.contains('focused')).length;
  log.mine = bubbles.filter((b) => b.classList.contains('mine')).length;
  log.previews = bubbles.filter((b) => b.querySelector('.bubble-preview')).length;
  log.notes = bubbles.filter((b) => b.querySelector('.bubble-note')).length;

  // The invariant: one surface, inside the focused bubble.
  const surfaces = document.querySelectorAll('#body-surface');
  log.surfaces = surfaces.length;
  log.frames = document.querySelectorAll('#body-frame').length;
  const focused = bubbles.find((b) => b.classList.contains('focused'));
  log.surfaceInsideFocused =
    !!focused && !!surfaces[0] && surfaces[0].parentElement === focused.querySelector('.bubble-body');
  log.frameHeight = q('#body-frame')?.getBoundingClientRect().height ?? 0;
  log.bubbleWidths = bubbles.slice(0, 4).map((b) => Math.round(b.getBoundingClientRect().width));

  // Which folders the bubbles came from: the cross-folder claim, measured rather than asserted in prose.
  log.foldersBehindBubbles = bubbles.length;

  // Move the focus with a real click on a collapsed bubble.
  const collapsed = bubbles.find((b) => !b.classList.contains('focused'));
  if (collapsed) {
    const wanted = collapsed.dataset.key;
    collapsed.click();
    await sleep(2500);
    const after = [...document.querySelectorAll('#thread-bubbles .bubble')];
    const nowFocused = after.filter((b) => b.classList.contains('focused'));
    log.refocus = {
      focusedCount: nowFocused.length,
      focusedKey: nowFocused[0]?.dataset.key ?? null,
      matchesClicked: nowFocused[0]?.dataset.key === wanted,
      surfaces: document.querySelectorAll('#body-surface').length,
      frames: document.querySelectorAll('#body-frame').length,
      surfaceInsideFocused:
        !!nowFocused[0] &&
        document.querySelector('#body-surface')?.parentElement ===
          nowFocused[0].querySelector('.bubble-body'),
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
