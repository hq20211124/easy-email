/**
 * The windowing arithmetic of the message list, tested without a window.
 *
 * Run by `tools/check.ps1` (the "conversation grouping" step's neighbour) so that a change to
 * `src/vlist.js` cannot quietly turn "only the viewport is in the DOM" into "all of it is".
 *
 *     node tools/vlist_test.mjs
 */

import { readFileSync } from 'node:fs';
import vm from 'node:vm';

const source = readFileSync(new URL('../src/vlist.js', import.meta.url), 'utf8');
const sandbox = { module: { exports: {} } };
vm.runInNewContext(source, sandbox);
const vlist = sandbox.__easyEmailVList ?? sandbox.module.exports;

if (!vlist || typeof vlist.computeWindow !== 'function') {
  console.error('- src/vlist.js did not export __easyEmailVList.computeWindow');
  process.exit(1);
}

const {
  computeWindow,
  windowChanged,
  planWindowPatch,
  scrollTopForRow,
  DEFAULT_ROW_HEIGHT,
  DEFAULT_OVERSCAN,
} = vlist;

const failures = [];
const eq = (label, actual, expected) => {
  const a = JSON.stringify(actual);
  const b = JSON.stringify(expected);
  if (a !== b) failures.push(`${label}: got ${a}, expected ${b}`);
};
const ok = (label, condition) => {
  if (!condition) failures.push(`${label}: expected true`);
};

const H = DEFAULT_ROW_HEIGHT;

// ---------------------------------------------------------------- the empty and the tiny
{
  const w = computeWindow({ scrollTop: 0, viewportHeight: 600, count: 0 });
  eq('an empty list renders nothing', [w.start, w.end, w.totalHeight], [0, 0, 0]);
}
{
  // One row: overscan must not invent rows that do not exist.
  const w = computeWindow({ scrollTop: 0, viewportHeight: 600, count: 1 });
  eq('a one-row list renders exactly that row', [w.start, w.end], [0, 1]);
}
{
  // Fewer rows than the overscan would like: still no phantom rows.
  const w = computeWindow({ scrollTop: 0, viewportHeight: 600, count: 3 });
  eq('a short list is not padded with imaginary rows', [w.start, w.end], [0, 3]);
}

// ---------------------------------------------------------------- the middle of a long list
{
  // 1000 rows, scrolled to row 500, 600px viewport (≈10.7 rows) with overscan 8.
  const w = computeWindow({ scrollTop: 500 * H, viewportHeight: 600, count: 1000 });
  ok('the window starts before the viewport (overscan)', w.start < 500);
  ok('the window starts no earlier than overscan rows above', w.start >= 500 - DEFAULT_OVERSCAN);
  ok('the window covers the viewport', w.end * H >= 500 * H + 600);
  ok('the window does not exceed the list', w.end <= 1000);
  ok('the window is a fraction of the list', w.end - w.start < 40);
  eq('offsetTop is the first rendered row', w.offsetTop, w.start * H);
  eq('totalHeight is the whole list, not the window', w.totalHeight, 1000 * H);
}

// ---------------------------------------------------------------- the ends
{
  // At the very top, overscan has nothing above to reach for.
  const w = computeWindow({ scrollTop: 0, viewportHeight: 600, count: 1000 });
  eq('the top of the list starts at row 0', w.start, 0);
}
{
  // At the bottom: the window must not run past the last row.
  const w = computeWindow({ scrollTop: 1000 * H - 600, viewportHeight: 600, count: 1000 });
  eq('the bottom of the list ends at the last row', w.end, 1000);
  ok('and starts before it', w.start < 1000);
}

// ---------------------------------------------------------------- junk in, defined behaviour out
{
  // A wheel gesture can hand a scroll container a negative scrollTop (rubber-banding on macOS),
  // and the first frame before layout can report a zero viewport. Neither may become NaN.
  const w = computeWindow({ scrollTop: -120, viewportHeight: 0, count: 500 });
  ok('negative scrollTop clamps to the top', w.start === 0);
  ok('a zero viewport still renders something', w.end > w.start);
  ok('nothing is NaN after junk input', Number.isFinite(w.offsetTop) && Number.isFinite(w.totalHeight));
}
{
  const w = computeWindow({ scrollTop: 0, viewportHeight: 600, count: 100000 });
  ok('a 100k-row list is still a small window', w.end - w.start < 40);
  eq('and its total height is the whole thing', w.totalHeight, 100000 * H);
}

// ---------------------------------------------------------------- one commit per frame
{
  const a = computeWindow({ scrollTop: 0, viewportHeight: 600, count: 1000 });
  const b = computeWindow({ scrollTop: 10, viewportHeight: 600, count: 1000 });
  const c = computeWindow({ scrollTop: 500 * H, viewportHeight: 600, count: 1000 });
  eq('a scroll inside the same window is not a change', windowChanged(a, b), false);
  eq('a scroll into another window is', windowChanged(a, c), true);
  eq('a first call always counts as changed', windowChanged(null, a), true);
}

// ---------------------------------------------------------------- keeping a row in sight
{
  eq('a row above the viewport scrolls to its top', scrollTopForRow(10, { scrollTop: 20 * H, viewportHeight: 600, rowHeight: H }), 10 * H);
  eq('a row below the viewport scrolls to its bottom', scrollTopForRow(40, { scrollTop: 0, viewportHeight: 600, rowHeight: H }), 41 * H - 600);
  eq('a row already visible is left alone', scrollTopForRow(3, { scrollTop: 0, viewportHeight: 600, rowHeight: H }), 0);
}

// ---------------------------------------------------------------- keeping rows between frames
{
  const a = computeWindow({ scrollTop: 100 * H, viewportHeight: 600, count: 1000 });
  eq('no previous window means nothing to keep', planWindowPatch(null, a), null);
  eq('no new window means nothing to keep', planWindowPatch(a, null), null);

  // A scroll of two rows: the rows still on screen stay, and the only work is at the two ends.
  const down = computeWindow({ scrollTop: 102 * H, viewportHeight: 600, count: 1000 });
  const patch = planWindowPatch(a, down);
  eq('scrolling down keeps everything it can', [patch.keepStart, patch.keepEnd], [down.start, a.end]);
  eq('and drops only the rows that left, from the front', patch.dropFront, down.start - a.start);
  eq('adds only the rows that entered, at the back', [patch.dropBack, patch.addFront, patch.addBack], [0, 0, down.end - a.end]);

  const up = computeWindow({ scrollTop: 98 * H, viewportHeight: 600, count: 1000 });
  const back = planWindowPatch(a, up);
  eq('scrolling up is the mirror image', [back.dropFront, back.dropBack, back.addFront, back.addBack], [
    0,
    a.end - up.end,
    a.start - up.start,
    0,
  ]);

  // A jump past the end shares no row: the caller rebuilds rather than pretending to patch.
  eq('a jump with nothing in common is not a patch', planWindowPatch(a, computeWindow({ scrollTop: 500 * H, viewportHeight: 600, count: 1000 })), null);
}
{
  // The identity the DOM update depends on: after the patch, the window element holds exactly the
  // new window's rows — survivors plus what was added at either end, and nothing left over.
  let checked = 0;
  for (let from = 0; from < 900; from += 7) {
    const previous = computeWindow({ scrollTop: from * H, viewportHeight: 600, count: 1000 });
    for (const step of [-3, -1, 0, 1, 4]) {
      const next = computeWindow({ scrollTop: Math.max(0, (from + step) * H), viewportHeight: 600, count: 1000 });
      const patch = planWindowPatch(previous, next);
      if (patch === null) continue;
      const survivors = previous.end - previous.start - patch.dropFront - patch.dropBack;
      if (survivors !== patch.keepEnd - patch.keepStart) failures.push(`patch at ${from}/${step}: survivors miscounted`);
      if (patch.addFront + survivors + patch.addBack !== next.end - next.start) {
        failures.push(`patch at ${from}/${step}: would leave the wrong rows on screen`);
      }
      if (patch.addFront !== patch.keepStart - next.start || patch.addBack !== next.end - patch.keepEnd) {
        failures.push(`patch at ${from}/${step}: added rows are not the ones outside the overlap`);
      }
      checked += 1;
    }
  }
  ok('the patch arithmetic holds across a whole list', checked > 500);
}
{
  // The budget P3 exists to catch, judged against the shape `design/15` §6 describes: a sweep at the
  // step `tools/bench-scroll.mjs` uses (600 px per frame), counting what the page's MutationObserver
  // counts — one change per row added or removed.
  const viewport = 600;
  const stepPx = 600;
  let patchWorst = 0;
  let rebuildWorst = 0;
  let previous = computeWindow({ scrollTop: 0, viewportHeight: viewport, count: 1000 });
  for (let top = stepPx; top < 1000 * H; top += stepPx) {
    const next = computeWindow({ scrollTop: top, viewportHeight: viewport, count: 1000 });
    if (!windowChanged(previous, next)) continue;
    const patch = planWindowPatch(previous, next);
    const churn = patch === null ? 2 * (next.end - next.start) : patch.dropFront + patch.dropBack + patch.addFront + patch.addBack;
    patchWorst = Math.max(patchWorst, churn);
    rebuildWorst = Math.max(rebuildWorst, 2 * (next.end - next.start));
    previous = next;
  }
  ok(`a scroll stays under the 40-row budget (worst frame: ${patchWorst})`, patchWorst <= 40);
  ok(`and rebuilding the window is what broke it (worst frame: ${rebuildWorst})`, rebuildWorst > 40);
}

// ---------------------------------------------------------------- the two must not drift apart
{
  // The one that actually breaks the feature: if the CSS row height changes and this constant does
  // not, the window computes offsets for a list that is laid out differently — rows would appear to
  // drift as you scroll, with no error anywhere.
  const css = readFileSync(new URL('../src/style.css', import.meta.url), 'utf8');
  const declared = /--msg-height:\s*(\d+)px/.exec(css);
  if (!declared) {
    failures.push('style.css no longer declares --msg-height: the list row height must have exactly one home');
  } else {
    eq('--msg-height and DEFAULT_ROW_HEIGHT must agree', Number(declared[1]), DEFAULT_ROW_HEIGHT);
  }
  if (!source.includes('--msg-height')) {
    failures.push('vlist.js should name the CSS custom property it mirrors, so the next reader looks for both');
  }
}

if (failures.length > 0) {
  for (const failure of failures) console.error(`- ${failure}`);
  console.error('\nthe list would put the wrong rows in the DOM, or none at all');
  process.exit(1);
}
console.log(
  `vlist paging: window = viewport + ${DEFAULT_OVERSCAN} rows of overscan, row height ${DEFAULT_ROW_HEIGHT}px, one commit per window change`
);
