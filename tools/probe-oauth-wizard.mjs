/**
 * The wizard's OAuth path, checked on the running window.
 *
 * This is the regression probe for a bug a real user hit: 「打开浏览器登录」点击没反应. The button was
 * visible and wired to nothing, because the listener was attached from inside `updateDetected()` under
 * a condition that only becomes true when the asynchronous `oauth_client_default` comes back — so when
 * the last thing the user did was the typing that *started* that request, the path was revealed by the
 * `.then()` while the listener was never attached.
 *
 * It deliberately stops short of clicking the button: that opens a real browser and starts a
 * two-minute wait for a real authorization. Pressing it is a person's step, not a test's.
 *
 * Requires the app to be built with `--remote-debugging-port=9222` in `additionalBrowserArgs`.
 */
(async () => {
  const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
  const q = (sel) => document.querySelector(sel);
  const log = {};

  document.querySelectorAll('#modal-root [data-role="cancel"]').forEach((el) => el.click());
  await sleep(200);

  q('#btn-add-account').click();
  await sleep(250);
  log.modalOpen = !!q('#wiz-email');
  if (!log.modalOpen) return log;

  // The failing sequence: choose a provider that has **no** OAuth path, so the first pass through
  // `updateDetected` cannot attach the listener, then type an address that switches to one that does.
  // That leaves the asynchronous answer as the only thing that reveals the button.
  q('#wiz-provider').value = 'qq';
  q('#wiz-provider').dispatchEvent(new Event('change', { bubbles: true }));
  await sleep(400);

  const choice = q('#wiz-auth-choice');
  const path = q('#wiz-oauth-path');
  const go = q('#wiz-oauth-go');
  log.beforeTyping = {
    oauthAvailable: choice.dataset.oauthAvailable ?? null,
    pathHidden: path.hidden,
    goWired: go.dataset.wired ?? null,
  };

  q('#wiz-email').value = 'someone@gmail.com';
  q('#wiz-email').dispatchEvent(new Event('input', { bubbles: true }));
  await sleep(1000);

  log.afterDetect = {
    provider: q('#wiz-provider').value,
    oauthAvailable: choice.dataset.oauthAvailable ?? null,
    pathHidden: path.hidden,
    goWired: go.dataset.wired ?? null,
    // Pre-filled from the backend, so the renderer and `oauth_sign_in` cannot disagree.
    clientIdHead: q('#wiz-client-id').value.slice(0, 16),
    hasSecret: !!q('#wiz-client-secret').value,
    registrationNotice: q('#wiz-oauth-notice').textContent.trim().slice(0, 40),
    confirmLabel: q('[data-role="confirm"]')?.textContent,
  };

  // The one thing that must hold: a visible button is a wired button.
  log.BUG_IF_TRUE = path.hidden === false && go.dataset.wired === undefined;

  document.querySelectorAll('#modal-root [data-role="cancel"]').forEach((el) => el.click());
  await sleep(200);
  return log;
})()
