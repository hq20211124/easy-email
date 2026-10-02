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

/** Icon shows where the button takes you, not where you are: a light UI offers the moon. */
const ICON_FOR = { light: '🌙', dark: '☀️' };
const LABEL_FOR = { light: '切换到深色模式', dark: '切换到浅色模式' };

function readStored() {
  try {
    const value = localStorage.getItem(THEME_KEY);
    return THEMES.includes(value) ? value : null;
  } catch {
    // Private mode or a locked-down profile: fall through to the default rather than break boot.
    return null;
  }
}

function currentTheme() {
  const applied = document.documentElement.dataset.theme;
  if (THEMES.includes(applied)) return applied;
  return readStored() ?? DEFAULT_THEME;
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

/*
  Bootstrap. Deliberately applies without persisting: writing the resolved default back to
  localStorage would erase the difference between "never chose" and "chose light", which is exactly
  what a future "follow system" option needs to tell apart. It also avoids a pointless write on
  every single launch. The theme is applied either way, so the visible behaviour is identical.
*/
applyTheme(currentTheme(), { persist: false });

window.__easyEmailTheme = {
  current: currentTheme,
  set: applyTheme,
  toggle: () => applyTheme(currentTheme() === 'dark' ? 'light' : 'dark'),
  iconFor: (theme) => ICON_FOR[theme],
  labelFor: (theme) => LABEL_FOR[theme],
};
