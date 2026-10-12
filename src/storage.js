/**
 * The pure half of the storage panel (`design/03` §5.4 设置页留痕).
 *
 * Split out of `app.js` for the reason `saved-search.js` was: these are the rules a test can check
 * (what an empty input means, how a byte count reads, what a cleanup says it did), and they should
 * not be buried in a thousand-line file where only a careful human finds them.
 * `tools/storage_test.mjs` loads this through `vm` the way it loads `history.js`.
 */
(function initStorage() {
  /** The default cap. Larger than any sane mail cache, smaller than a full disk. */
  const DEFAULT_LIMIT_MB = 500;

  /**
   * What the cap box should hold, as a number of megabytes.
   *
   * Anything that is not a positive number falls back to the default rather than to zero — a cap of
   * zero means "clean everything, always", which is the one value a typo must not be able to
   * produce. That is the whole rule, and it is why this is a function with a test rather than
   * `Number(input.value)` at the call site.
   */
  function suggestStorageLimit(input) {
    const n = Number(String(input ?? '').trim());
    if (!Number.isFinite(n) || n <= 0) return DEFAULT_LIMIT_MB;
    // Above a terabyte the box is holding a mistake, not a policy.
    return Math.min(Math.round(n), 1_000_000);
  }

  /**
   * Bytes as something a person reads.
   *
   * One decimal place, and never "0.0 MB" for something that is not zero: a reader who sees 0.0
   * where there is 400 KB concludes the number is broken.
   */
  function formatBytes(bytes) {
    const n = Number(bytes);
    if (!Number.isFinite(n) || n <= 0) return '0 MB';
    const units = [
      [1024 ** 4, 'TB'],
      [1024 ** 3, 'GB'],
      [1024 ** 2, 'MB'],
      [1024, 'KB'],
    ];
    for (const [size, label] of units) {
      if (n >= size) {
        const value = n / size;
        const rounded = value >= 100 ? Math.round(value) : Math.round(value * 10) / 10;
        return `${rounded} ${label}`;
      }
    }
    return `${n} 字节`;
  }

  /**
   * What a dry run says, in the words the panel shows.
   *
   * An empty plan says so plainly instead of showing a zero: "0 MB" reads like the cleanup does
   * nothing at all, when the truth is that nothing needed cleaning — which is the good case.
   */
  function cleanupSummary(plan) {
    const steps = Array.isArray(plan?.steps) ? plan.steps : [];
    if (steps.length === 0) {
      return { text: '当前不需要清理。', actionable: false };
    }
    const rows = steps.reduce((sum, step) => sum + (step.rows || 0), 0);
    const freed = steps.reduce((sum, step) => sum + (step.freed_bytes || 0), 0);
    return {
      text: `将释放 ${formatBytes(freed)}，涉及 ${rows} 封邮件` + '的已下载正文。',
      actionable: true,
    };
  }

  globalThis.__easyEmailStorage = {
    DEFAULT_LIMIT_MB,
    suggestStorageLimit,
    formatBytes,
    cleanupSummary,
  };
})();
