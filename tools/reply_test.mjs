/**
 * Does a reply written at the bottom of a conversation go to the right person, and join the thread?
 *
 *     node tools/reply_test.mjs
 *
 * The two failure modes this pins are both silent in the UI and loud in a real mailbox:
 *
 *   1. **Replying to the wrong message.** "Answer the newest message" is wrong whenever the newest
 *      message is the user's own — which is the normal state of a conversation they have already
 *      answered once — and the reply then goes to the user themselves.
 *   2. **Losing the thread.** A reply without `In-Reply-To`/`References` arrives as a brand new
 *      conversation: the recipient sees a second thread, and so does the sender's own list.
 *
 * Loaded through `vm` the way `tools/thread_keys_test.mjs` loads `src/threads.js`, and
 * `tools/check.ps1` runs this file so both stay asserted.
 */
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { dirname, join } from 'node:path';
import vm from 'node:vm';

const root = join(dirname(fileURLToPath(import.meta.url)), '..');

const sandbox = {};
vm.createContext(sandbox);
vm.runInContext(readFileSync(join(root, 'src/reply.js'), 'utf8'), sandbox, {
  filename: 'src/reply.js',
});

const { replyTarget, replySubject, forwardSubject, replyHeaders } = sandbox.__easyEmailReply ?? {};
if (!replyTarget || !replyHeaders) {
  console.error('- src/reply.js did not define globalThis.__easyEmailReply');
  process.exit(1);
}

const failures = [];
const check = (label, actual, expected) => {
  const a = JSON.stringify(actual);
  const b = JSON.stringify(expected);
  if (a !== b) failures.push(`${label}: got ${a}, expected ${b}`);
};

const ME = 'me@example.com';
const isMine = (addr) => String(addr ?? '').toLowerCase() === ME;
const member = (from, extra = {}) => ({
  folder: 'INBOX',
  summary: { uid: 1, from_addr: from, reply_to: null, ...extra },
});

// ---------------------------------------------------------------- who gets the reply
const incoming = member('alice@example.com');
const mine = member(ME);
const newest = member('bob@example.com');

check('no bubble expanded: the newest message that is not mine', replyTarget([incoming, mine, newest], null, isMine), newest);
check('the expanded bubble wins when it is not mine', replyTarget([incoming, newest], incoming, isMine), incoming);
// The one that matters: expanding your own message must not address the reply to yourself.
check('expanding my own message falls back to the counterpart', replyTarget([incoming, mine], mine, isMine), incoming);
check('a conversation of nothing but my own messages has no target', replyTarget([mine, mine], null, isMine), null);
check('an empty conversation has no target', replyTarget([], null, isMine), null);
check('a missing list is not a crash', replyTarget(undefined, null, isMine), null);
// A message with no address at all cannot be answered, and must not be chosen just because it is newest.
check('a message with no address is not a target', replyTarget([incoming, member('')], null, isMine), incoming);
check('Reply-To is what makes a message answerable', replyTarget([member('', { reply_to: 'list@example.com' })], null, isMine)?.summary.reply_to, 'list@example.com');

// ---------------------------------------------------------------- the subject line
check('a plain subject gets Re:', replySubject('十月报销'), 'Re: 十月报销');
check('an existing Re: is left alone', replySubject('Re: 十月报销'), 'Re: 十月报销');
// The composer used to test only the exact spelling `Re:`, so `RE:` grew a second prefix on every round.
check('RE: is recognised too', replySubject('RE: 十月报销'), 'RE: 十月报销');
check('the full-width colon is recognised', replySubject('回复：十月报销'), '回复：十月报销');
check('答复: is recognised', replySubject('答复: 十月报销'), '答复: 十月报销');
check('a plain subject gets Fwd:', forwardSubject('十月报销'), 'Fwd: 十月报销');
check('an existing Fwd:/Fw:/转发： is left alone', [
  forwardSubject('Fwd: x'),
  forwardSubject('Fw: x'),
  forwardSubject('转发：x'),
], ['Fwd: x', 'Fw: x', '转发：x']);

// ---------------------------------------------------------------- the threading headers
const headers = replyHeaders({
  from_addr: 'alice@example.com',
  reply_to: null,
  subject: '十月报销',
  message_id: '<second@example.com>',
  references: ['<root@example.com>', '<first@example.com>'],
});
check('to is the sender', headers.to, 'alice@example.com');
check('the reply answers this message', headers.inReplyTo, '<second@example.com>');
check('the whole chain is carried, oldest first, with this message appended', headers.references, [
  '<root@example.com>',
  '<first@example.com>',
  '<second@example.com>',
]);

// Reply-To wins: that is what the header is for, and mailing lists depend on it.
check(
  'Reply-To wins over From',
  replyHeaders({ from_addr: 'alice@example.com', reply_to: 'list@example.com', message_id: null }).to,
  'list@example.com'
);
// A message with no Message-ID still produces a usable reply rather than `[null]` in the chain.
const noId = replyHeaders({ from_addr: 'alice@example.com', references: ['<root@example.com>'] });
check('a missing Message-ID is not a null in the chain', noId.references, ['<root@example.com>']);
check('a missing Message-ID means no In-Reply-To', noId.inReplyTo, null);
check('no references at all is an empty chain', replyHeaders({ from_addr: 'a@b.c' }).references, []);
check('an empty message yields no address', replyHeaders(null).to, '');

if (failures.length > 0) {
  for (const failure of failures) console.error(`- ${failure}`);
  console.error('\na conversation reply would reach the wrong person or start a new thread');
  process.exit(1);
}

console.log(
  `conversation replies: ${[
    'the counterpart is chosen, never myself',
    'the subject keeps one prefix',
    'the References chain is carried and extended',
  ].join(' / ')}`
);
