/**
 * Windowing arithmetic for the message list (`design/15` §6: virtualise, ± 8 rows of overscan,
 * fixed and measurable row height, commit on `requestAnimationFrame`).
 *
 * It is a separate file, and a pure one, for the same reason `src/history.js` and `src/threads.js`
 * are: the part that can be tested without a window is the part that does not quietly rot. Every
 * function here takes numbers and returns numbers — no DOM, no app state.
 *
 * The shape of the list is a flat sequence of fixed-height rows: one row per conversation, plus one
 * row per message of every expanded conversation (`renderMessages` in `src/app.js`). Fixed height is
 * a **precondition**, not a simplification: with content-sized rows, index → offset needs measuring,
 * and the DOM window has to be rebuilt after every measurement — which is the version where a scroll
 * is a slideshow.
 */
(function (global) {
  /**
   * Row height, in CSS pixels. It has exactly one other home — the `--msg-height` custom property in
   * `src/style.css` — and the two must agree; `tools/check.ps1` fails if one of them disappears.
   */
  const DEFAULT_ROW_HEIGHT = 56;

  /** Rows rendered beyond the viewport on each side (`design/15` §6 says 8). */
  const DEFAULT_OVERSCAN = 8;

  /**
   * Which rows to render, and where to put them.
   *
   * @param {object} input
   * @param {number} input.scrollTop     scroll container's scrollTop (may be negative mid-gesture)
   * @param {number} input.viewportHeight visible height of the scroll container
   * @param {number} input.count         total rows
   * @param {number} [input.rowHeight]
   * @param {number} [input.overscan]
   * @returns {{start: number, end: number, offsetTop: number, totalHeight: number}}
   *   `start`/`end` are a half-open range: render rows `[start, end)`.
   */
  function computeWindow({
    scrollTop,
    viewportHeight,
    count,
    rowHeight = DEFAULT_ROW_HEIGHT,
    overscan = DEFAULT_OVERSCAN,
  }) {
    const rows = Math.max(0, Math.floor(Number(count) || 0));
    const height = Math.max(1, Number(rowHeight) || DEFAULT_ROW_HEIGHT);
    const pad = Math.max(0, Math.floor(Number(overscan) || 0));
    const totalHeight = rows * height;

    if (rows === 0) {
      // No rows: no window, and no NaN sneaking into a style attribute.
      return { start: 0, end: 0, offsetTop: 0, totalHeight: 0 };
    }

    // Clamped because all three of these arrive from the DOM and can be zero or negative on the
    // first frame, before layout has settled.
    const top = Math.max(0, Number(scrollTop) || 0);
    const visible = Math.max(height, Number(viewportHeight) || 0);

    const firstVisible = Math.floor(top / height);
    const lastVisible = Math.ceil((top + visible) / height);

    // At least one visible row even if the viewport is smaller than a row, and never past the end.
    const start = Math.max(0, Math.min(rows - 1, firstVisible - pad));
    const end = Math.max(start + 1, Math.min(rows, lastVisible + pad));

    return { start, end, offsetTop: start * height, totalHeight };
  }

  /**
   * Whether a window changed enough to be worth rebuilding the DOM for.
   *
   * Scroll events arrive far more often than frames (a wheel gesture can fire dozens per frame), so
   * comparing windows is what keeps "one commit per animation frame" true: the handler records the
   * newest scrollTop, and the rAF callback asks this before touching the DOM.
   */
  function windowChanged(previous, next) {
    if (!previous || !next) return true;
    return previous.start !== next.start || previous.end !== next.end;
  }

  /** Scroll offset that brings a row fully into view, given where the viewport starts and ends. */
  function scrollTopForRow(index, { scrollTop, viewportHeight, rowHeight = DEFAULT_ROW_HEIGHT }) {
    const height = Math.max(1, Number(rowHeight) || DEFAULT_ROW_HEIGHT);
    const rowTop = Math.max(0, Math.floor(Number(index) || 0)) * height;
    const rowBottom = rowTop + height;
    const top = Math.max(0, Number(scrollTop) || 0);
    const visible = Math.max(0, Number(viewportHeight) || 0);

    if (rowTop < top) return rowTop;
    if (rowBottom > top + visible) return Math.max(0, rowBottom - visible);
    return top;
  }

  const api = {
    DEFAULT_ROW_HEIGHT,
    DEFAULT_OVERSCAN,
    computeWindow,
    windowChanged,
    scrollTopForRow,
  };

  global.__easyEmailVList = api;
  if (typeof module !== 'undefined' && module.exports) module.exports = api;
})(typeof window === 'undefined' ? globalThis : window);
