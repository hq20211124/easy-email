/**
 * The window has no native title bar any more, so this file does the three things that used to come
 * with it: minimize, maximize/restore, close — and the drag region is declared in the markup
 * (`data-tauri-drag-region`), because Tauri handles that one itself.
 *
 * The buttons are wired here rather than inline so they keep working after a re-render of anything
 * else, and so the capability list stays the only place that decides what the window may do.
 */
function wireWindowControls() {
  const api = window.__TAURI__?.window;
  if (!api) return;

  const current = api.getCurrentWindow();
  const bind = (id, action) => {
    const button = document.getElementById(id);
    if (button) button.addEventListener('click', action);
  };

  bind('win-min', () => current.minimize().catch(() => {}));
  bind('win-max', () => current.toggleMaximize().catch(() => {}));
  bind('win-close', () => current.close().catch(() => {}));

  // The glyph follows the real state rather than the click, so a window maximized by double-clicking
  // the drag region (which the OS handles) still shows the right icon.
  const refresh = () => {
    current
      .isMaximized()
      .then((maximized) => {
        const button = document.getElementById('win-max');
        if (button) {
          button.textContent = maximized ? '\u2750' : '\u25A2';
          button.title = maximized ? '还原' : '最大化';
        }
      })
      .catch(() => {});
  };

  refresh();
  window.addEventListener('resize', refresh);
}

wireWindowControls();
