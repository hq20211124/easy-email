/**
 * Take a screenshot of the running app, so a layout can be *looked at* rather than inferred.
 *
 *     node tools/screenshot.mjs [output.png]
 *
 * Uses `Page.captureScreenshot` over CDP, which needs the debugging port (see `inspect-dom.mjs` for
 * how to enable it). Node 22+ has a built-in WebSocket, so there is nothing to install.
 *
 * Why this exists next to `inspect-dom.mjs`: measurements say a pane is 240px wide and nothing overlaps,
 * and the result can still look wrong — the difference between "the numbers are right" and "it looks
 * right" is a picture, and the person reporting a layout problem is looking at a picture.
 */

import { writeFileSync } from 'node:fs';

const PORT = 9222;
const output = process.argv[2] ?? 'screenshot.png';

async function findPageTarget() {
  const response = await fetch(`http://127.0.0.1:${PORT}/json`);
  const targets = await response.json();
  const page = targets.find((t) => t.type === 'page' && t.webSocketDebuggerUrl);
  if (!page) throw new Error(`no page target on :${PORT}`);
  return page;
}

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

const target = await findPageTarget();
const ws = new WebSocket(target.webSocketDebuggerUrl);
await new Promise((resolve, reject) => {
  ws.addEventListener('open', resolve);
  ws.addEventListener('error', reject);
});

const send = connect(ws);
await send('Page.enable');

const { data } = await send('Page.captureScreenshot', { format: 'png', captureBeyondViewport: false });
writeFileSync(output, Buffer.from(data, 'base64'));

const { result } = await send('Runtime.evaluate', {
  expression: '({ width: window.innerWidth, height: window.innerHeight, title: document.title })',
  returnByValue: true,
});
console.log(JSON.stringify({ output, ...result.value }, null, 2));
ws.close();
