/**
 * The rules behind the saved-search rows.
 *
 *     node tools/saved_search_test.mjs
 *
 * Round 89 (`design/09` §1 goal 7, SRCH-05). The storage layer was round 88; this pins the two
 * things that go wrong in the **renderer**, where nothing else would notice:
 *
 *   1. a name that is the whole query — a sidebar row 200 characters wide. The search box already
 *      accepts that; the sidebar cannot show it, and a row that wraps is a row nobody reads;
 *   2. a truncation that cuts a *value* in half (`after:2026-0`). That one is worse than long: it
 *      looks like a bug in the row, and the user stops trusting the list that produced it.
 *
 * Both rules are in `suggestName` and both are consequences of the same decision (truncate on whole
 * tokens, keep the key), so the test also checks the boundary where the decision changes.
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
vm.runInContext(readFileSync(join(root, 'src/saved-search.js'), 'utf8'), sandbox, {
  filename: 'src/saved-search.js',
});

const { suggestName, splitByScope, NAME_MAX } = sandbox.__easyEmailSavedSearch ?? {};
if (typeof suggestName !== 'function' || typeof splitByScope !== 'function') {
  console.error('- src/saved-search.js did not define globalThis.__easyEmailSavedSearch');
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

// ---------------------------------------------------------------- the short ones survive whole
check('an empty query has no suggested name', suggestName(''), '');
check('whitespace is not a name', suggestName('   \n '), '');
check('a short query is its own name', suggestName('is:unread'), 'is:unread');
check('padding is trimmed first', suggestName('  has:attachment  '), 'has:attachment');

// ---------------------------------------------------------------- the long ones keep whole tokens
const longQuery = 'from:shop has:attachment after:2026-01-01 subject:订单 filename:invoice.xlsx';
const suggested = suggestName(longQuery);
checkTrue(
  `"${suggested}" says there was more`,
  suggested.endsWith('…'),
  'a name that just stops looks like the query ended there',
);
checkTrue(`"${suggested}" fits the sidebar`, [...suggested].length <= NAME_MAX + 1);
// The rule the truncation must not break: it stops at a token boundary, never in the middle of a
// value. Expressed as "what came before the ellipsis is a prefix of the query that ends where a
// space begins" — that is the whole rule, and it catches `after:2026-0` because `after:2026-0` is
// not a prefix of the query at all (the query has `after:2026-01-01`, so position 12 of the query
// is `-`, not a space).
const tokens = longQuery.split(/\s+/);
const kept = suggested.replace('…', '');
checkTrue(`"${kept}" is a prefix of the query`, longQuery.startsWith(kept));
checkTrue(
  `"${suggested}" stops at a token boundary`,
  kept === longQuery || longQuery[kept.length] === ' ',
  'the next character in the query must be a space, not part of a value',
);
checkTrue(`"${suggested}" is made of whole tokens`, kept.split(' ').every((part) => tokens.includes(part)));

// ---------------------------------------------------------------- one very long token keeps its key
const oneLongToken = 'filename:verylongattachmentname-from-the-shop.xlsx';
const named = suggestName(oneLongToken);
checkTrue(`"${named}" keeps the key`, named.startsWith('filename:'), 'the key is what says what it is');
checkTrue(`"${named}" marks the cut`, named.endsWith('…'));
checkTrue(
  'the cut is in the value, not the key',
  named.slice('filename:'.length).replace('…', '').length >= 3,
  'a one-character value is not information',
);
checkTrue('and it still fits', [...named].length <= NAME_MAX + 1);

// A long token with no key is the case that cannot be helped; it must still be bounded.
const word = suggestName('x'.repeat(200));
checkTrue('a bare long word is bounded', [...word].length <= NAME_MAX + 1);

// ---------------------------------------------------------------- scope splitting
const split = splitByScope([
  { id: 1, account_id: 'a', name: 'scoped-a' },
  { id: 2, account_id: null, name: 'global' },
  { id: 3, account_id: 'b', name: 'scoped-b' },
]);
check(
  'account-scoped rows, whatever the account',
  split.account.map((s) => s.name).join(','),
  'scoped-a,scoped-b',
);
check('globals are their own group', split.global.map((s) => s.name).join(','), 'global');
const empty = splitByScope(null);
check('rubbish in, empty groups out', empty.account.length + empty.global.length, 0);
const mixed = splitByScope([null, 'x', 7]);
check('non-objects are skipped without throwing', mixed.account.length + mixed.global.length, 0);

// ---------------------------------------------------------------- report
if (failures.length > 0) {
  console.error(`saved_search_test: ${failures.length} failing`);
  for (const line of failures) console.error(`  - ${line}`);
  process.exit(1);
}
console.log('saved_search_test ok');
