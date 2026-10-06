/* Easy Email — who a conversation reply goes to, and the headers that put it in the same thread.
 *
 * Two small decisions that are easy to get wrong and impossible to check by looking at the screen:
 *
 *   1. **Which message of a conversation a reply answers.** Replying to the newest message is wrong
 *      whenever the newest message is the user's own (very common: you replied last, then want to add
 *      something) — the reply would be addressed to the user themselves. And when a bubble is
 *      expanded, that bubble is the one being answered, because that is what the user pointed at.
 *   2. **What makes the reply join the conversation.** `In-Reply-To` plus the `References` chain,
 *      exactly as the full composer builds them — a reply with a fresh subject and no references
 *      arrives as a new conversation, which is the one outcome a mail client must not produce.
 *
 * Kept here, as pure functions, for the same reason `threads.js` is: `tools/reply_test.mjs` runs in
 * node and pins the cases that only show up in a real mailbox, and `src/app.js` has no second copy of
 * the rules. The subject prefixing is shared with the composer for the same reason.
 */
(function (global) {
  'use strict';

  /** Prefixes that already mean "this is a reply", in both languages this app is used in. */
  const REPLY_PREFIX = /^\s*(re|答复|回复|回覆)\s*[:：]/i;
  /** The same for forwards. */
  const FORWARD_PREFIX = /^\s*(fwd|fw|转发)\s*[:：]/i;

  /**
   * The message a reply written at the bottom of a conversation answers.
   *
   * `focused` is the expanded bubble, or `null` when every bubble is collapsed. `isMine` is passed in
   * rather than looked up here so this stays a pure function of its arguments — the caller owns the
   * account list.
   *
   * Returns `null` when nothing in the conversation can be answered: every message is the user's own,
   * or the messages carry no address at all. Inventing one (the user's own address) would send mail to
   * themselves, so the caller says so instead.
   */
  function replyTarget(members, focused, isMine) {
    const addressOf = (member) => member?.summary?.reply_to || member?.summary?.from_addr || '';
    const answerable = (member) => addressOf(member) !== '' && !isMine(member.summary.from_addr);

    if (focused && answerable(focused)) return focused;

    // Newest first: a conversation is ordered oldest → newest by the backend.
    const list = members ?? [];
    for (let index = list.length - 1; index >= 0; index -= 1) {
      if (answerable(list[index])) return list[index];
    }
    return null;
  }

  /** `Re: …`, unless the subject already says so. */
  function replySubject(subject) {
    const text = String(subject ?? '');
    return REPLY_PREFIX.test(text) ? text : `Re: ${text}`;
  }

  /** `Fwd: …`, unless the subject already says so. */
  function forwardSubject(subject) {
    const text = String(subject ?? '');
    return FORWARD_PREFIX.test(text) ? text : `Fwd: ${text}`;
  }

  /**
   * The address, subject and threading headers for a reply to one message.
   *
   * `to` prefers `Reply-To` over `From` — that is what the header is for, and mailing lists depend on
   * it. An empty `to` is returned rather than guessed at; the caller decides what to say about it.
   */
  function replyHeaders(detail) {
    const references = [...(detail?.references ?? []), detail?.message_id].filter(Boolean);
    return {
      to: detail?.reply_to || detail?.from_addr || '',
      subject: replySubject(detail?.subject),
      inReplyTo: detail?.message_id ?? null,
      references,
    };
  }

  global.__easyEmailReply = { replyTarget, replySubject, forwardSubject, replyHeaders };
})(typeof window === 'undefined' ? globalThis : window);
