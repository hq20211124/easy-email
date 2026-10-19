/**
 * Does the first paint wait for the network it does not need — and does a failed refresh destroy
 * mail that is already on screen?
 *
 *     node tools/first_paint_test.mjs
 *
 * Until 2026-10-19 the folder view began with `list_messages`, which refreshes over IMAP before it
 * reads the cache, so the spinner covered rows that were already downloaded. Switching to
 * cache-first paint (`message_page` then `list_messages`) opens two ways to get it wrong, both
 * invisible in a screenshot taken after the loading finishes: declaring an unsynced folder empty,
 * and replacing a readable list with "收取失败" because the network is down.
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
vm.runInContext(readFileSync(join(root, 'src/first-paint.js'), 'utf8'), sandbox, {
  filename: 'src/first-paint.js',
});

const api = sandbox.__easyEmailFirstPaint ?? {};
if (typeof api.cacheStep !== 'function' || typeof api.freshStep !== 'function') {
  console.error('- src/first-paint.js did not define __easyEmailFirstPaint.{cacheStep, freshStep}');
  process.exit(1);
}

const failures = [];
const check = (label, actual, expected) => {
  if (actual !== expected) failures.push(`${label}: got ${JSON.stringify(actual)}, expected ${JSON.stringify(expected)}`);
};
const checkNot = (label, actual, unwanted) => {
  if (actual === unwanted) failures.push(`${label}: got ${actual}, which is exactly what must not happen`);
};

// ---------------------------------------------------------------- the cache leg paints
check('cached rows go on screen at once', api.cacheStep({ pageCount: 50 }), 'rows');
check('one cached row is enough to paint', api.cacheStep({ pageCount: 1 }), 'rows');

// ---------------------------------------------------------------- but an empty cache says nothing
// The whole failure of a naive cache-first: `message_page` returns [] for a folder that has never
// been synced, and rendering that list writes 「这个文件夹里还没有邮件。」 — a claim about the
// server, made from the absence of a local file.
checkNot('an unsynced folder is not declared empty', api.cacheStep({ pageCount: 0 }), 'rows');
check('an unsynced folder keeps waiting visibly', api.cacheStep({ pageCount: 0 }), 'spinner');
check('a cache leg that is not a number waits', api.cacheStep({ pageCount: undefined }), 'spinner');

// ---------------------------------------------------------------- the refresh repaints
check('a fresh page replaces what the cache painted', api.freshStep({ stale: false, failed: false, cacheShown: true }), 'rows');
check('a refresh that found nothing still repaints', api.freshStep({ stale: false, failed: false, cacheShown: false }), 'rows');

// ---------------------------------------------------------------- a failed refresh keeps the rows
// The reason this needs pinning: the failure path used to write an error into the list unconditionally,
// and once the list can already hold readable mail, that destroys information for no reason.
check('a failed refresh leaves cached rows up', api.freshStep({ stale: false, failed: true, cacheShown: true }), 'keep-cache');
checkNot('cached rows are not thrown away on a network failure', api.freshStep({ stale: false, failed: true, cacheShown: true }), 'error');
// ...and only when nothing was ever shown does the error get the screen.
check('nothing on screen and a failure is an error', api.freshStep({ stale: false, failed: true, cacheShown: false }), 'error');

// ---------------------------------------------------------------- a late answer belongs to nobody
// Two awaits, and the user can switch folders between them. The guard is cheap; the bug it prevents is
// a list for 收件箱 painted over 已发送 a second later.
check('a response for a folder already left is dropped', api.freshStep({ stale: true, failed: false, cacheShown: true }), 'drop');
check('a failed response for a folder already left is dropped too', api.freshStep({ stale: true, failed: true, cacheShown: false }), 'drop');

if (failures.length > 0) {
  for (const failure of failures) console.error(`- ${failure}`);
  console.error('\nthe first paint either waits for the network it does not need, or destroys rows the user could read');
  process.exit(1);
}

console.log(
  `first paint: ${[
    'cached rows are painted before the refresh',
    'an empty cache never claims the folder is empty',
    'a failed refresh keeps the rows it interrupted',
    'a late answer for another folder is dropped',
  ].join(' / ')}`
);
