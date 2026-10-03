/**
 * Does the sync-window setting survive the round trip, and did the cold start top the list up?
 *
 * Run it inside the running app:
 *
 *     node tools/inspect-dom.mjs @tools/probe-sync-window.mjs
 *
 * Requires the app to be built with `--remote-debugging-port=9222` in `additionalBrowserArgs`
 * (see the note at the top of inspect-dom.mjs); remove it again afterwards.
 *
 * Why this exists: `SyncWindow` is a *tagged* enum whose variants include a unit variant (`all`) and
 * newtype variants (`years(2)`), serialised with `tag` + `content`. serde rejects some of those
 * combinations at **run time**, not at compile time, and the frontend has to produce exactly the
 * shape serde accepts. That makes all four kinds worth asking for out loud, not just the default.
 *
 * The probe **restores the original value** before it finishes: it is writing to the real profile's
 * preference, and a probe may not leave the user's setting changed.
 */
(async () => {
  const invoke = window.__TAURI__.core.invoke;
  const steps = [];
  const note = (step, detail) => steps.push({ step, ...detail });
  const wait = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

  const original = await invoke('get_sync_window');
  note('initial', {
    value: original,
    label: windowLabel(original),
    row: document.getElementById('sync-window-value').textContent,
  });

  // The cold start runs inside `loadMailboxes`, which `boot` has already awaited by now. The set is
  // the renderer's own record of "this account has been topped up in this session".
  note('cold_start', {
    accounts: [...backfilledAccounts],
    current: state.accountId,
    ran: Boolean(state.accountId) && backfilledAccounts.has(state.accountId),
  });

  // ---------------------------------------------------------------- every kind, both ways
  const kinds = [
    { kind: 'years', value: 3 },
    { kind: 'count', value: 1234 },
    { kind: 'all' },
    { kind: 'on_demand' },
  ];
  const roundTrips = [];
  for (const window of kinds) {
    await invoke('set_sync_window', { window });
    roundTrips.push({ sent: window, got: await invoke('get_sync_window') });
  }
  note('round_trip', { roundTrips });

  // ---------------------------------------------------------------- the picker itself
  await invoke('set_sync_window', { window: original });
  state.syncWindow = original;
  openSyncWindowPicker();
  const modal = document.querySelector('#modal-root .modal');
  const checkedInitially = modal.querySelector('input[name="sync-window"]:checked')?.value ?? null;

  modal.querySelector('input[value="years"]').checked = true;
  modal.querySelector('#sync-years').value = '5';
  modal.querySelector('[data-role="confirm"]').click();
  await wait(700);

  const afterPick = await invoke('get_sync_window');
  note('picker', {
    checkedInitially,
    expectedInitially: original.kind,
    afterPick,
    row: document.getElementById('sync-window-value').textContent,
    modalClosed: document.querySelector('#modal-root .modal') === null,
  });

  // Put the user's setting back exactly as it was.
  await invoke('set_sync_window', { window: original });
  note('restored', { value: await invoke('get_sync_window'), expected: original });

  const same = (a, b) => JSON.stringify(a) === JSON.stringify(b);
  const failures = steps.flatMap((entry) => {
    const bad = [];
    if (entry.step === 'initial' && (!entry.label || !entry.row)) bad.push('initial');
    if (entry.step === 'cold_start' && !entry.ran) bad.push('cold start did not run');
    if (entry.step === 'round_trip') {
      for (const trip of entry.roundTrips) {
        if (!same(trip.sent, trip.got)) bad.push(`round trip ${JSON.stringify(trip.sent)}`);
      }
    }
    if (entry.step === 'picker') {
      if (entry.expectedInitially !== entry.checkedInitially) bad.push('picker did not preselect');
      if (!same(entry.afterPick, { kind: 'years', value: 5 })) bad.push('picker did not save');
      if (!entry.modalClosed) bad.push('picker modal stayed open');
      if (!/5/.test(entry.row ?? '')) bad.push('sidebar row not updated');
    }
    if (entry.step === 'restored' && !same(entry.value, entry.expected)) bad.push('not restored');
    return bad;
  });

  return { passed: failures.length === 0, failures, steps };
})()
