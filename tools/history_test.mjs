/**
 * Does 加载更多 try the local cache before the network — and stop only when both are exhausted?
 *
 *     node tools/history_test.mjs
 *
 * The failure this pins was live on a real mailbox (2026-10-06): the list shows one page (50) while the
 * first sync stores a whole window (500), so the cache holds far more than the list. The button went
 * straight to the server, the server had nothing older, and it announced "没有更早的邮件了" — while 142
 * messages that were already downloaded (back to 2026-09-02) were never shown. Both branches end with
 * *more rows in the list*, which is why the wrong order looks like "working" from the outside and only
 * a test like this one catches it.
 *
 * Loaded through `vm` the way the other renderer tests load their module, and `tools/check.ps1` runs it.
 */
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { dirname, join } from 'node:path';
import vm from 'node:vm';

const root = join(dirname(fileURLToPath(import.meta.url)), '..');

const sandbox = {};
vm.createContext(sandbox);
vm.runInContext(readFileSync(join(root, 'src/history.js'), 'utf8'), sandbox, {
  filename: 'src/history.js',
});

const { historyStep } = sandbox.__easyEmailHistory ?? {};
if (typeof historyStep !== 'function') {
  console.error('- src/history.js did not define globalThis.__easyEmailHistory.historyStep');
  process.exit(1);
}

const failures = [];
const check = (label, actual, expected) => {
  if (actual !== expected) failures.push(`${label}: got ${JSON.stringify(actual)}, expected ${JSON.stringify(expected)}`);
};
const checkNot = (label, actual, unwanted) => {
  if (actual === unwanted) failures.push(`${label}: got ${actual}, which is exactly what must not happen`);
};

// ---------------------------------------------------------------- the cache comes first
// Round 75 changed the question from "shown vs cached" (two counts that are equal by construction
// once the list pages) to "did the cache page return rows". These assertions are the same rule in
// the new shape: a page with rows is always shown, whatever the server has or has not said.
check('a page of cached rows is shown first', historyStep({ pageCount: 50, serverDone: false }), 'show-cached');
// Even when a previous server fetch already said "that is the bottom", cached rows the list has not
// shown yet must still be reachable: the two facts are about different sources.
check('cached rows still win over a finished server walk', historyStep({ pageCount: 1, serverDone: true }), 'show-cached');
check('one more cached row is enough to show locally', historyStep({ pageCount: 1, serverDone: false }), 'show-cached');

// ---------------------------------------------------------------- then the server
check('an exhausted cache asks the server', historyStep({ pageCount: 0, serverDone: false }), 'ask-server');

// ---------------------------------------------------------------- and only then "no more"
check('both exhausted means done', historyStep({ pageCount: 0, serverDone: true }), 'done');
// The exact combination the 2026-07 bug produced — the list was told "done" while cached rows it had
// not shown were still there. In this shape it cannot be written down, and the assertion below is
// what keeps that true if someone reintroduces a "shown vs cached" comparison.
checkNot('done is never reported while a cache page has rows', historyStep({ pageCount: 1, serverDone: true }), 'done');

if (failures.length > 0) {
  for (const failure of failures) console.error(`- ${failure}`);
  console.error('\n加载更多 would skip mail that is already cached on this machine');
  process.exit(1);
}

console.log(
  `history paging: ${[
    'the cache is shown before the network',
    'the server is asked once the cache is exhausted',
    'done only when both are',
  ].join(' / ')}`
);
