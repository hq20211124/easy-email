/**
 * 「已发送」的两个档是两个数据源，永不混排 (`design/07` §14.5 D5).
 *
 * The rule the design states in one sentence — `全部` reads only the cache, `待发` reads only the
 * outbox — is exactly the kind of thing a later edit breaks silently: add one row from the queue to
 * the cache list and the paging offsets, the count and the sort order all start disagreeing, with no
 * error anywhere. A screenshot cannot see it either (both lists look fine on their own).
 *
 * So it is pinned structurally, on the source text:
 *
 *   1. `renderMessages` has a 待发 branch, and it runs **before** the cache-empty check — otherwise
 *      the queue renders underneath (or instead of) rows that came from a different database.
 *   2. That branch renders the queue and **returns**; it never falls through to the cache rows.
 *   3. The queue half excludes `sent` rows: those are the 「已发出，副本待归档」 line, not rows.
 *   4. The pending count excludes `sent` too, or 「待发 (n)」 never reaches zero on an account whose
 *      Sent folder cannot be reached.
 *
 *     node tools/sent_tabs_test.mjs
 */

import { readFileSync } from 'node:fs';

const source = readFileSync(new URL('../src/app.js', import.meta.url), 'utf8');

function fail(message) {
  console.error(`- ${message}`);
  process.exit(1);
}

// 1. The branch exists, and is reached before the cache-empty early return.
// Anchored on the `if (` so a one-line caller elsewhere (the refresh handler also asks this question)
// cannot be mistaken for the render branch.
const pendingBranch = source.indexOf(
  "if (state.sentTab === 'pending' && looksLikeSentFolder(state.mailbox)) {"
);
if (pendingBranch < 0) {
  fail('renderMessages has no 待发 branch keyed on sentTab + the Sent folder');
}
const emptyCheck = source.indexOf('state.messages.length === 0', pendingBranch);
const emptyCheckBefore = source.indexOf('state.messages.length === 0');
if (emptyCheckBefore > 0 && emptyCheckBefore < pendingBranch) {
  fail('the 待发 branch must come BEFORE the cache-empty check, or the queue never renders');
}

// 2. It renders the queue and stops there.
const branch = source.slice(pendingBranch, pendingBranch + 500);
if (!/renderPending\(list\)/.test(branch)) {
  fail('the 待发 branch must render the pending list, not the cache rows');
}
if (!/return;/.test(branch)) {
  fail('the 待发 branch must return: falling through would paint cache rows under the queue');
}

// 3. `sent` rows are not rows in the queue half.
const filterAt = source.indexOf("state.outbox.filter((item) => item.state !== 'sent')");
if (filterAt < 0) {
  fail('the 待发 tab must filter out `sent` rows — they are the 副本待归档 line, not work');
}

// 4. One definition of "pending", and it does not include `sent`.
const counter = source.indexOf('function outboxPendingCount()');
if (counter < 0) {
  fail('outboxPendingCount() is missing: the sidebar badge and the tab must share one definition');
}
const counterBody = source.slice(counter, counter + 400);
for (const wanted of ['queued', 'sending', 'failed', 'needs_attention']) {
  if (!counterBody.includes(`'${wanted}'`)) {
    fail(`outboxPendingCount() must count \`${wanted}\` (design/07 §14.5 D5)`);
  }
}
if (counterBody.includes("'sent'")) {
  fail('outboxPendingCount() must NOT count `sent`: a message that went out is not pending work');
}

// 5. 行上的小字只在「服务器上没有副本」时出现，其余一律不说 (2026-11-01, `design/07` §14.5 D2).
//
// The first version labelled every row 「副本已归档」 or 「仅本地记录」. Both are our jargon, and a
// row in 已发送 already means "it was sent" — so the chip was noise, and the second label was
// sometimes a lie. The decision now is *silence unless the news is bad*, and a decision like that
// gets "restored" by the next reader of the design doc unless something checks it.
const chipStart = source.indexOf('function sentStateChip(');
if (chipStart < 0) {
  fail('sentStateChip() is missing: the row label has one home');
}
const chipBody = source.slice(chipStart, source.indexOf('\n}', chipStart));
// Comments explain *why* the old wording is gone and therefore contain it: strip them first, or the
// check passes on the explanation (this exact false pass happened in round 117).
const chipCode = chipBody.replace(/\/\*[\s\S]*?\*\//g, '').replace(/^\s*\/\/.*$/gm, '');
if (!chipCode.includes('hasCopy !== false')) {
  fail('the chip must be produced ONLY for `hasCopy === false`: every other answer is silence');
}
if (chipCode.includes('副本已归档')) {
  fail('the chip must not say 「副本已归档」 any more: a row in 已发送 already means it was sent');
}
if (chipCode.includes('仅本地记录')) {
  fail('「仅本地记录」 is jargon — the row says 「只在这台电脑上」, in words a user can read');
}
const chips = (chipCode.match(/msg-sent-state/g) ?? []).length;
if (chips !== 1) {
  fail(`sentStateChip must produce exactly one chip (found ${chips}): two states were the problem`);
}

console.log('两个档各读各的库；行上的小字只在没副本时出现，其余一律不说');
