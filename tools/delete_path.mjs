/**
 * 删除路径的四条规矩 —— 以及「墓碑不许回来」(`design/07` §14.9, 2026-11-01 决定).
 *
 * **Why this file exists at all.** For two rounds the repo carried a `deleted_marks` table with a
 * writer, a clearer, a 30-day expiry and a gate rule, whose only job was to keep a message the user
 * deleted from coming back when the server still listed it. It was removed on 2026-11-01: this is a
 * client, the server is the truth, and the answer to a resurrected message is to delete it again.
 * What is left of the delete path is four things that are *not* optional, and each one is a call
 * site rather than a definition — the exact shape that once let a whole feature rot unwired.
 *
 *   1. `deleted_marks` must not come back. Not the table, not the writer, not the prune.
 *   2. `cache.delete_message` clears `sent_index` in the same write, or 「查看副本」 keeps a link to a
 *      row that is gone.
 *   3. A folder-wide `EXPUNGE` runs only when the `\Deleted` set is exactly our message: on a server
 *      without UIDPLUS it would otherwise destroy another client's pending deletion.
 *   4. Both "still on the server" outcomes (a `MarkedOnly` expunge, and a network failure) queue a
 *      retry. Without it the message is deleted locally and never on the server — which is the one
 *      case the removed tombstone used to paper over.
 *
 *     node tools/delete_path.mjs
 */

import { readFileSync } from 'node:fs';

const read = (rel) => readFileSync(new URL(rel, import.meta.url), 'utf8');

// Production code only: the migration test legitimately builds a v5-shaped table to prove the
// upgrade drops it, and a check that counted that would forbid the very test that guards the rule.
const withoutTests = (src) => src.split('\nmod tests {')[0];

const lib = read('../src-tauri/src/lib.rs');
const sync = read('../crates/mail-core/src/sync.rs');
const cache = withoutTests(read('../crates/mail-core/src/cache.rs'));
const imap = read('../crates/mail-core/src/imap.rs');
const userdb = withoutTests(read('../crates/mail-core/src/userdb.rs'));
const app = read('../src/app.js');

function fail(message) {
  console.error(`- ${message}`);
  process.exit(1);
}

// 1. The tombstone stays gone. A table nobody writes is a table the next reader trusts, so the
//    upgrade drops it — and this is what stops it from being re-created "just in case".
// One line, not `[^;]`: a statement's name is on its own line, while `[^;]` happily walks across
// a doc comment and matches prose that merely mentions the table.
if (/create table[^\n]*deleted_marks/i.test(userdb) || /create table[^\n]*deleted_marks/i.test(cache)) {
  fail('deleted_marks was removed on purpose (design/07 §14.9) — do not create it again');
}
if (!/drop table if exists deleted_marks/.test(userdb)) {
  fail('user.db v6 must drop the tombstone table; leaving it behind is a schema that lies');
}
if (/deleted_marks/.test(cache)) {
  fail('the tombstone must not reappear in cache.rs either');
}
for (const [name, src] of [
  ['sync.rs', sync],
  ['src-tauri/src/lib.rs', lib],
]) {
  if (/hidden_by_tombstone|Tombstones/.test(src)) {
    fail(`${name} still references the removed tombstone plumbing`);
  }
}

// 2. The pointer dies with the message, in the same closure (one write transaction).
const deleteMessage = cache.match(/pub fn delete_message\([\s\S]*?\n    \}/);
if (!deleteMessage) fail('Cache::delete_message is missing');
if (!/delete from sent_index where account_id = \?1 and folder = \?2 and uid = \?3/.test(deleteMessage[0])) {
  fail('delete_message must clear sent_index, or 「查看副本」 points at a row that is gone');
}

// 3. The folder-wide EXPUNGE is not ours to run blindly.
const flagAndExpunge = imap.match(/async fn flag_deleted_and_expunge\([\s\S]*?\n    \}/);
if (!flagAndExpunge) fail('flag_deleted_and_expunge is missing');
if (!/uid_search\("DELETED"\)/.test(flagAndExpunge[0])) {
  fail('without UIDPLUS the client must count the \\Deleted set before a folder-wide EXPUNGE');
}
if (!/flagged\.len\(\) == 1 && flagged\.contains\(&uid\)/.test(flagAndExpunge[0])) {
  fail('the folder-wide EXPUNGE may only run when the set is exactly the message we just flagged');
}
if (/DeleteOutcome::MarkedOnly/.test(flagAndExpunge[0]) === false) {
  fail('the unsafe case must be reported as MarkedOnly, not silently treated as done');
}

// 4. Both "still on the server" outcomes queue the retry.
const deleteCommand = lib.match(/async fn delete_message\([\s\S]*?\n\}/);
if (!deleteCommand) fail('the delete_message command is missing');
const queues = (deleteCommand[0].match(/PendingOpKind::Delete/g) ?? []).length;
if (queues < 2) {
  fail('both MarkedOnly and a network failure must queue a retry (PendingOpKind::Delete)');
}
if (!/DeleteOutcome::Expunged/.test(deleteCommand[0])) {
  fail('the command must branch on the outcome: 「已删除」 and 「已标记删除」 are different facts');
}

// 5. The 已发送 empty state's hint: a hint, never a list source.
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

console.log('删除路径四条都在：墓碑没回来、指针同事务清、文件夹级 EXPUNGE 先数集合、两种「还在服务器上」都排队');
