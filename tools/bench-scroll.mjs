/**
 * P3 sampler for the message list (`design/15` §4: 60 fps, ≤ 40 rows of DOM change per frame;
 * §6: virtualise the list, fixed row height, commit on `requestAnimationFrame`).
 *
 * It is a bench, not a probe: it drives a scroll sweep inside the running window and then **judges**
 * the result against both budgets, exiting non-zero when they are missed. A tool that only prints
 * numbers is how the P3 budget stayed unmeasured for this long — `bench-app.ps1` mentioned scrolling
 * in its header and never did it.
 *
 * How it counts, and why it has to count inside the page:
 *   * frames — `requestAnimationFrame` timestamps. That is the only definition of "a frame" the
 *     renderer agrees with; a timer at 16 ms would report its own cadence, not the screen's.
 *   * DOM change per frame — a `MutationObserver` on the list subtree. The number of rows a scroll
 *     *changed* exists only where the changes happen. Added/removed element nodes are counted as an
 *     approximation of "rows", which is what the budget is phrased in.
 *
 * Zero dependencies: Node 22+ ships a WebSocket, and the minimal CDP client is copied from
 * `tools/inspect-dom.mjs` on purpose (same shape, no npm install in a repo whose frontend has no
 * build step at all).
 *
 * Usage:
 *     node tools/bench-scroll.mjs                  # the real app on :9222 (needs the CDP build)
 *     node tools/bench-scroll.mjs --port 9333      # another port
 *     node tools/bench-scroll.mjs --url <page>     # any page with a #message-list (the fixture)
 *     node tools/bench-scroll.mjs --seconds 6 --step 800
 *     node tools/bench-scroll.mjs --budget-rows 40 --budget-fps 60
 *
 * Requires the app to be built with `--remote-debugging-port=9222` in `additionalBrowserArgs` and
 * that flag to be **removed again** afterwards — any local process can drive the webview through
 * that port, and `tools/check.ps1` fails a build that leaves it in.
 */

const argv = process.argv.slice(2);
/** `--name=value` and `--name value` both work. The first version accepted only the first form and
 *  silently fell back to the default port when given the second — a bench that quietly measures the
 *  wrong thing is worse than one that refuses to run. */
const flag = (name, fallback) => {
  const eq = argv.find((a) => a.startsWith(`--${name}=`));
  if (eq) return eq.slice(name.length + 3);
  const at = argv.indexOf(`--${name}`);
  if (at >= 0 && at + 1 < argv.length && !argv[at + 1].startsWith('--')) return argv[at + 1];
  return fallback;
};
const has = (name) => argv.includes(`--${name}`);

const PORT = Number(flag('port', '9222'));
const URL_ = flag('url', '');
const SECONDS = Number(flag('seconds', '5'));
const STEP = Number(flag('step', '600'));
const BUDGET_ROWS = Number(flag('budget-rows', '40'));
const BUDGET_FPS = Number(flag('budget-fps', '60'));
/**
 * Slack on the fps judgement, and the reason it exists: rAF ticks at ~16.7 ms, so a run that is
 * perfectly smooth measures 59.9 or 60.4 fps depending on where the sweep started. The first version
 * had no slack and reported the *good* fixture as a failure, which is how a bench teaches people to
 * ignore it. Frame time is judged too, because that is the signal that survives a vsync cap.
 */
const FPS_TOLERANCE = Number(flag('fps-tolerance', '0.05'));
const FRAME_SLACK = Number(flag('frame-slack', '1.15'));
/** Above this many rows in the DOM at once, the sample says so instead of implying 100k. */
const BIG_SHAPE = 2000;

async function findPageTarget() {
  // With a timeout, and it matters: a stale browser squatting on the port made the first version of
  // this sampler hang forever instead of failing, and a bench that hangs is indistinguishable from a
  // bench that is still thinking.
  const response = await fetch(`http://127.0.0.1:${PORT}/json`, {
    signal: AbortSignal.timeout(Number(flag('connect-timeout', '5000'))),
  });
  const targets = await response.json();

  const isPage = (t) => t.type === 'page' && t.webSocketDebuggerUrl;
  const isReal = (t) => isPage(t) && !/^(about:|edge:\/|chrome:\/newtab)/.test(t.url);
  // Blank pages are filtered out and the **newest** real page wins: a browser opened with a URL also
  // has a blank tab, and the first version of this sampler took whichever came first in the list —
  // which measured an empty page (rows=0, everything passing) instead of the list under test.
  const real = targets.filter(isReal);
  if (URL_) {
    const wanted = real.find((t) => t.url.includes(URL_));
    if (wanted) return wanted;
    throw new Error(`no page on :${PORT} whose URL contains ${JSON.stringify(URL_)}`);
  }
  const page = real[real.length - 1];
  if (!page) {
    throw new Error(
      `no real page target on :${PORT} (saw ${targets.length} target(s)). Run the app built with --remote-debugging-port, or point --url at a page.`
    );
  }
  return page;
}

/** Minimal CDP client: one socket, one pending promise per request id. */
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
 * Runs inside the page. Everything the sampler needs is measured here, because frames and DOM
 * mutations are page-local facts — asking from outside can only see the result, not the cost.
 */
const SWEEP = `(async () => {
  const list = document.querySelector('#message-list') || document.scrollingElement || document.body;
  const rowSelector = list.querySelector('[data-uid]') ? '[data-uid]' : '.message-row';
  const rowsAtRest = () => document.querySelectorAll(rowSelector).length;
  const scrollHeight = () => Math.max(list.scrollHeight, document.body.scrollHeight);

  const before = { rows: rowsAtRest(), height: scrollHeight() };

  // One observer for the whole sweep. childList + characterData is what a row swap does; attributes
  // are excluded because a class or style change is not a row.
  let mutations = 0;
  const observer = new MutationObserver((records) => {
    for (const record of records) {
      mutations += record.addedNodes.length + record.removedNodes.length;
    }
  });
  observer.observe(list, { childList: true, subtree: true, characterData: true });

  const frames = [];
  const perFrameRows = [];
  const started = performance.now();
  let previous = started;

  await new Promise((done) => {
    const sweep = (now) => {
      frames.push(now - previous);
      previous = now;
      // Counted *before* scrolling: what this frame is asked to change, not what it finished with.
      perFrameRows.push(mutations);
      mutations = 0;

      // A synthetic scroll, not a wheel event: reproducible, and independent of the platform's
      // scroll settings. Bounce at the ends so the sweep keeps finding new rows to move.
      const atEnd = list.scrollTop + list.clientHeight >= list.scrollHeight - 1;
      list.scrollTop = atEnd ? 0 : list.scrollTop + ${STEP};

      if (now - started >= ${SECONDS} * 1000) {
        done();
        return;
      }
      requestAnimationFrame(sweep);
    };
    requestAnimationFrame(sweep);
  });

  observer.disconnect();
  const elapsed = performance.now() - started;
  const sorted = [...frames].sort((a, b) => a - b);
  const at = (q) => sorted[Math.min(sorted.length - 1, Math.floor(sorted.length * q))] ?? 0;

  return {
    selector: list.id ? '#' + list.id : list.tagName.toLowerCase(),
    rowsBefore: before.rows,
    scrollableHeight: scrollHeight(),
    frames: frames.length,
    elapsedMs: Math.round(elapsed),
    fps: Math.round((frames.length / elapsed) * 1000 * 10) / 10,
    frameMsP50: Math.round(at(0.5) * 10) / 10,
    frameMsP95: Math.round(at(0.95) * 10) / 10,
    frameMsMax: Math.round(Math.max(...frames, 0) * 10) / 10,
    rowsChangedPerFrameMax: perFrameRows.length ? Math.max(...perFrameRows) : 0,
    rowsInDomAtEnd: rowsAtRest(),
  };
})()`;

async function main() {
  const target = await findPageTarget();
  const ws = new WebSocket(target.webSocketDebuggerUrl);
  await new Promise((resolve, reject) => {
    ws.addEventListener('open', resolve);
    ws.addEventListener('error', reject);
  });
  const send = connect(ws);

  const result = await send('Runtime.evaluate', {
    expression: SWEEP,
    awaitPromise: true,
    returnByValue: true,
  });
  ws.close();

  if (result.exceptionDetails) {
    throw new Error(result.exceptionDetails.text ?? 'the sweep threw in the page');
  }
  const m = result.result.value ?? {};
  const payload = {
    list: m.selector,
    rows: m.rowsInDomAtEnd,
    rowsBefore: m.rowsBefore,
    fps: m.fps,
    frameMsP50: m.frameMsP50,
    frameMsP95: m.frameMsP95,
    frameMsMax: m.frameMsMax,
    rowsChangedPerFrameMax: m.rowsChangedPerFrameMax,
    budgetFps: BUDGET_FPS,
    budgetRows: BUDGET_ROWS,
  };

  if (has('json')) {
    // Machine-readable, for callers that need the numbers rather than a report (`bench-app.ps1`
    // records them into its history file). In this mode the human line goes to stderr, so running
    // it by hand still shows something and piping stdout stays clean.
    console.error(`P3 ${JSON.stringify(payload)}`);
  } else {
    console.log(`list      : ${m.selector}  rows=${m.rowsBefore}  scrollable=${m.scrollableHeight}px`);
    console.log(`sweep     : ${SECONDS}s requested, ${m.frames} frames in ${m.elapsedMs} ms`);
    console.log(
      `fps       : ${m.fps}                (budget >= ${BUDGET_FPS}, judged with ${Math.round(FPS_TOLERANCE * 100)}% slack)`
    );
    console.log(`frame ms  : P50 ${m.frameMsP50}  P95 ${m.frameMsP95}  max ${m.frameMsMax}`);
    console.log(
      `rows/frame: max ${m.rowsChangedPerFrameMax}   (budget <= ${BUDGET_ROWS})`
    );
    console.log(`rows in DOM at end: ${m.rowsInDomAtEnd}`);
    // Honest about what fps can and cannot mean here: a compositor that ticks at 60 Hz caps the
    // number, so frame time and DOM churn are the two signals that travel between machines.
    console.log(
      'note      : fps is capped by the compositor in a headless run; judge this by frame time and rows/frame.'
    );
  }

  // The shape caveat is part of the measurement: fps on 190 rows says nothing about fps on 100k,
  // and a reader who only sees "60 fps" would take it as the budget's answer.
  if (m.rowsInDomAtEnd > BIG_SHAPE) {
    console.log(
      `shape     : ${m.rowsInDomAtEnd} rows in the DOM — beyond the list sizes this sampler was tuned for; treat fps as a relative number.`
    );
  } else if (!has('json')) {
    console.log(
      `shape     : ${m.rowsInDomAtEnd} rows in the DOM — NOT the budget's 100k shape; compare like with like.`
    );
  }

  const problems = [];
  const fpsFloor = BUDGET_FPS * (1 - FPS_TOLERANCE);
  if (m.fps < fpsFloor) problems.push(`fps ${m.fps} < ${Math.round(fpsFloor)} (${BUDGET_FPS} less ${Math.round(FPS_TOLERANCE * 100)}% slack)`);
  const frameBudget = (1000 / BUDGET_FPS) * FRAME_SLACK;
  if (m.frameMsP95 > frameBudget)
    problems.push(`frame time P95 ${m.frameMsP95} ms > ${Math.round(frameBudget)} ms`);
  if (m.rowsChangedPerFrameMax > BUDGET_ROWS)
    problems.push(`${m.rowsChangedPerFrameMax} rows changed in one frame > ${BUDGET_ROWS}`);
  if (problems.length) {
    console.error(`P3 MISSED: ${problems.join('; ')}`);
    process.exit(1);
  }
  console.log('P3 ok (at this shape)');
}

if (has('help')) {
  console.log('usage: node tools/bench-scroll.mjs [--port 9222] [--url <substring>] [--seconds 5] [--step 600]');
  process.exit(0);
}

await main();
