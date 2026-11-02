/**
 * Does 正文不落盘 actually stop bodies from reaching the disk — on the user's real profile?
 *
 * Run it inside the running app, in two phases, with the byte check in between:
 *
 *     python tools/probe-body-storage-report.py plan
 *     node tools/inspect-dom.mjs @target/probe-plan.js
 *     node tools/inspect-dom.mjs @tools/probe-body-storage.mjs > target/probe-online.json
 *     python tools/probe-body-storage-report.py verify-online target/probe-online.json
 *     (restart the app — the startup path has to re-apply the stored mode)
 *     python tools/probe-body-storage-report.py pair target/probe-online.json
 *     node tools/inspect-dom.mjs @target/probe-plan.js
 *     node tools/inspect-dom.mjs @tools/probe-body-storage.mjs > target/probe-cached.json
 *     python tools/probe-body-storage-report.py verify-cached target/probe-cached.json
 *
 * The probe works out which arm to run from the mode it finds: 缓存在本地 means it is doing the
 * online arm, 正文不落盘 means the cached one.
 *
 * It needs the debugging port (see the note at the top of inspect-dom.mjs — it goes in
 * `tauri.windows.conf.json`, not `tauri.conf.json`); remove it again afterwards.
 *
 * Two things made the first two runs of this prove nothing, and both are fixed here rather than
 * explained away:
 *
 *   - **The population.** The messages picked were already cached, so the stored count sat at the
 *     same number in both arms. Only mail with **no `bodies` row** can tell the modes apart, so the
 *     candidates come from the database (`window.__bodyPlan.pool`), and the arm stops as soon as
 *     enough of them actually handed over a body — mail the server refuses to give back is not
 *     evidence of anything.
 *   - **The measurement.** The stored count is still reported, but it is not the verdict: the
 *     background backfill is writing bodies throughout, so a delta of 0 can happen with the mode off
 *     and a delta of 4 can happen with it on. The verdict is the body text **as bytes in
 *     cache.db / cache.db-wal**, read immediately after each arm by the Python half.
 *
 * `plan` builds the pool; `pair` rebuilds it from the online arm's report so the cached arm opens
 * **the same messages** — absent from the file after one read, present after the other.
 *
 * Each phase also drives the switch the way the user does — by clicking the panel — and checks what
 * the panel says on the way in *and* on the way out, because 清空已下载正文 is only offered on the
 * way into 不落盘.
 */
(async () => {
  const invoke = window.__TAURI__.core.invoke;
  const wait = (ms) => new Promise((resolve) => setTimeout(resolve, ms));
  const plan = window.__bodyPlan;
  if (!plan) {
    return { failed: 'window.__bodyPlan is not set — run the plan step and @target/probe-plan.js first' };
  }
  const ACCOUNT = state.accountId;
  if (!ACCOUNT) return { failed: 'no account configured on this profile' };

  const before = await invoke('get_body_storage');
  const phase = before.mode === 'cached' ? 'online' : 'cached';
  const pool = plan.pool;
  if (!Array.isArray(pool) || pool.length === 0) return { failed: 'the plan has no pool' };
  if (phase === 'online' && before.mode !== plan.restoreMode) {
    return { failed: `expected to start from ${plan.restoreMode}, found ${before.mode}` };
  }

  // Snippets are what the Python half looks for in the file. They have to survive the trip through
  // serde_json, so: one line of the plain-text body, no quote, no backslash, no control character,
  // no newline. Taken from away from the top of the body, where boilerplate repeats, and required to
  // carry real words — a run of dashes is body text too, and it matches half the database.
  const snippetOf = (detail) => {
    const lines = String(detail.body_text || '').split(/[\r\n\u2028\u2029]+/);
    for (const line of lines) {
      if (line.length < 56) continue;
      const window = line.slice(30, 56);
      if (/["\\]/.test(window) || /[\u0000-\u001f\u007f\u2028\u2029]/.test(window)) continue;
      const words = (window.match(/[\w\u3400-\u9fff\uf900-\ufaff]/g) || []).length;
      if (words < 12) continue;
      return window;
    }
    return null;
  };

  /* The panel, clicked. Round 122: 设置 is one sidebar row now, so the storage entry is one click in. */
  document.querySelector('#btn-settings').click();
  await wait(300);
  document.querySelector('#btn-storage').click();
  await wait(1500);
  const modal = document.querySelector('#modal-root .modal');
  if (!modal) return { failed: 'the storage panel did not open' };
  const textOf = (selector) => {
    const node = modal.querySelector(selector);
    return node ? node.textContent.replace(/\s+/g, ' ').trim() : null;
  };
  const pick = (value) => modal.querySelector(`input[name="body-storage"][value="${value}"]`);
  const purge = modal.querySelector('#body-purge');
  const checked = (value) => pick(value)?.checked === true;
  const diskLine = textOf('#disk-protection-line');
  const bodyLine = textOf('#body-storage-line');
  const panelShows = {
    cachedChecked: checked('cached'),
    onlineChecked: checked('online'),
    purgeEnabledAsFound: !purge.disabled,
    purgeLabelAsFound: textOf('#body-purge-label'),
  };

  pick(phase).click();
  await wait(200);
  const purgeEnabledAfterPicking = !purge.disabled;
  purge.checked = false;
  modal.querySelector('#btn-body-storage-save').click();
  await wait(2000);
  const bodyLineAfter = textOf('#body-storage-line');
  const toastText = (document.querySelector('.toast') || {}).textContent || null;
  modal.querySelector('[data-role="close"]').click();
  await wait(300);

  const afterSwitch = await invoke('get_body_storage');
  if (afterSwitch.mode !== phase) {
    return { failed: `the panel did not switch the mode: asked for ${phase}, got ${afterSwitch.mode}` };
  }

  /* The arm itself: open candidates until enough of them hand over usable body text. */
  const opened = [];
  const usable = () => opened.filter((entry) => entry.snippet).length;
  for (const item of pool) {
    if (usable() >= plan.need && phase === 'online') break;
    const entry = { accountId: item.accountId, folder: item.folder, uid: item.uid, snippet: null };
    try {
      const detail = await invoke('fetch_message', {
        accountId: item.accountId,
        mailbox: item.folder,
        uid: item.uid,
      });
      entry.chars =
        (detail.body_text || '').length + (detail.body_html || '').length;
      entry.attachments = (detail.attachments || []).length;
      entry.snippet = snippetOf(detail);
    } catch (error) {
      entry.error = String(error);
    }
    opened.push(entry);
  }
  const afterArm = await invoke('get_body_storage');

  /* On the way out of the cached arm — and only there — put the profile back the way it was found. */
  let restored = afterArm;
  if (phase === 'cached') {
    await invoke('set_body_storage', { mode: plan.restoreMode, purgeExisting: false });
    restored = await invoke('get_body_storage');
  }

  const readable = opened.filter((entry) => entry.chars > 0).length;
  const withSnippet = usable();
  const backfillNoise = afterArm.stored_bodies - afterSwitch.stored_bodies;

  return {
    phase,
    passed:
      panelShows[phase === 'online' ? 'cachedChecked' : 'onlineChecked'] &&
      panelShows.purgeEnabledAsFound === (phase === 'cached') &&
      purgeEnabledAfterPicking === (phase === 'online') &&
      afterSwitch.mode === phase &&
      new RegExp(phase === 'online' ? '正文不落盘' : '正文缓存在本地').test(bodyLineAfter) &&
      readable === withSnippet &&
      withSnippet >= 2 &&
      diskLine !== null &&
      (phase === 'online' ? backfillNoise === 0 : true),
    notes: {
      modeAtStart: before.mode,
      storedAtStart: before.stored_bodies,
      storedAfterSwitch: afterSwitch.stored_bodies,
      storedAfterArm: afterArm.stored_bodies,
      // The count is context, not the verdict — this is the backfill moving underneath the probe.
      countDeltaDuringArm: backfillNoise,
      readable: `${readable}/${opened.length}`,
      withSnippet: `${withSnippet}/${opened.length}`,
      diskLine,
      toastText,
      panelShows,
      bodyLine,
      bodyLineAfter,
      purgeEnabledAfterPicking,
      restored: restored.mode,
      folders: [...new Set(opened.map((entry) => entry.folder))],
    },
    opened,
  };
})();
