/* Easy Email — theme selection.
 *
 * Loaded synchronously from <head>, before the body exists. That placement is the whole point: the
 * document must already carry the right `data-theme` when the first frame is painted, otherwise a
 * user who chose dark sees a flash of light on every launch. A separate file is required because the
 * shell's CSP is `script-src 'self'`, so an inline snippet would be blocked.
 *
 * Light is the default. This deliberately does NOT consult `prefers-color-scheme`: the app used to
 * follow the OS, which meant a machine set to dark had no way to get a light UI. The trade-off is
 * that a first-run user on a dark desktop gets light — accepted, because it is what "白天模式为默认"
 * means. A "follow system" third state can be added later without touching callers.
 *
 * `style.css` holds the actual palette; this file only decides which one applies.
 */

const THEME_KEY = 'easy-email.theme';
const THEMES = ['light', 'dark'];
const DEFAULT_THEME = 'light';

/*
  The accent colour is a second, independent axis: `data-theme` picks light or dark, `data-accent`
  picks which palette sits on top of it. Keeping them separate is why `style.css` needs only four
  small overrides (lavender and sky, each with a light and a dark variant) rather than a full
  cross-product of six hand-written blocks — and it means adding a seventh accent later touches one
  attribute, not the light/dark logic. `original` is deliberately given no override: it is what the
  bare `:root` already is.
*/
const ACCENT_KEY = 'easy-email.accent';
const ACCENTS = ['original', 'lavender', 'sky'];
const DEFAULT_ACCENT = 'original';
const ACCENT_LABEL = { original: '默认蓝', lavender: '薰衣草', sky: '天蓝' };

/** Icon shows where the button takes you, not where you are: a light UI offers the moon. */
const ICON_FOR = { light: '🌙', dark: '☀️' };
const LABEL_FOR = { light: '切换到深色模式', dark: '切换到浅色模式' };

function readStored(key, allowed, fallback) {
  try {
    const value = localStorage.getItem(key);
    return allowed.includes(value) ? value : fallback;
  } catch {
    // Private mode or a locked-down profile: fall through to the default rather than break boot.
    return fallback;
  }
}

function currentTheme() {
  const applied = document.documentElement.dataset.theme;
  if (THEMES.includes(applied)) return applied;
  return readStored(THEME_KEY, THEMES, DEFAULT_THEME);
}

function currentAccent() {
  const applied = document.documentElement.dataset.accent;
  if (ACCENTS.includes(applied)) return applied;
  return readStored(ACCENT_KEY, ACCENTS, DEFAULT_ACCENT);
}

/**
 * @param {string} theme
 * @param {{persist?: boolean}} [options] `persist: false` is for the bootstrap call only.
 */
function applyTheme(theme, { persist = true } = {}) {
  const next = THEMES.includes(theme) ? theme : DEFAULT_THEME;
  document.documentElement.dataset.theme = next;
  if (persist) {
    try {
      localStorage.setItem(THEME_KEY, next);
    } catch {
      /* not being able to remember the choice is not worth failing the launch over */
    }
  }
  return next;
}

/** The accent half, mirroring `applyTheme`: the same no-persist-on-bootstrap rule applies. */
function applyAccent(accent, { persist = true } = {}) {
  const next = ACCENTS.includes(accent) ? accent : DEFAULT_ACCENT;
  document.documentElement.dataset.accent = next;
  if (persist) {
    try {
      localStorage.setItem(ACCENT_KEY, next);
    } catch {
      /* same as the theme: forgetting the choice is not worth failing the launch over */
    }
  }
  return next;
}

/** Advance to the next accent in `ACCENTS`; the button cycles rather than opens a menu to stay one
    icon wide in the compact brand row. */
function cycleAccent() {
  const from = currentAccent();
  const next = ACCENTS[(ACCENTS.indexOf(from) + 1) % ACCENTS.length];
  return applyAccent(next);
}

/*
  Bootstrap. Deliberately applies without persisting: writing the resolved default back to
  localStorage would erase the difference between "never chose" and "chose light", which is exactly
  what a future "follow system" option needs to tell apart. It also avoids a pointless write on
  every single launch. The theme is applied either way, so the visible behaviour is identical.
*/
applyTheme(currentTheme(), { persist: false });
applyAccent(currentAccent(), { persist: false });

window.__easyEmailTheme = {
  current: currentTheme,
  set: applyTheme,
  toggle: () => applyTheme(currentTheme() === 'dark' ? 'light' : 'dark'),
  iconFor: (theme) => ICON_FOR[theme],
  labelFor: (theme) => LABEL_FOR[theme],
  accent: currentAccent,
  setAccent: applyAccent,
  cycleAccent,
  accentLabel: (accent) => ACCENT_LABEL[accent] || accent,
  accents: () => ACCENTS.slice(),
};
