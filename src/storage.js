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

  /**
   * What the 正文存储 row says, given what the running app reports.
   *
   * Two facts, never one: the mode this process is honouring, and how many bodies are already on
   * disk. Showing only the mode would let "正文不落盘" sit next to three thousand stored bodies and
   * read as though the file were clean — the count is what turns 清空 into a decision instead of a
   * guess. A report we cannot read is reported as the default mode with the count unknown, because
   * the panel must still render.
   */
  function bodyStorageSummary(report) {
    const raw = Number(report?.stored_bodies);
    const stored = Number.isFinite(raw) && raw > 0 ? Math.round(raw) : 0;
    const online = report?.mode === 'online';
    return {
      online,
      stored,
      label: online ? '正文不落盘' : '正文缓存在本地',
      // Nothing to clear when the file holds no bodies; the button would be a lie.
      canPurge: stored > 0,
      note: online
        ? '正文只在打开这封信时下载到内存，不写进本地缓存文件。代价要说清楚：没读过的邮件离线打不开，' +
          '按正文里的词搜索也只覆盖清空之前存下的部分。'
        : '读过的邮件会把正文写进本地缓存文件，之后离线可读、按正文词可搜。',
    };
  }

  /**
   * The disk-encryption self-check, in the words the panel prints.
   *
   * The backend sends the state as a machine word (`protected`, `not_encrypted`, …) and the note as
   * its own sentence; this maps only the words. A state that is not in the table shows as 无法判定
   * rather than as the raw identifier, because the one reading that must never appear on this screen
   * is an unknown new value rendered like a known good one.
   */
  const DISK_STATE_LABELS = {
    protected: '已加密且保护已开启',
    encrypted_unprotected: '已加密但保护已关闭',
    not_encrypted: '未加密',
    converting: '加密进行中',
    unknown: '无法判定',
  };

  function diskProtectionSummary(report) {
    const labelOf = (state) => DISK_STATE_LABELS[state] ?? '无法判定';
    return {
      verdict: labelOf(report?.verdict),
      // Whether every volume the query saw is encrypted *with protection on*. Anything else —
      // including a status we could not read — is not "yes", and the panel says so in one colour.
      allProtected: report?.verdict === 'protected',
      note: typeof report?.note === 'string' ? report.note : '',
      volumes: (Array.isArray(report?.volumes) ? report.volumes : []).map(
        (v) => `${v.drive}：${labelOf(v.state)}`
      ),
    };
  }

  globalThis.__easyEmailStorage = {
    DEFAULT_LIMIT_MB,
    suggestStorageLimit,
    formatBytes,
    cleanupSummary,
    bodyStorageSummary,
    diskProtectionSummary,
  };
})();
