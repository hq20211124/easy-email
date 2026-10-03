/**
 * Real-machine probe ③: does closing the composer leave a copy in the server's Drafts folder?
 *
 * `design/07` §5.1: the mirror is best-effort and fires on a 30-second timer *or* on blur/close, and it
 * writes an APPEND without `\Seen` so the copy does not mark the user's own draft as read. What this
 * probe checks is the part only a real account can answer: that a draft written here shows up in the
 * server's Drafts folder, and that discarding the draft removes it again.
 *
 * Run it with the app open, then check `draft_mirrors` in user.db and the mailbox itself.
 */
(async () => {
  const invoke = window.__TAURI__.core.invoke;
  const wait = (ms) => new Promise((resolve) => setTimeout(resolve, ms));
  const phase = globalThis.__mirrorProbePhase ?? 'write';

  if (phase === 'write') {
    await openComposer();
    await wait(300);
    const modal = document.querySelector('#modal-root .modal');
    modal.querySelector('#compose-to').value = 'me@example.com';
    modal.querySelector('#compose-subject').value = '草稿镜像探针';
    modal.querySelector('#compose-body').value = '这条只写进草稿，不会发出去。';
    modal.querySelector('#compose-body').dispatchEvent(new Event('input'));

    // The 2 s autosave has to land before closing, or the mirror has nothing to mirror.
    await wait(2600);
    const draftId = state.composerDraftId ?? null;

    // Closing is the trigger the design relies on: the moment the user walks away is exactly when the
    // copy has to exist elsewhere.
    closeModal();
    await wait(4000);

    globalThis.__mirrorProbePhase = 'report';
    globalThis.__mirrorProbeDraftId = draftId;
    return { stage: 'written', draftId, drafts: state.drafts.length };
  }

  if (phase === 'report') {
    const mirrors = await invoke('draft_mirror', { draftId: globalThis.__mirrorProbeDraftId }).catch(
      (error) => ({ error: String(error) })
    );
    return {
      stage: 'reported',
      draftId: globalThis.__mirrorProbeDraftId,
      mirror: mirrors,
    };
  }

  // `discard`: dropping the draft must take the server copy with it (design/07 §5.1), or the mailbox
  // slowly fills with copies of drafts the user thought they deleted.
  const row = document.querySelector(
    `#message-list [data-draft-id="${globalThis.__mirrorProbeDraftId}"]`
  );
  if (!row) return { stage: 'discard', discarded: false, reason: 'the draft row is not in the list' };
  row.click();
  await wait(600);
  const modal = document.querySelector('#modal-root .modal');
  modal.querySelector('[data-role="discard"]')?.click();
  await wait(3000);
  const after = await invoke('draft_mirror', { draftId: globalThis.__mirrorProbeDraftId }).catch(
    (error) => ({ error: String(error) })
  );
  return { stage: 'discarded', mirrorAfterDiscard: after };
})()
