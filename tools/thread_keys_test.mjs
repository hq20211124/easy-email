/**
 * Do the conversation keys group what a person would call one conversation — and, just as
 * importantly, refuse to group what is not?
 *
 *     node tools/thread_keys_test.mjs
 *
 * The grouping is an approximation of the `References` chain (see the header of `src/threads.js`),
 * so the failure modes are the interesting part: a reply that does not join its thread is a
 * nuisance, but two unrelated messages folded into one row hides mail. Both directions are asserted
 * here, and `tools/check.ps1` runs this file so they stay asserted.
 *
 * Loaded through `vm` the same way `tools/payload_keys.mjs` loads `src/payload.js`: the renderer's
 * scripts assign to `globalThis` precisely so a node test can read them without a bundler.
 */
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { dirname, join } from 'node:path';
import vm from 'node:vm';

const root = join(dirname(fileURLToPath(import.meta.url)), '..');

const sandbox = {};
vm.createContext(sandbox);
vm.runInContext(readFileSync(join(root, 'src/threads.js'), 'utf8'), sandbox, {
  filename: 'src/threads.js',
});

const { conversationKey, groupConversations } = sandbox.__easyEmailThreads;
if (!conversationKey || !groupConversations) {
  console.error('- src/threads.js did not define globalThis.__easyEmailThreads');
  process.exit(1);
}

const failures = [];
const check = (label, condition, detail) => {
  if (!condition) failures.push(`${label}: ${detail}`);
};

const message = (over) => ({
  uid: 1,
  folder: 'INBOX',
  subject: '',
  from_addr: 'a@example.com',
  date: 1,
  ...over,
});

// ---------------------------------------------------------------- what must group
const reply = conversationKey(message({ subject: 'Re: 十月报销' }));
const original = conversationKey(message({ subject: '十月报销' }));
check('plain reply joins its thread', reply === original, `${reply} != ${original}`);

for (const subject of [
  'RE: 十月报销',
  'Re：十月报销',
  'Fwd: 十月报销',
  'Fw: 十月报销',
  'Re: Fwd: Re: 十月报销',
  '回复：十月报销',
  '答复: 十月报销',
  '转发：十月报销',
  '  re :  十月报销  ',
]) {
  check(
    `prefix "${subject}"`,
    conversationKey(message({ subject })) === original,
    `${conversationKey(message({ subject }))} != ${original}`
  );
}

// ---------------------------------------------------------------- what must NOT group
check(
  'the same subject from two people stays apart',
  conversationKey(message({ subject: 'Invoice', from_addr: 'a@example.com' })) !==
    conversationKey(message({ subject: 'Invoice', from_addr: 'b@example.com' })),
  'folding two senders together would hide mail'
);
// The known boundary of this approximation, pinned so a future fix has a target: a *sent* message
// has the user as its counterpart, so their reply does not join the incoming message that started
// the thread. Fixing it needs the `References` chain in the list row (PROGRESS, "会话串").
check(
  'a sent reply does not yet join the incoming message (known limitation)',
  conversationKey(message({ subject: 'Re: hi', from_addr: 'me@example.com' })) !==
    conversationKey(message({ subject: 'hi', from_addr: 'a@example.com' })),
  'if this ever starts grouping, the References key has landed and this assertion should be flipped'
);
check(
  'subject-less messages are not all one conversation',
  conversationKey(message({ subject: '', uid: 1 })) !==
    conversationKey(message({ subject: '', uid: 2 })),
  'a bounce has nothing to match on and must not be folded in with another'
);
check(
  'unrelated subjects stay apart',
  conversationKey(message({ subject: '十月报销' })) !==
    conversationKey(message({ subject: '十一月报销' })),
  'different subjects are different conversations'
);

// ---------------------------------------------------------------- grouping
const thread = groupConversations([
  message({ uid: 3, subject: 'Re: 十月报销' }),
  message({ uid: 2, subject: '十月报销' }),
  message({ uid: 1, subject: '随便别的', from_addr: 'other@example.com' }),
]);
check('three messages become two conversations', thread.length === 2, `got ${thread.length}`);
check(
  'the group keeps the list order',
  thread[0].messages.map((m) => m.uid).join(',') === '3,2',
  `got ${thread[0].messages.map((m) => m.uid).join(',')}`
);
check('an empty list makes no groups', groupConversations([]).length === 0, 'expected none');
check('a missing list is not a crash', groupConversations(undefined).length === 0, 'expected none');

if (failures.length > 0) {
  for (const failure of failures) console.error(`- ${failure}`);
  console.error('\nconversation grouping disagrees with what a person would call one thread');
  process.exit(1);
}

console.log(
  `conversation keys: ${[
    'reply prefixes join',
    'counterpart decides',
    'subject-less stays alone',
    'order preserved',
  ].join(' / ')}`
);
