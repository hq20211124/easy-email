/**
 * Query the running app's real computed layout over the Chrome DevTools Protocol.
 *
 * Screen measurements can only tell you what things look like; this tells you why. It was what
 * finally explained a layout bug where the panes measured 3363px tall: the grid's implicit row
 * was sized by content and `body { overflow: hidden }` was quietly clipping the result.
 *
 * It can also evaluate inside a child frame, which is the only way to see anything inside the mail
 * body: that iframe is `sandbox`ed without `allow-same-origin`, so it has an opaque origin and the
 * app's own JavaScript cannot touch it. CDP is not bound by the same-origin policy, so an isolated
 * world in that frame can read `img.naturalWidth` and settle "did the remote image actually load?".
 *
 * Requires the app to be built with the debugging port enabled. Add
 * `--remote-debugging-port=9222` to `additionalBrowserArgs` in `src-tauri/tauri.conf.json`,
 * rebuild, and launch. It is deliberately NOT left on by default: any local process can drive the
 * webview through that port.
 *
 *     node tools/inspect-dom.mjs
 *     node tools/inspect-dom.mjs "document.querySelector('#reader').getBoundingClientRect().width"
 *     node tools/inspect-dom.mjs @probe.js        # read the expression from a file
 *     node tools/inspect-dom.mjs --frames         # list the frames in the page
 *     node tools/inspect-dom.mjs --frame=0 @probe.js
 *
 * The `@file` form exists because a multi-step probe (click a message, wait for the body to
 * arrive, then read the DOM) is far too long and quote-heavy to pass as a shell argument.
 * `Runtime.evaluate` runs with `awaitPromise`, so the file may export an async IIFE.
 *
 * Node 22+ has a built-in WebSocket, so there is no dependency to install.
 */

import { readFileSync } from 'node:fs';

const PORT = 9222;

async function findPageTarget() {
  const response = await fetch(`http://127.0.0.1:${PORT}/json`);
  const targets = await response.json();
  const page = targets.find((t) => t.type === 'page' && t.webSocketDebuggerUrl);
  if (!page) {
    throw new Error(
      `no page target on :${PORT}. Is the app running and built with --remote-debugging-port?`
    );
  }
  return page;
}

/** Minimal CDP client: one socket, a promise per request id. */
function connect(ws) {
  let nextId = 1;
  const pending = new Map();

  ws.addEventListener('message', (event) => {
    const message = JSON.parse(event.data);
    const entry = pending.get(message.id);
    if (!entry) return;
    pending.delete(message.id);
    if (message.error) entry.reject(new Error(JSON.stringify(message.error)));
    else entry.resolve(message.result);
  });

  return (method, params = {}) =>
    new Promise((resolve, reject) => {
      const id = nextId++;
      pending.set(id, { resolve, reject });
      ws.send(JSON.stringify({ id, method, params }));
    });
}

const DEFAULT_PROBE = `(() => {
  const info = (sel) => {
    const el = document.querySelector(sel);
    if (!el) return { selector: sel, missing: true };
    const cs = getComputedStyle(el);
    const r = el.getBoundingClientRect();
    return {
      selector: sel,
      width: Math.round(r.width * 10) / 10,
      height: Math.round(r.height * 10) / 10,
      display: cs.display,
      gridTemplateColumns: cs.gridTemplateColumns,
      minWidth: cs.minWidth,
      flex: cs.flex,
      overflow: cs.overflow,
    };
  };
  return {
    devicePixelRatio: window.devicePixelRatio,
    innerWidth: window.innerWidth,
    innerHeight: window.innerHeight,
    panes: ['#app', '#sidebar', '#list-pane', '#reader'].map(info),
  };
})()`;

const argv = process.argv.slice(2);
const flags = argv.filter((a) => a.startsWith('--'));
const positional = argv.filter((a) => !a.startsWith('--'));
const rawArg = positional[0];

const frameFlag = flags.find((f) => f.startsWith('--frame='));
const listFrames = flags.includes('--frames');

const expression = !rawArg
  ? DEFAULT_PROBE
  : rawArg.startsWith('@')
    ? readFileSync(rawArg.slice(1), 'utf8')
    : rawArg;

const target = await findPageTarget();
const ws = new WebSocket(target.webSocketDebuggerUrl);

await new Promise((resolve, reject) => {
  ws.addEventListener('open', resolve);
  ws.addEventListener('error', reject);
});

const send = connect(ws);
await send('Page.enable');

/** Flatten the frame tree, main frame first. */
function flatten(node, depth = 0, out = []) {
  out.push({ frameId: node.frame.id, url: node.frame.url, depth, name: node.frame.name });
  for (const child of node.childFrames ?? []) flatten(child, depth + 1, out);
  return out;
}

const tree = await send('Page.getFrameTree');
const frames = flatten(tree.frameTree);

if (listFrames) {
  console.log(JSON.stringify(frames, null, 2));
  ws.close();
  process.exit(0);
}

const index = frameFlag ? Number(frameFlag.split('=')[1]) + 1 : 0; // +1 skips the main frame
const chosen = frames[index];
if (!chosen) {
  throw new Error(`no frame at index ${frameFlag}. Use --frames to list them.`);
}

const params = { expression, returnByValue: true, awaitPromise: true };
if (index > 0) {
  // An isolated world is required for a sandboxed frame: the main world belongs to an opaque
  // origin the debugger cannot otherwise evaluate into.
  const world = await send('Page.createIsolatedWorld', {
    frameId: chosen.frameId,
    worldName: 'dsh-probe',
  });
  params.contextId = world.executionContextId;
}

const result = await send('Runtime.evaluate', params);
console.log(JSON.stringify(result.result?.value ?? result, null, 2));
ws.close();
