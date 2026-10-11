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

const { computeWindow, windowChanged, scrollTopForRow, DEFAULT_ROW_HEIGHT, DEFAULT_OVERSCAN } = vlist;

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
