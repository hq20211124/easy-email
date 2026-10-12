/**
 * The pure half of saved searches (`design/09` §1 goal 7, SRCH-05).
 *
 * Split out of `app.js` for the reason `history.js` was: these are the rules a test can check
 * (a name that is the whole query, a truncation that cuts a value in half, a global row mixed in with
 * this account's), and they should not be buried in a thousand-line file where only a human reading
 * carefully can find them. `tools/saved_search_test.mjs` loads this through `vm` the way it loads
 * `history.js` — no DOM, no Tauri.
 */
(function initSavedSearch() {
  /** A sidebar row that is longer than this is a row nobody can read. */
  const NAME_MAX = 18;

  /**
   * A name to offer when the user saves the query in the search box.
   *
   * The query itself when it is short enough, whole tokens after that, and — for one token longer
   * than the whole budget — the **key** plus a shortened value. The rule that matters: the result
   * must never read like a value that got cut in half (`after:2026-0` looks like a bug, and a user
   * who sees it stops trusting the row). An empty query returns an empty name so the caller knows
   * it has to ask the user rather than invent one.
   */
  function suggestName(query) {
    const text = String(query || '').trim();
    if (!text) return '';
    if ([...text].length <= NAME_MAX) return text;

    const tokens = text.split(/\s+/).filter(Boolean);
    const kept = [];
    let used = 0;
    for (const token of tokens) {
      const next = used === 0 ? [...token].length : used + 1 + [...token].length;
      if (next > NAME_MAX) break;
      kept.push(token);
      used = next;
    }
    if (kept.length > 0) {
      // Whole tokens only, and "…" says there was more rather than silently ending the name.
      return `${kept.join(' ')}…`;
    }

    // One token longer than the entire budget. Keep the key if there is one: `filename:` is what
    // tells the user what the search is, and the value is what they would re-type anyway.
    const first = tokens[0];
    const colon = first.indexOf(':');
    if (colon > 0 && colon <= 8) {
      const head = first.slice(0, colon + 1);
      const room = NAME_MAX - [...head].length - 1;
      if (room >= 3) return `${head}${first.slice(colon + 1, colon + 1 + room)}…`;
    }
    return `${[...first].slice(0, NAME_MAX - 1).join('')}…`;
  }

  /**
   * Split the list into the rows scoped to **an** account and the ones shown for every account.
   *
   * Note what this does **not** do: filter by the current account. The backend already did that
   * (`account_id = ? or account_id is null`), so a second filter here would be the kind of thing
   * that looks defensive and quietly disagrees with the server. The names say `account`, not `mine`,
   * for that reason — a field called `mine` containing another account's row is how the next reader
   * concludes the function filters.
   *
   * The UI needs the two apart so it can label them: a global row appearing after an account switch
   * looks like it "followed" the user otherwise, which is exactly what makes people ask whether
   * their data leaked between accounts.
   */
  function splitByScope(list) {
    const account = [];
    const global = [];
    for (const item of Array.isArray(list) ? list : []) {
      if (!item || typeof item !== 'object') continue;
      if (item.account_id) account.push(item);
      else global.push(item);
    }
    return { account, global };
  }

  globalThis.__easyEmailSavedSearch = {
    NAME_MAX,
    suggestName,
    splitByScope,
  };
})();
