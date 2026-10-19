/**
 * P1 and P2 for the real window (`design/15` §4): cold start to **a row you can read**, and folder
 * switch to the first row of the new folder.
 *
 *     node tools/bench-first-paint.mjs
 *     node tools/bench-first-paint.mjs --port 9222 --switch --rounds 3
 *
 * Why this exists next to `bench-app.ps1`: that script's P1 is "time until the WebView2 debugging port
 * answers", which is not the budget. §4 writes P1 as 冷启动到**列表可见** and P2 as 切文件夹, and the
 * two instrumentation points it names — `t_first_paint`, `t_folder_switch` — do not exist anywhere in
 * this repo. So the two user-facing budgets have never had a ruler, and "首屏快了多少" could only be
 * argued, not measured. This is the ruler: it asks the page when the first row appeared.
 *
 * What counts as "visible": an element with `[data-uid]` inside `#message-list`. A spinner is not a
 * row, and 「这个文件夹里还没有邮件。」 is not a row either — both are honest states and both are
 * reported as such, because "the cache leg painted nothing" is exactly the case the first paint is
 * supposed to fall back on (`src/first-paint.js`).
 *
 * It also records whether the pane title said 「（正在收取…）」 at the moment of the first paint. That
 * is the direct evidence for cache-first behaviour: rows from disk appearing *before* the network
 * answered. A timing number alone cannot tell the two designs apart.
 *
 * The clock is the page's own (`performance.timeOrigin` + `performance.now()`), converted to epoch
 * milliseconds, because the node process connecting over CDP adds its own latency — and if the app
 * painted before the watcher was installed, that shows up as `alreadyPainted: true` instead of a
 * quietly biased number.
 *
 * Zero dependencies: Node 22+ ships a WebSocket, and the CDP client is the same shape as
 * `tools/bench-scroll.mjs` / `tools/inspect-dom.mjs`.
 *
 * Needs the app built with `--remote-debugging-port=9222`, which must be **removed again** afterwards
 * (`tools/check.ps1` fails if it is left in — in either config file).
 */

const argv = process.argv.slice(2);
const flag = (name, fallback) => {
  const eq = argv.find((a) => a.startsWith(`--${name}=`));
  if (eq) return eq.slice(name.length + 3);
  const at = argv.indexOf(`--${name}`);
  if (at >= 0 && at + 1 < argv.length && !argv[at + 1].startsWith('--')) return argv[at + 1];
  return fallback;
};

const PORT = Number(flag('port', '9222'));
/** Seconds to wait for the first row at cold start before giving up. */
const WAIT = Number(flag('wait', '60'));
/** Process start in epoch ms, given by bench-app.ps1. Without it, only page-relative numbers exist. */
const SINCE = Number(flag('since', '0'));
/** Do the folder-switch measurement too. */
const SWITCH = flag('switch', '1') !== '0';
/** Do the cold-start measurement. Off when `bench-app.ps1` runs the switch probe on its own, later. */
const PAINT = flag('paint', '1') !== '0';
const ROUNDS = Number(flag('rounds', '3'));

async function findPageTarget() {
  const response = await fetch(`http://127.0.0.1:${PORT}/json`, { signal: AbortSignal.timeout(2000) });
  const targets = await response.json();
  const isPage = (t) => t.type === 'page' && t.webSocketDebuggerUrl;
  const real = targets.filter((t) => isPage(t) && !/^(about:|edge:\/|chrome:\/newtab)/.test(t.url));
  return real[real.length - 1] ?? null;
}

function connect(ws) {
  let nextId = 1;
  const pending = new Map();
  ws.addEventListener('message', (event) => {
    const message = JSON.parse(event.data);
    const entry = pending.get(message.id);
    if (!entry) return;
    pending.delete(message.id);
    if (message.error) entry.reject(new Error(JSON.stringify(message.error)));
    else entry.resolve(message.result);
  });
  return (method, params = {}) =>
    new Promise((resolve, reject) => {
      const id = nextId++;
      pending.set(id, { resolve, reject });
      ws.send(JSON.stringify({ id, method, params }));
    });
}

/**
 * Runs inside the page.
 *
 * The number comes from the app's own stamp (`window.__easyEmailPaint`, set in `renderMessages` at the
 * line where rows enter the DOM — design/15 §4's `t_first_paint`). Timing from this side would measure
 * when the watcher happened to look: the WebSocket connect alone can be a few hundred milliseconds, and
 * folding that into a 1 500 ms budget would be a measurement of the tool, not of the app.
 *
 * The DOM poll stays as the fallback and the cross-check. If the stamp and "a row is on screen" disagree
 * by more than a frame, one of them is lying — usually because the stamp was removed and the app was
 * never re-measured.
 */
const FIRST_PAINT = `(async () => {
  const wait = ${Math.min(WAIT, 20)} * 1000;
  const started = performance.now();
  const epoch = () => Math.round(performance.timeOrigin + performance.now());
  const stamp = () => window.__easyEmailPaint || {};

  const rows = () => document.querySelectorAll('#message-list [data-uid]').length;
  const paneText = () => (document.querySelector('#message-list')?.textContent || '').trim().slice(0, 40);
  const title = () => document.querySelector('#mailbox-title')?.textContent || '';

  const atConnect = stamp();
  let painted = atConnect.firstRowsAt ? { epoch: atConnect.firstRowsAt, source: 'page-stamp' } : null;
  let detected = rows() > 0 ? { epoch: epoch(), source: 'dom-poll' } : null;
  const deadline = started + wait;
  let paintedSeenAt = painted ? started : null;

  while (performance.now() < deadline) {
    await new Promise((done) => requestAnimationFrame(() => done()));
    if (!painted && stamp().firstRowsAt) {
      painted = { epoch: stamp().firstRowsAt, source: 'page-stamp' };
      paintedSeenAt = performance.now();
    }
    if (!detected && rows() > 0) detected = { epoch: epoch(), source: 'dom-poll' };
    // Give the DOM cross-check half a second to agree, then stop waiting: a folder with no cached row
    // never will, and spinning to the deadline here would only delay the honest answer.
    if (painted && (detected || performance.now() - paintedSeenAt > 500)) break;
  }

  return {
    // Which clock answered: the app's stamp is the budget's own number; the DOM poll is an upper bound.
    source: painted ? painted.source : detected ? 'dom-poll' : null,
    epochAtFirstRow: painted ? painted.epoch : detected ? detected.epoch : null,
    detectedEpoch: detected ? detected.epoch : null,
    // The stamp says a row went up; the watcher says it saw none. Then the stamp is not the mail list.
    stampVsDomMs: painted && detected ? detected.epoch - painted.epoch : null,
    paintsBeforeWatcher: atConnect.paints ?? 0,
    // Whether this binary has the instrumentation at all. Rows on screen with stampPresent false is
    // the one shape that proves it does not.
    stampPresent: stamp().paints !== undefined,
    msSincePageStart:
      painted && detected
        ? Math.round(painted.epoch - performance.timeOrigin)
        : detected
          ? Math.round(detected.epoch - performance.timeOrigin)
          : null,
    rows: rows(),
    title: title(),
    // What is on screen if no row ever arrived: the spinner's sentence, or the empty/failure one.
    pane: rows() === 0 ? paneText() : '',
    refreshingMark: title().includes('正在收取'),
    bannerVisible: !document.querySelector('#sync-banner')?.hidden,
    watcherAttachedMsAfterPageStart: Math.round(started),
    // Where the page-leg time actually went: the stage marks src/app.js pushes on the road to the
    // first row, as absolute epoch ms. Empty means this binary predates them — say so rather than
    // silently reporting a boot with no steps in it.
    bootStagesPresent: Array.isArray(window.__easyEmailBoot?.stages),
    bootStages: (window.__easyEmailBoot?.stages ?? []).slice(0, 60),
    pageOriginEpoch: Math.round(performance.timeOrigin),
    // The same leg split by *whose* clock it is: the boot stage cannot say whether the page waited on the host
    // serving index.html, on twelve sequential script fetches, or on executing app.js, and those have
    // three different fixes. These are the browser's own timing entries, read after the fact, so nothing
    // here perturbs the load — and no rebuild either, since this file is a tool and not embedded JS.
    // (No backticks anywhere below: this whole expression is one template literal.)
    nav: (() => {
      const n = performance.getEntriesByType('navigation')[0];
      if (!n) return null;
      return {
        respEnd: Math.round(n.responseEnd),
        domInteractive: Math.round(n.domInteractive),
        dclStart: Math.round(n.domContentLoadedEventStart),
      };
    })(),
    // Path without the origin, so the table reads "app.js" not "http://tauri.localhost/app.js".
    // Deliberately no regex here: this expression lives inside a template literal, where a backslash in
    // a pattern is eaten by the string and the page receives broken syntax ("Uncaught", 2026-10-20).
    assets: performance
      .getEntriesByType('resource')
      .map((r) => {
        const s = r.name.indexOf('://') >= 0 ? r.name.slice(r.name.indexOf('://') + 3) : r.name;
        const slash = s.indexOf('/');
        const file = (slash >= 0 ? s.slice(slash + 1) : s).split('?')[0];
        return {
          file,
          isJs: file.endsWith('.js'),
          isCss: file.endsWith('.css'),
          start: Math.round(r.startTime),
          respEnd: Math.round(r.responseEnd),
          dur: Math.round(r.duration),
          kb: Math.round(r.encodedBodySize / 1024),
        };
      })
      .filter((r) => r.isJs || r.isCss),
  };
})()`;

/** Click a folder, then measure until its first row is on screen. Returns per-round detail. */
const SWITCH_PROBE = `(async () => {
  const rounds = ${ROUNDS};
  const rows = () => document.querySelectorAll('#message-list [data-uid]').length;
  const firstUid = () => document.querySelector('#message-list [data-uid]')?.dataset?.uid ?? null;
  const title = () => document.querySelector('#mailbox-title')?.textContent || '';
  const boxes = () => [...document.querySelectorAll('#mailbox-list button[data-mailbox]')];
  const stamp = () => window.__easyEmailPaint || {};
  const epoch = () => Math.round(performance.timeOrigin + performance.now());

  // Every round presses a different folder that is not the one on screen, walking the sidebar in order
  // and wrapping around — so --rounds widens the sample instead of re-pressing the same pair.
  const current = document.querySelector('#mailbox-list button.active')?.dataset?.mailbox;
  const candidates = boxes().filter((b) => b.dataset.mailbox !== current);
  if (candidates.length < 2) {
    // Say what the sidebar actually holds: "no two folders to click" can mean the account never got
    // selected, the folder list never loaded, or this account really has one folder — and only the
    // first two are a measurement problem.
    return {
      skipped: '侧栏里没有两个可点的文件夹：文件夹行 ' + boxes().length +
        '，账号行 ' + document.querySelectorAll('#account-list .row').length +
        '，当前 ' + (current || '无') + '，列表区文案「' +
        (document.querySelector('#message-list')?.textContent || '').trim().slice(0, 40) + '」',
    };
  }
  const now = () => performance.now();
  const results = [];

  for (let i = 0; i < rounds; i += 1) {
    const button = candidates[i % candidates.length];
    const before = { uid: firstUid(), rows: rows(), paints: stamp().paints ?? 0 };
    const sourceBefore = stamp().paints === undefined ? 'dom-poll' : 'page-stamp';

    const clickStart = now();
    const clickEpoch = epoch();
    // loadMessages({reset:true}) runs synchronously up to its first await, so by the time the click
    // returns the pane has already been wiped to the spinner: before.rows === 0 is the normal shape.
    button.click();
    const wiped = rows() === 0;

    let painted = null;
    let settled = null;
    const deadline = now() + 60000;
    while (now() < deadline) {
      await new Promise((done) => requestAnimationFrame(() => done()));
      const s = stamp();
      if (!painted && s.paints > before.paints) {
        // The stamp is written on the same line the rows are, so it is the moment, not a sighting of it.
        painted = {
          epoch: s.lastRowsAt,
          ms: Math.round(s.lastRowsAt - clickEpoch),
          title: title(),
          // The title still claims a refresh is running, so these rows came off the disk.
          fromCache: title().includes('正在收取'),
          source: 'page-stamp',
        };
        // Rows that did *not* arrive with that mark can only be the refresh's own answer, so the
        // network leg is over — an unsynced folder never shows the mark at all, and waiting for the
        // title to change would wait forever.
        if (!painted.fromCache) settled = { ms: painted.ms };
      } else if (!painted && rows() > 0 && (wiped || firstUid() !== before.uid)) {
        // Fallback for a build without the stamp: when the watcher sees a row, which is no earlier
        // than when it was painted.
        painted = { epoch: epoch(), ms: null, title: title(), fromCache: title().includes('正在收取'), source: 'dom-poll' };
        painted.ms = Math.round(painted.epoch - clickEpoch);
        if (!painted.fromCache) settled = { ms: painted.ms };
      }
      if (painted && !settled && painted.fromCache && !title().includes('正在收取')) {
        // The refresh leg is over when the title stops claiming it is running. Waiting for a second
        // *paint* would wait forever on a folder whose refresh comes back empty: 「这个文件夹里还没有
        // 邮件。」 is painted by the branch that has no rows to stamp.
        settled = { ms: Math.round(now() - clickStart) };
      }
      if (painted && settled) break;
    }

    results.push({
      folder: button.dataset.mailbox,
      rowsBefore: before.rows,
      wipedToSpinner: wiped,
      measuredBy: painted ? painted.source : sourceBefore === 'dom-poll' ? 'none' : 'dom-poll',
      firstPaintMs: painted ? painted.ms : null,
      firstPaintTitle: painted ? painted.title : null,
      // true = 缓存先画（行的第一次出现早于网络）; false = 第一行就是网络的回答（这条文件夹没缓存）
      cacheLegPainted: painted ? painted.fromCache : null,
      settledMs: settled ? settled.ms : null,
      timedOut: !settled,
    });
    // Let the folder finish loading before the next press, so rounds do not overlap.
    await new Promise((done) => setTimeout(done, 1500));
  }
  return { rounds: results };
})()`;

const sleep = (ms) => new Promise((done) => setTimeout(done, ms));

async function evaluate(send, expression, label) {
  const result = await send('Runtime.evaluate', { expression, awaitPromise: true, returnByValue: true });
  if (result.exceptionDetails) {
    throw new Error(`${label}: ${result.exceptionDetails.text ?? JSON.stringify(result.exceptionDetails)}`);
  }
  return result.result.value;
}

async function openPage(timeoutMs) {
  const connectStarted = Date.now();
  let target = null;
  // Poll for the page target rather than assuming the port is ready: every millisecond of connect
  // latency is a millisecond the first paint could have happened inside.
  while (!target && Date.now() - connectStarted < timeoutMs) {
    target = await findPageTarget().catch(() => null);
    if (!target) await sleep(50);
  }
  if (!target) throw new Error(`no page target on :${PORT} within ${timeoutMs} ms — built with --remote-debugging-port?`);

  const ws = new WebSocket(target.webSocketDebuggerUrl);
  await new Promise((resolve, reject) => {
    ws.addEventListener('open', resolve);
    ws.addEventListener('error', reject);
  });
  return { send: connect(ws), ws };
}

/**
 * Evaluate in the page, re-attaching if the attempt dies.
 *
 * The first run of this sampler failed with `{"code":-32000,"message":"Execution context was
 * destroyed"}`: Tauri's window is still navigating when the watcher attaches, and a navigation takes
 * the pending expression — and the page id with it. So a single-shot evaluate against a starting app
 * cannot work; the retry has to resolve the target again, not just resend.
 *
 * That is safe for what is being measured: the answer is the app's own epoch stamp, taken whenever the
 * paint happened, so a re-attach reports the same moment rather than a later one.
 */
async function evaluateOnPage(expression, label, timeoutMs) {
  const deadline = Date.now() + timeoutMs;
  let lastError = null;
  while (Date.now() < deadline) {
    let ws = null;
    try {
      const page = await openPage(Math.min(5000, deadline - Date.now()));
      ws = page.ws;
      const value = await evaluate(page.send, expression, label);
      ws.close();
      return value;
    } catch (error) {
      if (ws) {
        try {
          ws.close();
        } catch {}
      }
      lastError = error;
      await sleep(300);
    }
  }
  throw new Error(`${label}: ${String(lastError?.message ?? lastError)}（在 ${timeoutMs} ms 内没能完成一次求值）`);
}

async function main() {
  let p1 = { skipped: '未启用（--paint=0）', rows: 0, source: null };
  if (PAINT) p1 = await evaluateOnPage(FIRST_PAINT, 'first paint', WAIT * 1000);
  if (SINCE && p1.epochAtFirstRow) {
    p1.msSinceProcessStart = p1.epochAtFirstRow - SINCE;
  }
  // Which number is the budget's own: §4 counts from the process starting, not from the page loading.
  p1.measuredFrom = p1.msSinceProcessStart !== undefined ? 'processStart' : 'pageStart';
  // The page's stamp makes the number the moment itself. A row only *sighted* from this side is an
  // upper bound — and an upper bound can prove a pass but never a miss, because the connect latency
  // that inflated it is not in the user's budget.
  p1.upperBound = p1.source !== 'page-stamp';

  let p2 = { skipped: '未启用（--switch=0）' };
  if (SWITCH) {
    // Each round can wait a minute for the network leg, so this gets its own, longer deadline.
    p2 = await evaluateOnPage(SWITCH_PROBE, 'folder switch', (ROUNDS * 75 + 60) * 1000);
  }

  const firstRowMs = p1.msSinceProcessStart ?? p1.msSincePageStart;
  const p1Skipped = PAINT === false;
  const verdict = {
    p1,
    p2,
    budgets: { p1Ms: 1500, p2Ms: 200 },
    p1Pass: p1Skipped || (firstRowMs !== null && firstRowMs !== undefined && firstRowMs <= 1500),
    // Every switch round must land inside the budget; one over-budget press is a miss.
    p2Pass:
      Array.isArray(p2.rounds) &&
      p2.rounds.length > 0 &&
      p2.rounds.every((r) => r.firstPaintMs !== null && r.firstPaintMs <= 200),
  };
  console.log(`P1P2 ${JSON.stringify(verdict)}`);

  const misses = [];
  if (!verdict.p1Pass) {
    misses.push(
      `P1 冷启动到第一行 ${firstRowMs ?? `未在 ${WAIT}s 内出现（屏幕上是「${p1.pane}」）`} ms（${p1.measuredFrom} / ${p1.source ?? '无'}），预算 ≤ 1500 ms`
    );
  }
  if (!verdict.p2Pass) misses.push('P2 有切文件夹的回合超过 200 ms 或一行都没出来');
  // Not a miss: a build whose binary predates the stamp in src/app.js still gives a valid ceiling, and
  // saying so is the difference between "P1 = 900 ms" and "P1 ≤ 900 ms".
  if (!p1Skipped && p1.upperBound && p1.rows > 0) {
    console.warn(
      '- 页面里没有 __easyEmailPaint 戳记：这个数字是观察上界（含 CDP 连接延迟），不是第一行出现的时刻。带新的二进制再跑一次。'
    );
  }
  if (misses.length > 0) {
    for (const miss of misses) console.error(`- ${miss}`);
    process.exit(1);
  }
}

main().catch((error) => {
  console.error(String(error?.message ?? error));
  process.exit(1);
});
