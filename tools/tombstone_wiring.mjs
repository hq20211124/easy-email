/**
 * 墓碑接上生产代码了吗 —— `design/07` §14.9 的五条接线。
 *
 * The table, the four methods and the tests all existed for two rounds while **nothing in the app
 * called them**. So the bug they were built for — "删了又同步回来" — stayed alive, and no test failed,
 * because every test was about the storage rather than about the wiring. That is the failure mode
 * this step exists to catch, and it is invisible to a screenshot: the message simply comes back.
 *
 * Five things have to stay true, and each one is a *call site*, not a definition:
 *
 *   1. `refresh_folder` reads the marks out of `user.db` and hands them to the driver. A driver
 *      built without them silently stores everything the server sends.
 *   2. `SyncDriver::start` takes them as a **required** argument — an `Option` would let the next
 *      caller opt out by accident, which is exactly how this bug was born.
 *   3. `cache.delete_message` clears `sent_index` in the same transaction, or 「查看副本」 keeps a
 *      link to a row that is gone.
 *   4. The mark is cleared on the **success** arm of a replay and nowhere else: clearing it on a
 *      failure resurrects the message one sync later.
 *   5. The 已发送 empty state asks for the count and shows it as a hint, never as a list source.
 *
 *     node tools/tombstone_wiring.mjs
 */

import { readFileSync } from 'node:fs';

const read = (rel) => readFileSync(new URL(rel, import.meta.url), 'utf8');

const lib = read('../src-tauri/src/lib.rs');
const sync = read('../crates/mail-core/src/sync.rs');
const cache = read('../crates/mail-core/src/cache.rs');
const pending = read('../crates/mail-core/src/pending.rs');
const app = read('../src/app.js');

function fail(message) {
  console.error(`- ${message}`);
  process.exit(1);
}

// 1. The production path reads the marks and passes them on.
if (!/\.tombstones\(&cfg\.id, mailbox\)/.test(lib)) {
  fail('refresh_folder must read the tombstones out of user.db — without them the sync stores everything the server sends');
}
if (!/SyncDriver::start\(cache, &cfg\.id, &info, &tombstones\)/.test(lib)) {
  fail('refresh_folder must hand the tombstones to the driver; building it without them is the bug this step guards');
}
// The other two server-facing walks re-store pages that may contain deleted mail.
const backfillCalls = lib.match(/backfill_older\(/g) ?? [];
if (backfillCalls.length < 1 || !/backfill_older\([\s\S]{0,400}?&tombstones,/.test(lib)) {
  fail('backfill_older must be given the tombstones: 加载更多 re-fetches pages the user may have deleted from');
}
if (!/fill_thread_keys\([\s\S]{0,400}?&tombstones,/.test(lib)) {
  fail('fill_thread_keys must be given the tombstones for the same reason');
}

// 2. Required, not optional.
const startSignature = sync.match(/pub fn start\([\s\S]*?\)\s*->\s*Result<Self>/);
if (!startSignature) fail('SyncDriver::start is missing');
if (!/tombstones: &Tombstones/.test(startSignature[0])) {
  fail('SyncDriver::start must take `&Tombstones` (a required argument, not an Option)');
}
if (/tombstones: Option</.test(startSignature[0])) {
  fail('an Option lets the next caller opt out by accident — that is how the resurrection bug was born');
}

// 3. The pointer dies with the message, in the same closure (one write transaction).
const deleteMessage = cache.match(/pub fn delete_message\([\s\S]*?\n    \}/);
if (!deleteMessage) fail('Cache::delete_message is missing');
if (!/delete from sent_index where account_id = \?1 and folder = \?2 and uid = \?3/.test(deleteMessage[0])) {
  fail('delete_message must clear sent_index, or 「查看副本」 points at a row that is gone');
}
if (/write_blocking/.test(deleteMessage[0].split('delete from sent_index')[0]) === false) {
  fail('the sent_index delete must happen inside the same write_blocking closure as the rest');
}

// 4. Cleared on success only.
const replay = pending.match(/match applier\.apply\(&op\)\.await \{[\s\S]*?\n        \}/);
if (!replay) fail('the apply/record match in replay_pending_ops is missing');
const successArm = replay[0].split('Err(e) =>')[0];
const failureArm = replay[0].split('Err(e) =>')[1] ?? '';
if (!/clear_deleted_mark/.test(successArm)) {
  fail('a confirmed delete must clear its tombstone');
}
if (/clear_deleted_mark/.test(failureArm)) {
  fail('a failed delete must KEEP its tombstone: clearing it puts the message back one sync later');
}
if (!/op\.kind == Some\(PendingOpKind::Delete\)/.test(successArm)) {
  fail('only a delete may clear a tombstone — a queued flag change must not resurrect a deleted message');
}

// 5. The table lives in the database a user cannot delete, and nowhere else.
//
// `design/07` §14.0 第 4 条: a tombstone in `cache.db` disappears the moment somebody uses 「清空缓存」
// — and the next sync then restores every message they deleted. It is a fact about a user action,
// so it belongs with the drafts and the queue, not with the throwaway cache.
if (/deleted_marks/.test(cache)) {
  fail('deleted_marks must not appear in cache.rs: 清空缓存 would wipe the tombstones and resurrect deleted mail');
}
if (!/create table if not exists deleted_marks/.test(read('../crates/mail-core/src/userdb.rs'))) {
  fail('deleted_marks must be created by user.db\'s own migration');
}

// 6. The empty state's hint, and the one rule about it.
if (!/invoke\('sent_record_count'/.test(app)) {
  fail('the 已发送 empty state must ask for the record count (`design/07` §14.8)');
}
const note = app.match(/async function noteSentRecordsOnEmpty\([\s\S]*?\n\}/);
if (!note) fail('noteSentRecordsOnEmpty is missing');
if (!/looksLikeSentFolder\(state\.mailbox\)/.test(note[0])) {
  fail('the hint belongs to 已发送 only — other folders have no such records to speak of');
}
if (/state\.messages\s*(\.push|=)/.test(note[0])) {
  fail('the count is a hint, never a list source: pushing it into state.messages is the two-source merge the design rejected');
}

console.log('墓碑接线五条都在：同步读它、驱动必须收它、删信清指针、成功才清标记、库放对了、空态只说一句');
