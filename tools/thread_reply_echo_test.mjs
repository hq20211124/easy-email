/**
 * 在会话里回一句话，得当场看见它 —— 而不是等一轮同步 (round 122, user's report).
 *
 * The failure this pins is silent in every screenshot taken before the send: the reply travels
 * outbox → worker → SMTP → the Sent copy is filed → the next sync puts it in the cache. The
 * conversation is read from the cache, so between pressing 发送 and that last step the thread is
 * unchanged — which looks exactly like "my reply did not go anywhere".
 *
 * What must stay true, all of it structural because none of it is visible in a static render:
 *
 *   1. `sendThreadReply` keeps the outbox id `enqueue_send` returns, and calls
 *      `showSentReplyInThread` with it — a bubble keyed by anything else could not follow the row.
 *   2. That bubble is **not** pushed into `state.threadMembers`: that array is the cache's answer and
 *      is replaced wholesale by the next `loadConversation`, so mixing the two resurrects or
 *      duplicates the message on a refresh.
 *   3. The pending bubble is drawn by `renderThreadReader` after the real messages.
 *   4. 撤销 removes it (`forgetSentReply`), and a re-read of the conversation clears the list.
 *   5. Its label comes from the outbox state, so it never claims to be sent before it is.
 *
 *     node tools/thread_reply_echo_test.mjs
 */

import { readFileSync } from 'node:fs';

const source = readFileSync(new URL('../src/app.js', import.meta.url), 'utf8');

function fail(message) {
  console.error(`- ${message}`);
  process.exit(1);
}

/** The body of one function, from its declaration to the next top-level `}`. */
function bodyOf(name) {
  const start = source.indexOf(`function ${name}(`);
  if (start < 0) fail(`${name} is missing`);
  let depth = 0;
  let i = source.indexOf('{', start);
  const from = i;
  for (; i < source.length; i += 1) {
    if (source[i] === '{') depth += 1;
    else if (source[i] === '}') {
      depth -= 1;
      if (depth === 0) return source.slice(from, i + 1);
    }
  }
  return source.slice(from);
}

// 1. The send path keeps the id and draws the bubble with it.
const send = bodyOf('sendThreadReply');
if (!/const queuedId = await run\('加入发件箱'/.test(send)) {
  fail('sendThreadReply must keep the outbox id enqueue_send returns');
}
if (!/showSentReplyInThread\(queuedId/.test(send)) {
  fail('sendThreadReply must put the reply into the conversation it was typed in');
}
const enqueue = send.indexOf("run('加入发件箱'");
const draw = send.indexOf('showSentReplyInThread(queuedId');
if (enqueue < 0 || draw < enqueue) fail('the bubble must be drawn after the enqueue succeeds');

// 2. The cache's array stays the cache's array.
const show = bodyOf('showSentReplyInThread');
if (/threadMembers\.push|threadMembers\s*=\s*\[\.\.\./.test(show)) {
  fail('the pending reply must not be pushed into threadMembers: the next load replaces that array');
}
if (!/threadLocal\.push/.test(show)) fail('showSentReplyInThread must record the bubble in threadLocal');

// 3. It is rendered after the real messages.
const render = bodyOf('renderThreadReader');
if (!/\.\.\.state\.threadLocal/.test(render)) {
  fail('renderThreadReader must draw the pending replies after the real messages');
}
if (!/renderPendingBubble/.test(render)) fail('renderThreadReader must have a pending-bubble renderer');

// 4. Undo takes it back out; a re-read clears the list.
if (!/forgetSentReply\(id\)/.test(bodyOf('cancelSend'))) {
  fail('撤销 must remove the bubble — otherwise the conversation shows a reply that was never sent');
}
const load = bodyOf('loadConversation');
if (!/state\.threadLocal = \[\]/.test(load)) {
  fail('loadConversation must clear threadLocal: the cache speaks for the whole thread again');
}

// 5. The label follows the queue rather than a guess.
const label = bodyOf('pendingSendLabel');
for (const state of ['sent', 'failed', 'sending']) {
  if (!label.includes(`'${state}'`)) fail(`pendingSendLabel must handle the outbox state '${state}'`);
}
if (!/row\.state !== entry\.state/.test(bodyOf('syncThreadPendingStates'))) {
  fail('the bubbles must follow the outbox rows as they move');
}

console.log('会话里的回复当场出现：用发件箱 id 记着，撤销会收回，重新打开以缓存为准');
