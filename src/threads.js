/* Easy Email — conversation grouping.
 *
 * `design/07` §2 asks the list to read like conversations rather than like a pile of messages. The
 * *exact* key for that is the `References` chain, and the renderer does not have it: a list row
 * carries only the subject, the sender and the date (`MessageSummary`), while `References` lives in
 * the detail that gets fetched when a message is opened. Putting it in the list is the real fix and
 * it needs a `cache.db` migration with new columns — see PROGRESS, "会话串".
 *
 * Until then this is the approximation every other client falls back to: **normalised subject plus
 * the counterpart address**. The counterpart matters: without it, "Invoice" from two different
 * senders would merge into one row, which is worse than no grouping at all. With it, the failure
 * mode is narrow enough to live with, and the UI says how many messages were folded together so the
 * user is never shown a hidden one.
 *
 * Kept as a pure function of the messages it is handed, so `tools/thread_keys_test.mjs` can pin the
 * cases that are easy to get wrong (prefix stripping, a missing subject, the two people who share a
 * first name).
 */
(function (global) {
  'use strict';

  /** Conversation-reply prefixes, in both languages this app is used in. */
  const REPLY_PREFIXES = [
    're',
    'fwd',
    'fw',
    '答复',
    '回复',
    '转发',
    '答复：',
    '回复：',
    '转发：',
    '回覆',
  ];

  /**
   * A subject with its reply/forward prefixes removed.
   *
   * Repeatedly, because "Re: Fwd: Re: x" is what a long thread actually looks like — stripping once
   * leaves "Fwd: Re: x" and every message in that thread gets a different key.
   */
  function normaliseSubject(subject) {
    let text = String(subject ?? '').trim();
    let stripped = true;
    while (stripped) {
      stripped = false;
      for (const prefix of REPLY_PREFIXES) {
        const pattern = new RegExp(`^${prefix}\\s*[:：]\\s*`, 'i');
        if (pattern.test(text)) {
          text = text.replace(pattern, '').trim();
          stripped = true;
        }
      }
    }
    // Case and内部空白 differences are noise for this purpose; CJK is unaffected by both.
    return text.replace(/\s+/g, ' ').toLowerCase();
  }

  /**
   * Who the conversation is with: the sender, by address.
   *
   * Only the address is used — display names change with every reply ("张三" vs "Zhang, San").
   *
   * **A known boundary of this approximation**: the list row does not say whether the message was
   * received or sent (that is a folder role, and `MailboxInfo` deliberately does not carry it). So a
   * message in 已发送 has the user as its counterpart, which means their own replies group with each
   * other but **not** with the incoming message that started the thread. Fixing that properly is the
   * `References` key, which needs the list to carry it (PROGRESS, "会话串"); pretending to handle it
   * here — by guessing "outgoing" from a field that does not exist — would be worse than the gap.
   */
  function counterpart(message) {
    return String(message.from_addr ?? message.reply_to ?? '')
      .trim()
      .toLowerCase();
  }

  /**
   * A stable key for the conversation a message belongs to.
   *
   * An empty subject is deliberately **not** grouped with other empty subjects: a subject-less
   * message (a bounce, a notification) has nothing to match on, and folding unrelated ones together
   * would hide mail. It gets a key of its own, derived from the message.
   */
  function conversationKey(message) {
    const subject = normaliseSubject(message.subject);
    if (!subject) {
      return `solo:${message.folder ?? ''}:${message.uid ?? ''}`;
    }
    return `${counterpart(message)}|${subject}`;
  }

  /**
   * The messages grouped into conversations, newest first, with the newest message of each group
   * first inside the group.
   *
   * The caller decides what to show; this only decides what belongs together. Order is preserved
   * rather than re-derived, because the list is already sorted the way the user expects and a second
   * sort here would fight it.
   */
  function groupConversations(messages) {
    const groups = new Map();
    for (const message of messages ?? []) {
      const key = conversationKey(message);
      const group = groups.get(key);
      if (group) {
        group.messages.push(message);
      } else {
        groups.set(key, { key, messages: [message] });
      }
    }
    return [...groups.values()];
  }

  global.__easyEmailThreads = { conversationKey, groupConversations, normaliseSubject };
})(typeof window === 'undefined' ? globalThis : window);
