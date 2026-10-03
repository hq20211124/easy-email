/**
 * Does the list really show conversations, and does the count match what was folded?
 *
 *     node tools/inspect-dom.mjs @tools/probe-threads.mjs
 *
 * Requires the app to be built with `--remote-debugging-port=9222` in `additionalBrowserArgs`
 * (see the note at the top of inspect-dom.mjs); remove it again afterwards.
 *
 * Why this exists on top of `tools/thread_keys_test.mjs`: that file proves the *key* is right; this
 * proves the list uses it. The two can disagree — a renderer that keeps one row per message while
 * the key sits in a module is exactly the kind of drift the payload gate was written for, and the
 * check is self-consistent: the rows rendered must equal the groups computed from the same array.
 */
(async () => {
  const rows = () => {
    const list = document.getElementById('message-list');
    return [...list.querySelectorAll('.msg')].map((row) => ({
      subject: row.querySelector('.msg-subject')?.textContent ?? '',
      count: row.querySelector('.msg-count')?.textContent ?? '',
    }));
  };

  if (!state.accountId || state.messages.length === 0) {
    return { failed: 'no cached mail on this profile to list' };
  }

  const expected = threads.groupConversations(state.messages).length;
  const rendered = rows();

  // Every row's badge has to match the size of a group, not just "some number".
  const groups = threads.groupConversations(state.messages).map((group) => group.messages.length);
  const badges = rendered.filter((row) => row.count !== '').map((row) => Number(row.count));
  const sorted = (values) => [...values].sort((a, b) => a - b);

  return {
    messages: state.messages.length,
    exactKeys: state.messages.filter((m) => typeof m.thread_key === 'string' && m.thread_key)
      .length,
    fallbackOnly: state.messages.filter((m) => !m.thread_key).length,
    expectedRows: expected,
    renderedRows: rendered.length,
    hasCounts: badges.length > 0,
    badges: sorted(badges),
    multiMessageGroups: sorted(groups.filter((size) => size > 1)),
    sample: rendered.slice(0, 4),
    passed:
      rendered.length === expected &&
      JSON.stringify(sorted(badges)) === JSON.stringify(sorted(groups.filter((size) => size > 1))),
  };
})()
