/**
 * Does the mail body's HTML actually render, and did its images actually load?
 *
 * Run this inside the mail body frame:
 *
 *     node tools/inspect-dom.mjs --frame=0 @tools/probe-mail-body.mjs
 *
 * Requires the app to be built with `--remote-debugging-port=9222` in `additionalBrowserArgs`
 * (see the note at the top of inspect-dom.mjs); remove it again afterwards.
 *
 * Why this exists as a tool rather than a one-off: the body's remote images are gated by two CSPs
 * that intersect — the shell's and the frame's own — and a `srcdoc` frame inherits the embedder's
 * policy. Getting that wrong is silent: the "显示图片" opt-in changes the inner policy, the UI
 * correctly hides the warning bar, and every image still fails. `complete: true` with
 * `naturalWidth: 0` is the signature, and there is no way to see it from the app's own JavaScript
 * because the `sandbox` iframe has an opaque origin.
 *
 * Expected with remote images NOT enabled: `failed` equals `imageCount` minus any `data:` images.
 * Expected after clicking 显示图片: `failed: 0`.
 */
(() => {
  const de = document.documentElement;
  const images = [...document.images].map((img) => ({
    src: (img.currentSrc || img.src).slice(0, 70),
    complete: img.complete,
    naturalWidth: img.naturalWidth,
    renderedW: Math.round(img.getBoundingClientRect().width),
  }));

  return {
    viewport: { w: de.clientWidth, h: de.clientHeight },
    content: { w: de.scrollWidth, h: de.scrollHeight },
    // True means the message is wider than the pane and the frame will scroll sideways; fixed-width
    // newsletters do this, and it is not an app bug.
    horizontallyOverflows: de.scrollWidth > de.clientWidth + 1,
    imageCount: images.length,
    loaded: images.filter((i) => i.naturalWidth > 0).length,
    failed: images.filter((i) => i.complete && i.naturalWidth === 0).length,
    pending: images.filter((i) => !i.complete).length,
    // A 1x1 with a per-recipient URL is a tracking pixel; loading it reports "opened".
    trackingPixelSuspects: images.filter((i) => i.naturalWidth === 1).map((i) => i.src),
    samples: images.slice(0, 5),
  };
})()
