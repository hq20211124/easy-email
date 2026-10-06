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

// ---------------------------------------------------------------- the exact key wins
check(
  'a backend thread key replaces the fallback',
  conversationKey(message({ subject: 'Re: anything', thread_key: '<root@x>' })) === '<root@x>',
  'the References-derived key must be used when the backend supplied one'
);
check(
  'messages of one real thread group even with different subjects',
  conversationKey(message({ subject: 'Re: hi', thread_key: '<root@x>' })) ===
    conversationKey(message({ subject: 'completely different', thread_key: '<root@x>' })),
  'a subject change mid-thread is normal (people edit subjects) and must not split the thread'
);
check(
  'a sent reply joins its thread through the exact key',
  conversationKey(
    message({ subject: 'Re: hi', from_addr: 'me@example.com', thread_key: '<root@x>' })
  ) ===
    conversationKey(message({ subject: 'hi', from_addr: 'a@example.com', thread_key: '<root@x>' })),
  'this is the limitation the exact key exists to remove'
);
check(
  'an empty backend key falls back to subject and sender',
  conversationKey(message({ subject: 'hi', thread_key: '' })) ===
    conversationKey(message({ subject: 'hi' })),
  'rows written before the column existed must still group by the fallback'
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

// ---------------------------------------------------------------- the two signals, unioned
// Reported 2026-10-06: a correspondent sent four separate messages under one subject, none of them a
// reply to anything (no `references`), so each carried its own `thread_key` — and the list showed four
// rows. The backend stores a second, heuristic key for exactly this mail
// (`crate::threads::subject_key`); the two signals are unioned, never ranked, so nothing that groups
// today can be split by the guess.
const keyed = (over) => message({ thread_key: null, subject_key: null, ...over });

const sameSubject = [
  keyed({ uid: 10, thread_key: '<a@outlook>', subject_key: 'subj:王芳 您好|wangfang@example.com' }),
  keyed({ uid: 11, thread_key: '<b@outlook>', subject_key: 'subj:王芳 您好|wangfang@example.com' }),
  keyed({
    uid: 12,
    thread_key: '<c@outlook>',
    subject_key: 'subj:王芳 您好|wangfang@example.com',
    subject: 'Re: 王芳 您好',
  }),
];
const merged = groupConversations(sameSubject);
check('separate messages under one subject become one row', merged.length === 1, `got ${merged.length}`);
check(
  'and the row keeps all of them',
  merged[0].messages.length === 3,
  `got ${merged[0].messages.length}`
);
check(
  'the row is opened with a key the backend accepts',
  merged[0].key === '<a@outlook>',
  `got ${merged[0].key}`
);

// A different sender with the same subject is a different conversation: the heuristic is
// sender-and-subject, not subject.
check(
  'a different sender is never folded in',
  groupConversations([
    ...sameSubject,
    keyed({ uid: 13, thread_key: '<d@else>', subject_key: 'subj:王芳 您好|someone@else.com' }),
  ]).length === 2,
  'sender is half of the key'
);

// The user's own mail carries no heuristic key (the backend refuses to guess which of several
// recipients a fresh mail belongs to), so it keeps its own row.
check(
  'mail the user sent is not folded in by subject',
  groupConversations([...sameSubject, keyed({ uid: 14, thread_key: '<e@qq>', from_addr: 'me@example.com' })])
    .length === 2,
  'your own fresh mail is not a conversation by subject'
);

// The union can only add: a chain whose subject was edited mid-thread keeps its chain, because its
// members share the exact key even though their subjects differ.
check(
  'an edited subject does not split a chain',
  groupConversations([
    keyed({ uid: 20, thread_key: '<root@x>', subject_key: 'subj:十月报销|boss@example.com' }),
    keyed({ uid: 21, thread_key: '<root@x>', subject: '改成别的主题了' }),
  ]).length === 1,
  'the chain is a fact and outranks the guess'
);

// ...and a chain absorbs a lone message that shares the heuristic key, which is what makes a fresh
// mail from the same person with the same subject join the conversation it belongs to.
const chainPlusLone = groupConversations([
  keyed({ uid: 30, thread_key: '<root2@x>', subject_key: 'subj:发票|billing@example.com' }),
  keyed({ uid: 31, thread_key: '<root2@x>', from_addr: 'me@example.com' }),
  keyed({ uid: 32, thread_key: '<lone@x>', subject_key: 'subj:发票|billing@example.com' }),
]);
check('a chain absorbs a lone message with its subject key', chainPlusLone.length === 1, `got ${chainPlusLone.length}`);
check('and keeps all three', chainPlusLone[0].messages.length === 3, `got ${chainPlusLone[0].messages.length}`);

// A row from before either column existed still groups the old way, which is what the conversation
// view falls back to as well.
check(
  'a row with no stored signal falls back to sender and subject',
  groupConversations([message({ uid: 40, subject: 'Re: 十月报销' }), message({ uid: 41, subject: '十月报销' })])
    .length === 1,
  'the pre-migration fallback must still work'
);

// The bound on the heuristic, and the reason it exists: recurring notifications reuse one subject for
// years (Google's 安全提醒, 24 times over 4.7 years in the real cache), while the case that asked for
// the heuristic is a burst inside a day. `THREAD_SESSION_GAP_SECS` in `cache.rs` is the same week.
const DAY = 86_400;
const sameWeek = groupConversations([
  keyed({ uid: 50, thread_key: '<g1@x>', subject_key: 'subj:安全提醒|no-reply@accounts.google.com', date: DAY * 100 }),
  keyed({ uid: 51, thread_key: '<g2@x>', subject_key: 'subj:安全提醒|no-reply@accounts.google.com', date: DAY * 102 }),
]);
check('messages two days apart under one subject are one conversation', sameWeek.length === 1, `got ${sameWeek.length}`);

const monthsApart = groupConversations([
  keyed({ uid: 60, thread_key: '<h1@x>', subject_key: 'subj:安全提醒|no-reply@accounts.google.com', date: DAY * 100 }),
  keyed({ uid: 61, thread_key: '<h2@x>', subject_key: 'subj:安全提醒|no-reply@accounts.google.com', date: DAY * 140 }),
]);
check('the same subject six weeks later is a different conversation', monthsApart.length === 2, `got ${monthsApart.length}`);

// Chained, not anchored: a running exchange never splits just because it outlives one window.
const chained = groupConversations([
  keyed({ uid: 70, thread_key: '<i1@x>', subject_key: 'subj:日结单|s@bank.hk', date: DAY * 100 }),
  keyed({ uid: 71, thread_key: '<i2@x>', subject_key: 'subj:日结单|s@bank.hk', date: DAY * 105 }),
  keyed({ uid: 72, thread_key: '<i3@x>', subject_key: 'subj:日结单|s@bank.hk', date: DAY * 110 }),
]);
check('a chain of short gaps stays one conversation', chained.length === 1, `got ${chained.length}`);

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
