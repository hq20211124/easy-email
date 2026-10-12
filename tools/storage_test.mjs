/**
 * The rules behind the storage panel.
 *
 *     node tools/storage_test.mjs
 *
 * Round 95 (`design/03` §5.4). The panel shows three numbers the user has to be able to trust: how
 * big the cache is, what a cleanup would free, and what happened last time. Two of the ways those
 * go wrong are silent:
 *
 *   1. a cap box that accepts zero. "0" means "clean everything, always" — the one value a typo must
 *      never produce, and the reason `suggestStorageLimit` exists rather than `Number(input)`.
 *   2. a byte count that reads as 0.0 MB when there is 400 KB. A reader who sees that concludes the
 *      number is broken, and stops trusting the rest of the panel.
 *
 * Loaded through `vm` the way the other renderer tests load their module; `tools/check.ps1` runs it.
 */
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { dirname, join } from 'node:path';
import vm from 'node:vm';

const root = join(dirname(fileURLToPath(import.meta.url)), '..');

const sandbox = {};
vm.createContext(sandbox);
vm.runInContext(readFileSync(join(root, 'src/storage.js'), 'utf8'), sandbox, {
  filename: 'src/storage.js',
});

const { suggestStorageLimit, formatBytes, cleanupSummary, DEFAULT_LIMIT_MB } =
  sandbox.__easyEmailStorage ?? {};
if (typeof suggestStorageLimit !== 'function' || typeof cleanupSummary !== 'function') {
  console.error('- src/storage.js did not define globalThis.__easyEmailStorage');
  process.exit(1);
}

const failures = [];
const check = (label, actual, expected) => {
  if (actual !== expected) {
    failures.push(`${label}: got ${JSON.stringify(actual)}, expected ${JSON.stringify(expected)}`);
  }
};
const checkTrue = (label, condition) => {
  if (!condition) failures.push(`${label}: expected true`);
};

// ---------------------------------------------------------------- the cap box
check('an empty box is the default', suggestStorageLimit(''), DEFAULT_LIMIT_MB);
check('a blank box is the default', suggestStorageLimit('   '), DEFAULT_LIMIT_MB);
check('nothing at all is the default', suggestStorageLimit(null), DEFAULT_LIMIT_MB);
check('zero is the default, not "clean everything"', suggestStorageLimit('0'), DEFAULT_LIMIT_MB);
check('a negative is the default', suggestStorageLimit('-5'), DEFAULT_LIMIT_MB);
check('NaN is the default', suggestStorageLimit('abc'), DEFAULT_LIMIT_MB);
check('a sane number survives', suggestStorageLimit('250'), 250);
check('padding is forgiven', suggestStorageLimit('  250  '), 250);
check('a half megabyte rounds to a whole one', suggestStorageLimit('250.6'), 251);
// Above a terabyte the box is holding a mistake, not a policy.
check('a terabyte-scale value is capped', suggestStorageLimit('999999999'), 1_000_000);

// ---------------------------------------------------------------- byte counts
check('nothing is 0 MB', formatBytes(0), '0 MB');
check('a few hundred bytes say bytes', formatBytes(400), '400 字节');
check('kilobytes read as KB', formatBytes(400 * 1024), '400 KB');
check('megabytes read as MB', formatBytes(12.5 * 1024 * 1024), '12.5 MB');
check('hundreds of MB drop the decimal', formatBytes(240 * 1024 * 1024), '240 MB');
check('gigabytes read as GB', formatBytes(2.5 * 1024 ** 3), '2.5 GB');
checkTrue('never 0.0 for something nonzero', !formatBytes(400 * 1024).startsWith('0.0'));

// ---------------------------------------------------------------- the dry-run line
const empty = cleanupSummary({ steps: [] });
check('an empty plan says nothing needs cleaning', empty.text, '当前不需要清理。');
check('and offers no action', empty.actionable, false);
check('a missing plan is the same', cleanupSummary(null).text, '当前不需要清理。');

const one = cleanupSummary({ steps: [{ rows: 1, freed_bytes: 1024 * 1024 }] });
checkTrue('one body reads as a body', one.text.includes('1 封邮件'));
checkTrue('and the size is there', one.text.includes('1 MB'));
check('and it is actionable', one.actionable, true);

const many = cleanupSummary({
  steps: [
    { rows: 3, freed_bytes: 5 * 1024 * 1024 },
    { rows: 7, freed_bytes: 1024 * 1024 },
  ],
});
// Steps are summed: two rows of the same plan are one answer, not two.
checkTrue('rows add up', many.text.includes('10 封邮件'));
checkTrue('and so do the bytes', many.text.includes('6 MB'));

// ---------------------------------------------------------------- report
if (failures.length > 0) {
  console.error(`storage_test: ${failures.length} failing`);
  for (const line of failures) console.error(`  - ${line}`);
  process.exit(1);
}
console.log('storage_test ok');
