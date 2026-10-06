/* Easy Email — what one press of 加载更多 should do.
 *
 * The list is backed by a local cache that holds **more than one page**: the first sync stores a whole
 * window (500) while the list shows a page (50), and every history fetch adds to the cache as well.
 * That leaves two sources of "older mail", and pressing the button has to try them in the right order:
 *
 *   1. **the cache first.** Mail already on this machine must be reachable offline, must not need a
 *      round trip, and — the failure this file exists for — must not be skipped. The first version
 *      went straight to the server, so on a mailbox whose server has nothing older, the button said
 *      "没有更早的邮件了" while 142 messages sat in `cache.db` that the UI never showed.
 *   2. **the server after that**, for the next page beyond what is cached.
 *   3. **done** only when both are exhausted.
 *
 * The rule is a pure function of three counts so `tools/history_test.mjs` can pin the order, which is
 * invisible on screen: both branches end with more rows in the list, and only one of them works
 * offline.
 */
(function (global) {
  'use strict';

  /**
   * Which source the next page comes from.
   *
   * `shownCount` is what the list is displaying, `cachedCount` what the local cache holds for this
   * folder, `serverDone` whether a previous fetch already reported the bottom of the mailbox.
   */
  function historyStep({ shownCount, cachedCount, serverDone }) {
    // Strictly greater: equal counts mean the list already shows everything cached, so there is
    // nothing to reveal locally and the server is the only place left to look.
    if (Number(cachedCount) > Number(shownCount)) return 'show-cached';
    if (serverDone) return 'done';
    return 'ask-server';
  }

  global.__easyEmailHistory = { historyStep };
})(typeof window === 'undefined' ? globalThis : window);
