/**
 * Remove the rows the other probes leave behind in a real profile.
 *
 * Run it inside the running app:
 *
 *     node tools/inspect-dom.mjs @tools/probe-cleanup.mjs
 *
 * Requires the app to be built with `--remote-debugging-port=9222` in `additionalBrowserArgs`
 * (see the note at the top of inspect-dom.mjs); remove it again afterwards.
 *
 * Why this exists: the other probes are careful not to send anything, and they clean up their own
 * drafts — but a cancelled send is still a row, and a real profile should not keep a fake
 * 「M1a-x 探针」 in its 发件箱 forever. This deletes exactly those rows and nothing else: a row is
 * only touched when it is not open **and** its subject carries the probe marker.
 */
(async () => {
  const invoke = window.__TAURI__.core.invoke;
  const steps = [];

  const ACCOUNT = state.accountId;
  if (!ACCOUNT) return { failed: 'no account configured on this profile' };

  const items = await invoke('list_outbox', { accountId: ACCOUNT });
  const mine = items.filter((item) => {
    if (item.state === 'queued' || item.state === 'sending') return false; // never touch live mail
    try {
      return String(JSON.parse(item.payload_json).subject ?? '').includes('探针');
    } catch {
      return false;
    }
  });

  steps.push({
    step: 'found',
    total: items.length,
    probeRows: mine.map((item) => ({ id: item.id, state: item.state })),
  });

  for (const item of mine) {
    await invoke('discard_outbox', { id: item.id });
  }

  const left = await invoke('list_outbox', { accountId: ACCOUNT });
  steps.push({
    step: 'after',
    total: left.length,
    remainingProbeRows: left
      .filter((item) => {
        try {
          return String(JSON.parse(item.payload_json).subject ?? '').includes('探针');
        } catch {
          return false;
        }
      })
      .map((item) => item.id),
  });

  const remaining = steps[1].remainingProbeRows;
  return {
    passed: remaining.length === 0,
    failures: remaining.length === 0 ? [] : ['probe rows left behind'],
    steps,
  };
})()
