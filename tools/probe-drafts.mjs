/**
 * Does the composer actually save, restore and discard a draft — through real Tauri IPC?
 *
 * Run it inside the running app:
 *
 *     node tools/inspect-dom.mjs @tools/probe-drafts.mjs
 *
 * Requires the app to be built with `--remote-debugging-port=9222` in `additionalBrowserArgs`
 * (see the note at the top of inspect-dom.mjs); remove it again afterwards.
 *
 * Why this exists as a tool rather than a one-off: the draft path crosses three boundaries that
 * no single test sees — the renderer builds a JSON object, Tauri hands it to `save_draft`, and
 * serde deserialises it into `Draft`. A field name that does not match (`bodyText` for
 * `body_text`) is a runtime `missing field` and nothing on either side complains at build time:
 * that is exactly how `send_mail` shipped broken. `tools/check.ps1` compares the key sets
 * statically; this probe proves the same contract against the real command surface, and then
 * drives the UI the way a user does (type → autosave → close → drafts list → discard).
 *
 * It uses one fixed, obviously-synthetic draft id and removes it at the end and at the start, so
 * running it against a real profile leaves nothing behind.
 */
(async () => {
  const invoke = window.__TAURI__.core.invoke;
  const { draft, serializeAttachments, outgoingMail } = window.__easyEmailPayload;
  const steps = [];
  const note = (step, detail) => steps.push({ step, ...detail });

  const ID = 'dsh-verify-m1a7';
  const ACCOUNT = state.accountId;
  if (!ACCOUNT) return { failed: 'no account configured on this profile' };

  const wait = (ms) => new Promise((resolve) => setTimeout(resolve, ms));
  const drafts = () => invoke('list_drafts', { accountId: ACCOUNT });
  const mine = async () => (await drafts()).find((d) => d.id === ID) ?? null;

  note('payload_builders', {
    outgoingMailKeys: Object.keys(outgoingMail({})),
    draftKeys: Object.keys(draft({})),
  });

  // ---------------------------------------------------------------- 1. save_draft over IPC
  await invoke('delete_draft', { id: ID }); // left over from an interrupted earlier run
  const before = (await drafts()).length;
  const now = Math.floor(Date.now() / 1000);

  await invoke('save_draft', {
    draft: draft({
      id: ID,
      accountId: ACCOUNT,
      toAddrs: 'tester@example.com',
      subject: 'M1a-7 探针草稿',
      bodyText: '第一行',
      attachmentsJson: serializeAttachments([
        { name: 'a.txt', mime: 'text/plain', path: 'C:\\nonexistent\\a.txt', size: 3, sha256: 'ab' },
      ]),
      referencesJson: '["<parent@example.com>"]',
      createdAt: now,
      updatedAt: now,
    }),
  });

  const stored = await mine();
  note('save_draft', {
    accepted: Boolean(stored),
    countDelta: (await drafts()).length - before,
    subject: stored?.subject,
    references: stored?.references,
    attachments: stored?.attachments_json,
  });

  // ---------------------------------------------------------------- 2. the drafts list UI
  await showDrafts();
  const rowsBefore = document.querySelectorAll('#message-list [data-draft-id]').length;
  note('drafts_list', {
    rows: rowsBefore,
    badge: document.getElementById('drafts-count').textContent,
    title: document.getElementById('mailbox-title').textContent,
    rowMentionsSubject: document
      .querySelector(`#message-list [data-draft-id="${ID}"]`)
      ?.textContent.includes('M1a-7 探针草稿'),
  });

  // ---------------------------------------------------------------- 3. resume it in the composer
  const row = document.querySelector(`#message-list [data-draft-id="${ID}"]`);
  row.click();
  const modal = document.querySelector('#modal-root .modal');
  const prefilled = {
    to: modal.querySelector('#compose-to').value,
    subject: modal.querySelector('#compose-subject').value,
    body: modal.querySelector('#compose-body').value,
    attachments: modal.querySelectorAll('#compose-attachments .attachment').length,
    status: modal.querySelector('#compose-status').textContent,
  };
  note('resume', prefilled);

  // ---------------------------------------------------------------- 4. autosave on a 2 s pause
  const body = modal.querySelector('#compose-body');
  body.value = '第二行（自动保存）';
  body.dispatchEvent(new Event('input', { bubbles: true }));
  await wait(2600); // DRAFT_AUTOSAVE_MS + the write
  note('autosave', {
    status: modal.querySelector('#compose-status').textContent,
    storedBody: (await mine())?.body_text,
  });

  // ---------------------------------------------------------------- 5. save on close
  const edited = '第三行（关闭时保存）';
  body.value = edited;
  modal.querySelector('[data-role="close"]').click(); // dismiss, not send
  await wait(600);
  note('close_saves', {
    modalGone: document.querySelector('#modal-root .modal') === null,
    storedBody: (await mine())?.body_text,
    expected: edited,
  });

  // ---------------------------------------------------------------- 6. discard from the list
  window.confirm = () => true; // the probe is the user here
  await showDrafts();
  const again = document.querySelector(`#message-list [data-draft-id="${ID}"]`);
  again.querySelector('[data-role="discard"]').click();
  await wait(600);
  note('discard', {
    stillStored: Boolean(await mine()),
    countBackToStart: (await drafts()).length === before,
    rowsAfter: document.querySelectorAll('#message-list [data-draft-id]').length,
  });

  // ---------------------------------------------------------------- 7. the send payload shape
  // Sent with an account id that does not exist: Tauri deserialises the arguments *before* the
  // command body runs, so a wrong field name fails with "missing field" and a correct one fails
  // with "找不到账号" — the second is the proof, and no SMTP connection is ever attempted.
  // This is the exact bug M1a-7 fixed: the composer sent `bodyText`, so every send failed with
  // `missing field \`body_text\`` while the 发送 button looked perfectly wired.
  const sendError = await invoke('send_mail', {
    accountId: 'dsh-no-such-account',
    mail: outgoingMail({
      to: ['nobody@example.com'],
      subject: 'shape probe',
      bodyText: 'hello',
      references: [],
      attachments: [],
    }),
  }).then(
    () => '',
    (error) => errText(error)
  );
  note('send_payload', {
    error: sendError,
    reachedTheCommand: sendError.includes('找不到账号'),
    rejectedForThePayload: /missing field|invalid type/i.test(sendError),
  });

  // The falsification half: the shape v0.1 actually sent must still be refused. If this one is
  // accepted, the check above is not checking anything.
  const oldShapeError = await invoke('send_mail', {
    accountId: 'dsh-no-such-account',
    mail: {
      to: ['nobody@example.com'],
      cc: [],
      bcc: [],
      subject: 'shape probe',
      bodyText: 'hello',
      bodyHtml: null,
      inReplyTo: null,
      references: [],
      attachments: [],
    },
  }).then(
    () => '',
    (error) => errText(error)
  );
  note('send_payload_regression', {
    error: oldShapeError,
    refused: /missing field|unknown field/i.test(oldShapeError),
  });

  // Leave the profile exactly as it was, whatever happened above.
  await invoke('delete_draft', { id: ID });

  const failures = steps.flatMap((entry) => {
    if (entry.step === 'send_payload' && (!entry.reachedTheCommand || entry.rejectedForThePayload))
      return ['send_payload'];
    if (entry.step === 'send_payload_regression' && !entry.refused) return ['send_payload_regression'];
    if (entry.step === 'save_draft' && (!entry.accepted || entry.countDelta !== 1)) return ['save_draft'];
    if (entry.step === 'drafts_list' && (!entry.rowMentionsSubject || entry.rows < 1)) return ['drafts_list'];
    if (entry.step === 'resume' && (entry.body !== '第一行' || entry.attachments !== 1 || !entry.status))
      return ['resume'];
    if (entry.step === 'autosave' && entry.storedBody !== '第二行（自动保存）') return ['autosave'];
    if (entry.step === 'close_saves' && (entry.storedBody !== entry.expected || !entry.modalGone)) return ['close_saves'];
    if (entry.step === 'discard' && (entry.stillStored || !entry.countBackToStart)) return ['discard'];
    return [];
  });

  return { passed: failures.length === 0, failures, steps };
})()
