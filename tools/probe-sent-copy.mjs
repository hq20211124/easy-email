/**
 * Real-machine probe ④: does "查看副本" actually find the copy that was filed?
 *
 * The message was sent for real in probe ② (`已发送副本：归档 1` in the log). This probe checks the two
 * halves of the button: the recorded link (`sent_index`, read through `sent_copy`) and what the user
 * sees when they press it — including that the reader ends up open on the copy in the Sent folder.
 */
(async () => {
  const invoke = window.__TAURI__.core.invoke;
  const wait = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

  // The outbox keeps sent rows as history, which is where the button lives.
  const rows = await invoke('list_outbox', { accountId: state.accountId });
  const sent = rows.filter((row) => row.state === 'sent' && row.sent_message_id).at(-1);
  if (!sent) return { found: false, rows: rows.length };

  const record = await invoke('sent_copy', {
    accountId: state.accountId,
    messageId: sent.sent_message_id,
  });

  // Now the user-facing half: switch to the outbox and press the button.
  state.view = 'outbox';
  await refreshOutbox();
  await wait(400);
  const row = document.querySelector(`#message-list [data-outbox-id="${sent.id}"]`);
  if (!row) return { found: true, sentId: sent.id, record, pressed: false, reason: 'no row' };

  const button = [...row.querySelectorAll('button')].find((candidate) =>
    candidate.textContent.includes('查看副本')
  );
  if (!button) {
    return {
      found: true,
      sentId: sent.id,
      record,
      pressed: false,
      buttons: [...row.querySelectorAll('button')].map((candidate) => candidate.textContent),
    };
  }

  button.click();
  await wait(3500);
  return {
    found: true,
    sentId: sent.id,
    messageId: sent.sent_message_id,
    record,
    pressed: true,
    view: state.view,
    mailbox: state.mailbox,
    openedUid: state.selectedUid,
    detailSubject: state.detail?.subject ?? null,
    readingNow: document.getElementById('reader-subject')?.textContent ?? null,
  };
})()
