/**
 * Does the outbox actually hold, show, cancel and discard a queued message?
 *
 * Run it inside the running app:
 *
 *     node tools/inspect-dom.mjs @tools/probe-outbox.mjs
 *
 * Requires the app to be built with `--remote-debugging-port=9222` in `additionalBrowserArgs`
 * (see the note at the top of inspect-dom.mjs); remove it again afterwards.
 *
 * Why this exists: the queue is the only send path now, and every part of it that the user can see
 * — the badge, the countdown, the 撤销 button — is assembled in the renderer from a `user.db` row
 * that the Rust worker is also writing to. The unit and loopback tests cover the Rust half;
 * `tools/check.ps1` covers the payload contract; this covers the half in between.
 *
 * It is careful about one thing: **it must not send anything**. So the recipient is `@example.invalid`
 * (reserved, never deliverable), the undo window is the longest the backend allows (30 s), and the
 * message is cancelled within a second or two of being queued. The worst case if this probe dies
 * half way through is one rejected SMTP envelope, never a delivered message.
 */
(async () => {
  const invoke = window.__TAURI__.core.invoke;
  const { outgoingMail } = window.__easyEmailPayload;
  const steps = [];
  const note = (step, detail) => steps.push({ step, ...detail });
  const wait = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

  const ACCOUNT = state.accountId;
  if (!ACCOUNT) return { failed: 'no account configured on this profile' };

  const rows = () => invoke('list_outbox', { accountId: ACCOUNT });
  const rowOf = async (id) => (await rows()).find((item) => item.id === id) ?? null;

  // ---------------------------------------------------------------- 1. queue it
  const id = await invoke('enqueue_send', {
    accountId: ACCOUNT,
    mail: outgoingMail({
      to: ['nobody@example.invalid'],
      subject: 'M1a-9 探针（不会真的发出）',
      bodyText: '探针正文',
      references: [],
      attachments: [],
    }),
    draftId: null,
    // The longest window the backend allows, so a hang here cannot turn into a send.
    undoSeconds: 30,
  });

  const queued = await rowOf(id);
  note('enqueue', {
    id,
    state: queued?.state,
    spoolBytes: queued?.spool_bytes,
    spoolRecorded: Boolean(queued?.spool_path),
    payloadSubject: queued ? JSON.parse(queued.payload_json).subject : null,
  });

  // ---------------------------------------------------------------- 2. the outbox view
  await showOutbox();
  const row = document.querySelector(`#message-list [data-outbox-id="${id}"]`);
  note('outbox_view', {
    title: document.getElementById('mailbox-title').textContent,
    rowRendered: Boolean(row),
    badge: document.getElementById('outbox-count').textContent,
    stateLabel: row?.querySelector('.outbox-state')?.textContent ?? null,
    countdown: row?.querySelector('.outbox-note')?.textContent ?? null,
    buttons: [...(row?.querySelectorAll('.outbox-buttons .btn') ?? [])].map((b) => b.textContent),
  });

  // ---------------------------------------------------------------- 3. undo it from the row
  const undo = [...(row?.querySelectorAll('.outbox-buttons .btn') ?? [])].find(
    (button) => button.textContent === '撤销'
  );
  undo?.click();
  await wait(700);

  const cancelled = await rowOf(id);
  note('undo', {
    state: cancelled?.state,
    domSays: document
      .querySelector(`#message-list [data-outbox-id="${id}"] .outbox-state`)
      ?.textContent.trim(),
    badgeAfter: document.getElementById('outbox-count').textContent,
  });

  // ---------------------------------------------------------------- 4. discard it
  const discard = [...document.querySelectorAll(`#message-list [data-outbox-id="${id}"] .outbox-buttons .btn`)].find(
    (button) => button.textContent === '删除'
  );
  window.confirm = () => true; // the probe is the user here
  discard?.click();
  await wait(700);

  note('discard', {
    stillThere: Boolean(await rowOf(id)),
    rowsAfter: document.querySelectorAll('#message-list [data-outbox-id]').length,
  });

  // ---------------------------------------------------------------- 5. the composer button
  // The wiring this probes is "写信's 发送 queues instead of sending inline". The cancel below is
  // the very next local read after the click, so it lands well inside the composer's 5 s undo
  // window: even if this probe died right after, the worker cannot have sent anything yet.
  const COMPOSER_SUBJECT = 'M1a-9 探针·写信（不会真的发出）';
  openComposer({});
  const modal = document.querySelector('#modal-root .modal');
  modal.querySelector('#compose-to').value = 'nobody@example.invalid';
  modal.querySelector('#compose-subject').value = COMPOSER_SUBJECT;
  modal.querySelector('#compose-body').value = '探针正文';
  modal.querySelector('[data-role="confirm"]').click();

  let composerRow = null;
  for (let attempt = 0; attempt < 60 && !composerRow; attempt += 1) {
    composerRow =
      (await rows()).find(
        (item) => item.state === 'queued' && JSON.parse(item.payload_json).subject === COMPOSER_SUBJECT
      ) ?? null;
    if (!composerRow) await wait(50);
  }
  // Cancel first, then look at anything else.
  if (composerRow) await invoke('cancel_send', { id: composerRow.id });

  const composerState = composerRow ? (await rowOf(composerRow.id))?.state : null;

  // The composer saved a draft before queueing (deliberately: if the send never succeeds, that
  // draft must hold the last edit). Cancelling keeps it, so the probe removes its own fixture.
  const scratch = (await invoke('list_drafts', { accountId: ACCOUNT })).filter(
    (draft) => draft.subject === COMPOSER_SUBJECT
  );
  for (const draft of scratch) await invoke('delete_draft', { id: draft.id });

  note('composer', {
    queued: Boolean(composerRow),
    state: composerState,
    modalClosed: document.querySelector('#modal-root .modal') === null,
    scratchDraftsRemoved: scratch.length,
  });

  const failures = steps.flatMap((entry) => {
    const bad = [];
    if (entry.step === 'enqueue') {
      if (entry.state !== 'queued') bad.push('enqueue');
      if (!entry.spoolRecorded || !(entry.spoolBytes > 0)) bad.push('spool not recorded');
      if (entry.payloadSubject !== 'M1a-9 探针（不会真的发出）') bad.push('payload round-trip');
    }
    if (entry.step === 'outbox_view') {
      if (entry.title !== '发件箱') bad.push('outbox_view title');
      if (!entry.rowRendered) bad.push('outbox_view row');
      if (!entry.badge) bad.push('outbox_view badge');
      if (!/秒内可撤销/.test(entry.countdown ?? '')) bad.push('outbox_view countdown');
      if (!(entry.buttons ?? []).includes('撤销')) bad.push('outbox_view undo button');
    }
    if (entry.step === 'undo') {
      if (entry.state !== 'cancelled') bad.push('undo state');
      if (!/已撤销/.test(entry.domSays ?? '')) bad.push('undo dom');
      if (entry.badgeAfter) bad.push('undo badge not cleared');
    }
    if (entry.step === 'discard' && entry.stillThere) bad.push('discard');
    if (entry.step === 'composer') {
      if (!entry.queued) bad.push('composer did not queue');
      if (entry.state !== 'cancelled') bad.push('composer cancel');
      if (!entry.modalClosed) bad.push('composer modal stayed open');
    }
    return bad;
  });

  return { passed: failures.length === 0, failures, steps };
})()
