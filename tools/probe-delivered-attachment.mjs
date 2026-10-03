/**
 * Did the message that went out carry the attachment the sender approved — the *frozen* one?
 *
 * The probe changed the file on disk after the message was enqueued. If the delivered copy is the
 * version from before the change (31 bytes, not 76), then the freeze is real: what is sent is what was
 * approved, and a later edit to the file cannot change it.
 */
(async () => {
  const invoke = window.__TAURI__.core.invoke;
  const wait = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

  const subject = '附件被改动探针';
  await invoke('sync_now', { accountId: state.accountId, mailbox: 'INBOX' }).catch(() => null);
  await wait(2500);

  const summaries = await invoke('list_messages', {
    accountId: state.accountId,
    mailbox: 'INBOX',
    limit: 40,
  });
  const hit = summaries.find((message) => String(message.subject).includes(subject));
  if (!hit) {
    return { found: false, scanned: summaries.length, newest: summaries.slice(0, 3) };
  }

  const detail = await invoke('fetch_message', {
    accountId: state.accountId,
    mailbox: 'INBOX',
    uid: hit.uid,
  });
  return {
    found: true,
    uid: hit.uid,
    subject: hit.subject,
    from: hit.from_addr,
    attachments: (detail.attachments ?? []).map((file) => ({
      name: file.name,
      mime: file.mime,
      size: file.size,
    })),
    bodyText: (detail.body_text ?? '').slice(0, 80),
  };
})()
