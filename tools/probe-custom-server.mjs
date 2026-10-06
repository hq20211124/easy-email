/**
 * The wizard's manual-endpoint block, checked on the running window.
 *
 * It answers the four questions the plan asks of the custom-server path, and it never touches a real
 * mail server: everything it types is `127.0.0.1` with nothing listening, or a hostname that is
 * *supposed* to be refused before any connection is attempted.
 *
 *   1. choosing 自定义 / 自建服务器 shows the block and prefills `mail.<域名>`;
 *   2. `imap.example.com:993` is refused with the sentence that names the 端口 field;
 *   3. **the refused attempt saves nothing** — the account list does not grow. That is the property
 *      worth a probe: the validation has to run *before* the credential is written and before
 *      `accounts.json` is touched, and no pure-function test can see where the call sits. (The file
 *      itself is checked from the shell around this probe; the list here is the same data.)
 *   4. a host that cannot be reached is reported as a connection failure, and dismissing the dialog
 *      removes the half-created account again.
 *
 * Requires the app to be built with `--remote-debugging-port=9222` in `additionalBrowserArgs`.
 */
(async () => {
  const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
  const q = (sel) => document.querySelector(sel);
  const accounts = () => document.querySelectorAll('#account-list .row').length;
  const set = (sel, value) => {
    const el = q(sel);
    el.value = value;
    el.dispatchEvent(new Event('input', { bubbles: true }));
    el.dispatchEvent(new Event('change', { bubbles: true }));
  };
  const log = {};

  document.querySelectorAll('#modal-root [data-role="close"]').forEach((el) => el.click());
  await sleep(300);

  log.accountsBefore = accounts();

  q('#btn-add-account').click();
  await sleep(300);
  if (!q('#wiz-email')) return { ...log, modalDidNotOpen: true };

  q('#wiz-provider').value = 'custom';
  q('#wiz-provider').dispatchEvent(new Event('change', { bubbles: true }));
  await sleep(600);

  set('#wiz-email', 'probe@example.com');
  set('#wiz-password', 'not-a-real-password');
  await sleep(600);

  const block = q('#wiz-endpoints');
  log.step1 = {
    blockHidden: block.hidden,
    imapHost: q('#wiz-imap-host').value,
    smtpHost: q('#wiz-smtp-host').value,
    imapPort: q('#wiz-imap-port').value,
    imapSecurity: q('#wiz-imap-security').value,
    securityOptions: [...q('#wiz-imap-security').options].map((o) => o.value),
    note: q('#wiz-endpoint-note').textContent.trim(),
    statusLine: [...document.querySelectorAll('#wiz-detected div')]
      .map((d) => d.textContent.trim())
      .find((t) => t.startsWith('状态')),
  };

  // ---- the refused host must not create anything
  set('#wiz-imap-host', 'imap.example.com:993');
  await sleep(200);
  q('[data-role="confirm"]').click();
  await sleep(1500);
  log.step2 = {
    report: q('#wiz-report')?.textContent.trim() ?? null,
    modalStillOpen: !!q('#wiz-email'),
  };
  log.step3 = { accountsAfterRefusedSave: accounts() };

  // ---- a host that exists but answers nothing, then dismiss
  set('#wiz-imap-host', '127.0.0.1');
  set('#wiz-imap-port', '9');
  set('#wiz-smtp-host', '127.0.0.1');
  set('#wiz-smtp-port', '9');
  await sleep(200);
  q('[data-role="confirm"]').click();
  await sleep(3000);
  log.step4 = {
    report: q('#wiz-report')?.textContent.trim().slice(0, 400) ?? null,
    modalStillOpen: !!q('#wiz-email'),
    accountsWhileUnverified: accounts(),
  };

  document.querySelectorAll('#modal-root [data-role="close"]').forEach((el) => el.click());
  await sleep(1500);
  log.step4.accountsAfterDismiss = accounts();

  log.BUG_IF_TRUE =
    log.step1.blockHidden !== false ||
    log.step1.imapHost !== 'mail.example.com' ||
    log.step3.accountsAfterRefusedSave !== log.accountsBefore ||
    log.step4.accountsAfterDismiss !== log.accountsBefore;

  return log;
})()
