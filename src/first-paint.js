/* Easy Email — what the mail list shows before the network has answered.
 *
 * Arriving at a folder used to await one command, `list_messages`, which refreshes the folder over
 * IMAP and only then reads the cache. So the screen sat on 「正在收取邮件…」 for a whole round trip —
 * tens of milliseconds of local query hidden behind seconds of network — even though the rows it was
 * waiting for were already on this machine. The list now reads the cache first (`message_page`, no
 * network) and paints, then lets the refresh repaint. That trades one thing: the rows on screen for
 * the first moment may be as old as the last sync.
 *
 * Two decisions in that trade are invisible unless you look for them, which is what this file is for:
 *
 *   1. **an empty cache is not an empty folder.** Painting what `message_page` returned when it
 *      returned nothing renders 「这个文件夹里还没有邮件。」 for a folder that has never been synced —
 *      a statement about the server, asserted from the absence of a local file. The spinner is the
 *      only honest thing to show then.
 *   2. **a refresh that fails must not wipe the list.** With network-first, "收取失败" replaced a
 *      screen that had nothing on it yet. With cache-first it would replace *rows the user could
 *      already read*, and it does: reading mail that is on this machine has nothing to do with the
 *      connection that just failed. The banner (`refreshSyncBanner`) is what says why.
 *
 * Painting the cache also opens a window that did not exist before: rows from disk are on screen
 * looking like the server's current state. The pane title carries 「（正在收取…）」 for exactly as
 * long as that is true — not `#sync-banner`, whose meaning is "the last refresh *failed*", and nothing
 * has failed yet.
 *
 * `folderStep` is the same trade one level up, applied to the folder list that must be chosen before any
 * row can be painted; see its own comment.
 *
 * `tools/first_paint_test.mjs` pins both, and the case that is only wrong occasionally — a response
 * for a folder the user has already left.
 */
(function (global) {
  'use strict';

  /**
   * @param {{pageCount: number}} input — rows the cache leg returned for this folder (`message_page`
   * answered with a page, the same question 「加载更多」 asks).
   * @returns {'rows' | 'spinner'}
   */
  function cacheStep({ pageCount }) {
    return Number(pageCount) > 0 ? 'rows' : 'spinner';
  }

  /**
   * What the refresh's answer should do to the screen.
   *
   * `cacheShown` is whether the cache leg painted rows, not whether the list is non-empty: the two
   * differ exactly when the cache leg failed too, and that is the one case where "收取失败" is still
   * the truth.
   *
   * @param {{stale: boolean, failed: boolean, cacheShown: boolean}} input
   * @returns {'drop' | 'rows' | 'keep-cache' | 'error'}
   */
  function freshStep({ stale, failed, cacheShown }) {
    if (stale) return 'drop';
    if (failed) return cacheShown ? 'keep-cache' : 'error';
    return 'rows';
  }

  /**
   * Where the *folder* list should come from on the way to the first row — the same rule one level up.
   *
   * `list_mailboxes` asks the server for `LIST`, and at cold start that question pays for a fresh IMAP
   * connection too: measured at 461 ms sitting in front of the first row, while `cached_mailboxes`
   * answered the same thing off this machine in 7–12 ms. Zero cached folders is the
   * one case where the server genuinely is the only source, and that is also the only case where the
   * pane may still be a spinner.
   *
   * @param {{folderCount: number}} input — selectable folders the cache knows for this account
   * @returns {'cache' | 'server'}
   */
  function folderStep({ folderCount }) {
    return Number(folderCount) > 0 ? 'cache' : 'server';
  }

  /**
   * What the deferred `LIST` should do to a sidebar that is already showing folders.
   *
   * Same rule as `freshStep`, one level up: once rows are on screen, a network failure has destroyed
   * nothing. The folders the user can click are still the ones from the last sync — and `run()` answers
   * with `null` rather than a list, so the failure has to be read from that, not from a length.
   *
   * @param {{failed: boolean}} input — whether the server answered at all
   * @returns {'keep' | 'replace'}
   */
  function folderRefreshStep({ failed }) {
    return failed ? 'keep' : 'replace';
  }

  global.__easyEmailFirstPaint = { cacheStep, freshStep, folderStep, folderRefreshStep };
})(typeof window === 'undefined' ? globalThis : window);
