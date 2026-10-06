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
   *
   * This is one message's *identity*; the grouping itself is [`groupConversations`], which unions it
   * with the second signal the backend stores.
   */
  function conversationKey(message) {
    // The exact key first: the backend computes it from the `References` chain when it parses the
    // headers (`crate::threads::root_of`) and stores it beside the row. It is the one key that joins
    // a reply to the message it answers *and* to the message that started the thread, including a
    // reply the user sent — which is exactly what the fallback below cannot do.
    const exact = message.thread_key;
    if (typeof exact === 'string' && exact.length > 0) {
      return exact;
    }

    // The heuristic the backend also stores, for mail with no chain at all: sender plus normalised
    // subject. Present only for mail the user received (`crate::threads::subject_key`).
    const heuristic = message.subject_key;
    if (typeof heuristic === 'string' && heuristic.length > 0) {
      return heuristic;
    }

    const subject = normaliseSubject(message.subject);
    if (!subject) {
      return `solo:${message.folder ?? ''}:${message.uid ?? ''}`;
    }
    return `${counterpart(message)}|${subject}`;
  }

  /**
   * The same window the backend uses to bound the heuristic (`THREAD_SESSION_GAP_SECS` in
   * `cache.rs`): a week.
   *
   * A bound is not optional. The case that asked for the heuristic is four messages inside a day,
   * while recurring notifications reuse one subject for years — Google's 安全提醒 arrived 24 times
   * over 4.7 years. Without the window the list would fold all of those into one row.
   */
  const SESSION_GAP_SECONDS = 7 * 24 * 60 * 60;

  /**
   * The subject-and-sender signal, one per **session** of same-subject mail.
   *
   * Consecutive messages chain while the gap stays inside the window, which is what makes "a burst of
   * mail under one subject" one conversation and "the same subject next month" another.
   */
  function sessionSignals(messages, baseKeyOf) {
    const byKey = new Map();
    for (const message of messages) {
      const key = baseKeyOf(message);
      if (!key) continue;
      const rows = byKey.get(key);
      if (rows) rows.push(message);
      else byKey.set(key, [message]);
    }

    const signals = new Map();
    for (const [key, rows] of byKey) {
      const ordered = rows
        .slice()
        .sort((a, b) => (a.date ?? 0) - (b.date ?? 0) || (a.uid ?? 0) - (b.uid ?? 0));
      let session = 0;
      let previous = null;
      for (const row of ordered) {
        const date = row.date ?? 0;
        if (previous !== null && date - previous > SESSION_GAP_SECONDS) session += 1;
        previous = date;
        signals.set(row, `s:${key}#${session}`);
      }
    }
    return signals;
  }

  /**
   * The messages grouped into conversations, newest first, with the newest message of each group
   * first inside the group.
   *
   * **Two signals, unioned.** The `References` chain (`thread_key`) is a fact; the sender-and-subject
   * key (`subject_key`, bounded to a week-long session) is a heuristic for mail that carries no chain —
   * which is what folds the four separate messages a correspondent sent under one subject into one row
   * (reported 2026-10-06). Union rather than "the exact key wins", because a thread whose subject was
   * edited mid-way and a thread whose chain reaches further back than the cache both keep grouping:
   * the heuristic can only ever add rows to a conversation, never take them away.
   *
   * The caller decides what to show; this only decides what belongs together. Order is preserved
   * rather than re-derived, because the list is already sorted the way the user expects and a second
   * sort here would fight it.
   */
  function groupConversations(messages) {
    const list = messages ?? [];

    /** The heuristic's base key for a row, or `null` when the row must not take part in it. */
    const baseKeyOf = (message) => {
      const stored = message.subject_key;
      if (typeof stored === 'string' && stored.length > 0) return stored;
      // A row that has an exact key but no stored heuristic key is one the backend deliberately left
      // out (mail the user sent): it groups by its chain and nothing else.
      if (typeof message.thread_key === 'string' && message.thread_key.length > 0) return null;
      // Neither column existed when this row was written: the renderer's own guess, which is what the
      // conversation view falls back to as well.
      const subject = normaliseSubject(message.subject);
      const who = counterpart(message);
      return subject && who ? `${who}|${subject}` : null;
    };
    const sessions = sessionSignals(list, baseKeyOf);

    /** Union-find over signal names; `parent.get(x) === x` marks a root. */
    const parent = new Map();
    const add = (key) => {
      if (!parent.has(key)) parent.set(key, key);
      return key;
    };
    const find = (key) => {
      let root = key;
      while (parent.get(root) !== root) root = parent.get(root);
      let walk = key;
      while (walk !== root) {
        const next = parent.get(walk);
        parent.set(walk, root);
        walk = next;
      }
      return root;
    };
    const union = (a, b) => {
      add(a);
      add(b);
      const rootA = find(a);
      const rootB = find(b);
      if (rootA !== rootB) parent.set(rootB, rootA);
    };

    // Two passes on purpose: union **all** the signals first, then group. Grouping while merging
    // would key the earlier rows to a root that a later union moves, splitting one conversation into
    // two — which is exactly what this did until the test for it failed.
    const rows = [];
    for (const message of list) {
      const signals = [];
      const exact = message.thread_key;
      if (typeof exact === 'string' && exact.length > 0) signals.push(`t:${exact}`);
      const session = sessions.get(message);
      if (session) signals.push(session);
      // A row with no signal at all keeps a group of its own, exactly as before: two bounces have
      // nothing to match on, and folding them together would hide mail.
      const own =
        signals.length > 0 ? signals[0] : `solo:${message.folder ?? ''}:${message.uid ?? ''}`;
      add(own);
      for (const signal of signals) union(own, signal);
      rows.push({ message, own });
    }

    const groups = new Map();
    for (const { message, own } of rows) {
      const root = find(own);
      const group = groups.get(root);
      if (group) {
        group.messages.push(message);
      } else {
        // The raw signal travels on as `key`: it is what the conversation view is opened with, and a
        // chain key is always the right thing to hand it (the backend follows the chain, and expands
        // to the session from there).
        groups.set(root, { key: own.replace(/^[ts]:/, '').replace(/#\d+$/, ''), messages: [message] });
      }
    }
    return [...groups.values()];
  }

  global.__easyEmailThreads = {
    conversationKey,
    groupConversations,
    normaliseSubject,
    sessionSignals,
  };
})(typeof window === 'undefined' ? globalThis : window);
