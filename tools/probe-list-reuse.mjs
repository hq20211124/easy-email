/**
 * Does the windowed mail list still show the **right rows** after scrolling reuses them?
 *
 *     node tools/probe-list-reuse.mjs                  # the real app on :9222 (needs the CDP build)
 *     node tools/probe-list-reuse.mjs --port 9333 --json
 *
 * `tools/bench-scroll.mjs` measures how *cheap* a scroll is (P3's ≤ 40 rows of DOM change per frame).
 * This probe asks the other question, the one a perf number cannot answer: since 2026-10-19 (round 99)
 * a scroll keeps the rows that are still on screen instead of rebuilding them, so the failure mode is
 * no longer "too slow" but **a row that is in the wrong place, missing, or duplicated** — and every one
 * of those looks perfect in a screenshot taken after the scroll stops.
 *
 * What it asserts, all inside the page, in one `Runtime.evaluate`:
 *   1. a row two consecutive windows both hold is **the same DOM object** in both — not the same mail
 *      reprinted by a rebuild. This is the claim the change made, and a screenshot cannot tell the two
 *      apart; checked between neighbouring windows, because a window that slides off the list *must*
 *      drop its rows, so identity across a whole top-to-bottom round trip is not a thing that exists;
 *   2. where two windows share rows, the shared rows come in the **same order** in both — an off-by-one
 *      at either end of the patch shows up here or in 3, never in a count;
 *   3. no row vanishes mid-sweep and reappears later (the window is one contiguous slice of one order),
 *      no row appears twice in one window, and every row carries a `data-uid`;
 *   4. the rows the sweep visited agree with the sizer: `scrollHeight / row height` rows exist, and a
 *      sweep that reached the bottom saw that many distinct ones — the arithmetic that lets a 100k list
 *      scroll is checked against the DOM actually on screen;
 *   5. scrolling back to the top reproduces the first window's rows in the first window's order;
 *   6. a row from 1 — kept, not rebuilt — still opens its message when clicked, and still highlights;
 *   7. its fold caret still toggles: the child rows enter and leave the window.
 *
 * Checks 6 and 7 only touch rows that are already read; opening an unread one would call `mark_seen`,
 * and a read-only probe has no business changing the user's mailbox or the server behind it.
 *
 * Exits non-zero on any of them. Zero dependencies, same minimal CDP client as `bench-scroll.mjs`.
 *
 * Exits non-zero on any of them. Zero dependencies, same minimal CDP client as `bench-scroll.mjs`.
 */

const argv = process.argv.slice(2);
const flag = (name, fallback) => {
  const eq = argv.find((a) => a.startsWith(`--${name}=`));
  if (eq) return eq.slice(name.length + 3);
  const at = argv.indexOf(`--${name}`);
  if (at >= 0 && at + 1 < argv.length && !argv[at + 1].startsWith('--')) return argv[at + 1];
  return fallback;
};
const has = (name) => argv.includes(`--${name}`);

const PORT = Number(flag('port', '9222'));
const STEPS = Number(flag('steps', '12'));

async function findPageTarget() {
  const response = await fetch(`http://127.0.0.1:${PORT}/json`, {
    signal: AbortSignal.timeout(Number(flag('connect-timeout', '5000'))),
  });
  const targets = await response.json();
  const isPage = (t) => t.type === 'page' && t.webSocketDebuggerUrl;
  const isReal = (t) => isPage(t) && !/^(about:|edge:\/|chrome:\/newtab)/.test(t.url);
  const real = targets.filter(isReal);
  const page = real[real.length - 1];
  if (!page) {
    throw new Error(
      `no real page target on :${PORT} (saw ${targets.length} target(s)). Run the app built with --remote-debugging-port, or point the probe at nothing else.`
    );
  }
  return page;
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
 * Runs inside the page — the DOM order and the click handlers only exist there.
 * A real function, stringified with `toString()`: writing it as a template literal instead once
 * swallowed the outer string with a backtick in a comment, and `node --check` cannot see inside a
 * string, so the page code was not code at all.
 */
async function probePage(steps) {
  const problems = [];
  const list = document.querySelector('#message-list');
  const win = () => document.querySelector('.vlist-window');
  const settle = () => new Promise((done) => requestAnimationFrame(() => requestAnimationFrame(() => requestAnimationFrame(done))));
  const snapshot = () => {
    const kids = [...win().children];
    return { uids: kids.map((el) => el.dataset.uid ?? null), els: kids };
  };

  if (!list || !win()) return { problems: ['#message-list or .vlist-window is missing — the mail list is not windowed right now'] };
  if (win().children.length === 0) return { problems: ['the window is empty: nothing to check (is this folder showing rows at all?)'] };

  const first = snapshot();
  const rowHeight = first.els[0].offsetHeight;
  if (!(rowHeight > 0)) problems.push('a row has zero height: the fixed-height precondition of windowing is gone');
  const totalRows = Math.round(list.scrollHeight / rowHeight);

  // Two facts a sliding window must never break, checked between consecutive windows rather than
  // against a copy of the app's own order (the point is that the DOM contradicts itself, which needs
  // nothing but the two windows on screen):
  //   * where two windows share rows, the shared rows are **the same rows in the same order**;
  //   * every row of the list is visited exactly once over a sweep from the top to the bottom.
  // A `prepend`/`append` off by one, or the wrong range from `planWindowPatch`, breaks one of them.
  const visited = new Set(first.uids);
  // Half a viewport: small enough that consecutive windows must share rows (that overlap is the only
  // place the reuse can be caught lying), and repeated often enough to reach the real bottom.
  const maxScroll = Math.max(0, list.scrollHeight - list.clientHeight);
  const stepPx = Math.max(1, Math.floor(list.clientHeight / 2));
  const positions = [];
  for (let top = stepPx; top < maxScroll; top += stepPx) positions.push(top);
  if (maxScroll > 0) positions.push(maxScroll);
  const sweep = positions.slice(0, steps);
  // A list longer than `steps` half-viewports cannot be swept to the bottom; keep the check honest by
  // naming the part that was covered instead of failing a count the sweep never reached.
  const reachedEnd = sweep.length === 0 ? maxScroll === 0 : sweep[sweep.length - 1] === maxScroll;

  let previous = first;
  let overlapping = 0;
  let reusedMax = 0;
  let lastSurvivors = [];
  const reusedPerStep = [];
  // A forward sweep must see each row once and then never again; `absent` counts the steps a row was
  // gone for before it came back, which is what an off-by-one at either end of the patch produces.
  const lastSeen = new Map(first.uids.map((uid) => [uid, 0]));
  let cameBack = 0;
  for (const [step, top] of sweep.entries()) {
    list.scrollTop = top;
    await settle();
    const snap = snapshot();
    if (snap.uids.some((uid) => !uid)) problems.push('a row in the window has no data-uid');
    const dupe = snap.uids.find((uid, i) => snap.uids.indexOf(uid) !== i);
    if (dupe) problems.push('the same row is in the window twice: ' + dupe);

    // Compare by row identity rather than by position: a window that slides in from the top and one
    // that grows at the bottom have no shared coordinate, but the rows they both hold are the same
    // rows — and "the same row" here has to mean the same DOM object, because that is the whole claim
    // the 2026-10-19 change made. A rebuild that happens to print the same mail passes an order check
    // and fails this one.
    const previousByUid = new Map(previous.uids.map((uid, i) => [uid, previous.els[i]]));
    const snapByUid = new Map(snap.uids.map((uid, i) => [uid, snap.els[i]]));
    const sharedInSnap = snap.uids.filter((uid) => previousByUid.has(uid));
    const sharedInPrevious = previous.uids.filter((uid) => snapByUid.has(uid));
    lastSurvivors = snap.uids
      .map((uid, i) => ({ uid, el: snap.els[i], prev: previousByUid.get(uid) }))
      .filter((row) => row.prev === row.el)
      .map((row) => row.el);
    if (sharedInSnap.length > 0) {
      overlapping += 1;
      reusedMax = Math.max(reusedMax, sharedInSnap.length);
      if (sharedInSnap.join(',') !== sharedInPrevious.join(',')) {
        problems.push(`scrollTop=${top} shows the rows shared with the window before it in a different order`);
      }
      for (const uid of sharedInSnap) {
        if (previousByUid.get(uid) !== snapByUid.get(uid)) {
          problems.push(`scrollTop=${top} rebuilt row ${uid}, which the previous window already held — reuse did not happen`);
          break;
        }
      }
    }
    for (const uid of snap.uids) {
      const before = lastSeen.get(uid);
      if (before !== undefined && step - before > 1) cameBack += 1;
      lastSeen.set(uid, step + 1);
    }
    reusedPerStep.push(lastSurvivors.length);
    snap.uids.forEach((uid) => visited.add(uid));
    previous = snap;
  }

  if (overlapping === 0) problems.push('no two windows shared a row — the steps are bigger than the window, so nothing here was tested');
  if (reusedMax === 0) problems.push('every window after the first was built from scratch: the list still calls replaceChildren on scroll');
  if (cameBack > 0) problems.push(cameBack + ' row(s) disappeared mid-sweep and came back later — the window is not one contiguous slice');
  if (reachedEnd && visited.size !== totalRows) {
    problems.push(`the sweep saw ${visited.size} distinct rows but the sizer says ${totalRows} exist`);
  }

  // The two things a row is for — open, and fold — are checked on rows from `lastSurvivors`: elements
  // that were in the window before the last scroll and are still in it, unchanged. Their click handlers
  // are the ones the 2026-10-19 change kept alive, so this is the only place where "we kept the old row"
  // could quietly mean "we kept a row that no longer does anything".
  //
  // Both clicks are deliberately restricted to rows already marked read: clicking an unread row calls
  // `mark_seen`, which writes to the user's real mailbox and to the IMAP server behind it.
  // This probe reads the screen; it must not change it.
  const readRow = (el) => !el.classList.contains('unread');
  const onScreen = (el) => win().contains(el);
  const live = lastSurvivors.filter(onScreen);

  let wantedUid = null;
  let selected = [];
  let scrolledAway = null;
  const openRow = live.find((el) => readRow(el) && !el.classList.contains('thread-child'));
  if (!openRow) problems.push('no reused, already-read top-level row in the last window — "a reused row still opens" was not tested');
  else {
    wantedUid = openRow.dataset.uid;
    // What the row says the message is, taken before the click: "a reader appeared" is not the claim,
    // "this row opened **this** message" is. A kept row whose closure pointed at an older message
    // would pass the first and fail here.
    const wantedSubject = (openRow.querySelector('.msg-subject')?.firstChild?.textContent ?? '').trim();
    const scrollBefore = list.scrollTop;
    openRow.dispatchEvent(new MouseEvent('click', { bubbles: true }));
    // The body is fetched, so "opened" is not true on the next frame. Poll instead of asserting on a
    // screenshot taken mid-flight — that is how a probe reports a healthy app as broken.
    const shown = () => {
      const body = document.querySelector('#reader-body');
      const thread = document.querySelector('#thread-view');
      const subject = document.querySelector('#reader-subject');
      if (thread && !thread.hidden) return 'thread';
      if (body && !body.hidden) return subject && subject.textContent.trim() ? 'body' : 'empty';
      return 'hidden';
    };
    let view = shown();
    for (let waited = 0; waited < 40 && view === 'hidden'; waited += 1) {
      await new Promise((done) => setTimeout(done, 50));
      view = shown();
    }
    if (view === 'hidden') problems.push('clicking a reused row never showed a reader (placeholder still up after 2 s)');
    if (view === 'empty') problems.push('clicking a reused row opened a message with an empty subject');
    if (view !== 'hidden' && view !== 'empty' && wantedSubject) {
      // The reader has two shapes (single message, conversation), and both must contain the subject of
      // the row that was clicked — "a reader appeared" is not the claim, "this row opened this message" is.
      const shownText = (document.querySelector('#reader-subject')?.textContent ?? '')
        + ' ' + (document.querySelector('#thread-view')?.textContent ?? '');
      if (!shownText.includes(wantedSubject)) {
        problems.push(`reused row ${wantedUid} ("${wantedSubject}") opened a reader that does not contain that subject`);
      }
    }
    // Opening a message re-renders the list, and the re-render rebuilds the scroll container, which
    // puts the list back at the top: the clicked row is usually no longer on screen here. Recording
    // that rather than asserting on it — it is what the app has done all along, not what this change
    // claims — and only checking the highlight when the row did survive.
    scrolledAway = { before: scrollBefore, after: list.scrollTop };
    selected = [...win().children].filter((el) => el.classList.contains('selected')).map((el) => el.dataset.uid);
    const stillInWindow = [...win().children].some((el) => el.dataset.uid === wantedUid);
    if (stillInWindow && !selected.includes(wantedUid)) {
      problems.push('the clicked row ' + wantedUid + ' is on screen but not highlighted (selected: ' + (selected.join(',') || 'nobody') + ')');
    }
  }

  // The fold caret last: toggling it re-renders the whole list, which detaches every row the checks
  // above are holding. Run it after them and re-read the window instead of trusting the old elements.
  const caretRow = live.find((el) => readRow(el) && el.querySelector('.msg-caret') && onScreen(el));
  if (caretRow) {
    const childBefore = win().querySelectorAll('.thread-child').length;
    caretRow.querySelector('.msg-caret').dispatchEvent(new MouseEvent('click', { bubbles: true }));
    await settle();
    const childAfter = win().querySelectorAll('.thread-child').length;
    if (childAfter === childBefore) problems.push('the fold caret of a reused row did nothing: still ' + childAfter + ' child rows');
    const refold = [...win().children].find((el) => readRow(el) && el.querySelector('.msg-caret'));
    if (refold) {
      refold.querySelector('.msg-caret').dispatchEvent(new MouseEvent('click', { bubbles: true }));
      await settle();
    }
  }

  // Back to the top: the same rows in the same order. Not the same **elements** — a window that has
  // slid away drops the rows behind it, so the top rows are rebuilt on the way back, and expecting
  // otherwise would fail a list longer than two windows for the right reason.
  list.scrollTop = 0;
  await settle();
  const again = snapshot();
  if (again.uids.join(',') !== first.uids.join(',')) {
    problems.push(`scrolling back to the top produced a different first window: ${again.uids.join(',')} vs ${first.uids.join(',')}`);
  }

  return {
    problems,
    rowsInWindow: again.els.length,
    totalRows,
    rowHeight,
    distinctRowsSeen: visited.size,
    sweepCoveredWholeList: reachedEnd,
    reusedPerStep,
    reusedRowsInLastStep: live.length,
    listHeightPx: list.scrollHeight,
    clicked: wantedUid,
    highlighted: selected,
    listMovedAfterOpen: scrolledAway,
  };
}

const PROBE = `(${probePage.toString()})(${STEPS})`;

async function main() {
  const target = await findPageTarget();
  const ws = new WebSocket(target.webSocketDebuggerUrl);
  await new Promise((resolve, reject) => {
    ws.addEventListener('open', resolve);
    ws.addEventListener('error', reject);
  });
  const send = connect(ws);
  const result = await send('Runtime.evaluate', { expression: PROBE, awaitPromise: true, returnByValue: true });
  ws.close();

  if (result.exceptionDetails) {
    console.error('the probe threw in the page: ' + (result.exceptionDetails.text ?? JSON.stringify(result.exceptionDetails)));
    process.exit(1);
  }
  const m = result.result.value ?? {};
  const problems = m.problems ?? ['the probe returned nothing'];

  if (has('json')) {
    console.log(JSON.stringify(m));
  } else {
    console.log(`rows in window : ${m.rowsInWindow}   total rows: ${m.totalRows} (row ${m.rowHeight}px, list ${m.listHeightPx}px)`);
    console.log(`reuse          : rows kept across each scroll step: ${(m.reusedPerStep||[]).join(', ')} (the last step is the one the clicks below use)`);
    console.log(`coverage       : ${m.distinctRowsSeen} of ${m.totalRows} rows visited${m.sweepCoveredWholeList ? '' : ' (the sweep stopped at --steps)'}`);
    console.log(`click          : opened row ${m.clicked}, highlighted ${JSON.stringify(m.highlighted)}`);
    if (m.listMovedAfterOpen) console.log(`note           : opening it moved the list from scrollTop ${m.listMovedAfterOpen.before} to ${m.listMovedAfterOpen.after} — renderMessages rebuilds the scroll container, so the highlight check above may have had nothing left to check`);
  }
  if (problems.length) {
    for (const problem of problems) console.error(`- ${problem}`);
    const rebuilt = problems.some((p) => p.includes('reuse did not happen'));
    console.error(rebuilt
      ? '\nscrolling is rebuilding rows it could have kept — the P3 fix is not engaged (this probe was run against a build without row reuse, or the patch path broke)'
      : '\nthe windowed list is showing the wrong rows, or a row that is on screen no longer does anything');
    process.exit(1);
  }
  if (!has('json')) console.log('every step kept the rows it could have kept, in one contiguous order, and a kept row opened its own message');
}

if (has('help')) {
  console.log('usage: node tools/probe-list-reuse.mjs [--port 9222] [--steps 12] [--json]');
  process.exit(0);
}

await main();
