/**
 * Probe for the link bar (`design/14` §5.3): a message body must not be able to navigate the reading
 * pane, and every link must be openable deliberately, labelled with the host it will really visit.
 *
 *     node tools/probe-links.mjs
 *
 * Requires the app to be built with `--remote-debugging-port=9222` in `additionalBrowserArgs` and
 * running — see `inspect-dom.mjs` for the recipe (and put the flag back afterwards; `tools/check.ps1`
 * fails the build if it is left in).
 *
 * What it asserts, in the running renderer rather than in a unit test:
 *   1. a link in a body carries no followable `href`, only `data-ee-href`;
 *   2. the link bar renders one chip per link, and the chip names the real host;
 *   3. `classify_link` reaches Rust and answers with the risk the sender's markup deserves;
 *   4. `open_link` refuses a dangerous scheme with a sentence, rather than opening anything.
 *
 * Assertions 3 and 4 are the ones worth having: they cross the IPC boundary, which no offline test in
 * this repository can do.
 */

const PORT = 9222;

async function findPageTarget() {
  const response = await fetch(`http://127.0.0.1:${PORT}/json`);
  const targets = await response.json();
  const page = targets.find((t) => t.type === 'page' && t.webSocketDebuggerUrl);
  if (!page) throw new Error(`no page target on :${PORT}. Is the app running and built with the flag?`);
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

async function evaluate(expression) {
  const { result, exceptionDetails } = await send('Runtime.evaluate', {
    expression,
    awaitPromise: true,
    returnByValue: true,
  });
  if (exceptionDetails) throw new Error(JSON.stringify(exceptionDetails));
  return result.value;
}

const failures = [];
function check(name, condition, detail) {
  if (condition) {
    console.log(`ok   ${name}`);
  } else {
    failures.push(name);
    console.log(`FAIL ${name}${detail === undefined ? '' : ` — ${JSON.stringify(detail)}`}`);
  }
}

// A body as the sanitiser leaves it: the targets are in `data-ee-href`, and there is no `href` for
// the frame to follow. Built here rather than fetched, so the probe needs no mailbox.
const body = [
  '<p>账单见附件</p>',
  '<a data-ee-href="https://example.com/invoice?id=7">example.com 的账单</a>',
  '<a data-ee-href="https://bit.ly/3xYz">短链接</a>',
  '<a data-ee-href="javascript:alert(1)">点我</a>',
].join('');

const rendered = await evaluate(`
  (() => {
    updateLinkBar(${JSON.stringify(body)});
    const bar = document.getElementById('link-bar');
    return {
      links: linksInHtml(${JSON.stringify(body)}),
      hidden: bar.hidden,
      chips: [...bar.querySelectorAll('.link-chip')].map((c) => c.textContent),
      frameSandbox: document.getElementById('body-frame').getAttribute('sandbox'),
      frameHasHref: /<a[^>]*\\shref=/i.test(${JSON.stringify(body)}),
    };
  })()
`);

check('the link bar found every link', rendered.links.length === 3, rendered.links);
check('the link bar is visible', rendered.hidden === false);
check('one chip per link', rendered.chips.length === 3, rendered.chips);
check(
  'a chip names the host it will really visit',
  rendered.chips.some((c) => c.includes('example.com')) && rendered.chips.some((c) => c.includes('bit.ly')),
  rendered.chips
);
check('the body carries no followable href', rendered.frameHasHref === false);

// The IPC boundary: the classification must come from Rust, not from the renderer's own guess.
const verdicts = await evaluate(`
  (async () => {
    const invoke = window.__TAURI__.core.invoke;
    return {
      safe: await invoke('classify_link', { url: 'https://example.com/invoice', text: 'example.com' }),
      mismatch: await invoke('classify_link', { url: 'https://evil.example/login', text: 'www.example.com' }),
      punycode: await invoke('classify_link', { url: 'https://xn--80ak6aa92e.com/x', text: 'apple.com' }),
      short: await invoke('classify_link', { url: 'https://bit.ly/3xYz', text: '短链接' }),
      dangerous: await invoke('classify_link', { url: 'javascript:alert(1)', text: '点我' }),
    };
  })()
`);

check('an ordinary link classifies as safe', verdicts.safe.risk === 'safe', verdicts.safe);
check('a different domain in the text is a mismatch', verdicts.mismatch.risk === 'mismatch', verdicts.mismatch);
check('an IDN host is flagged as punycode', verdicts.punycode.risk === 'punycode', verdicts.punycode);
check('a shortener says the destination is unknown', verdicts.short.risk === 'shortener', verdicts.short);
check('a script URL is a dangerous scheme', verdicts.dangerous.risk === 'dangerous_scheme', verdicts.dangerous);

const refused = await evaluate(`
  window.__TAURI__.core.invoke('open_link', { url: 'javascript:alert(1)', text: '点我' })
    .then(() => 'opened')
    .catch((error) => String(error))
`);
check('open_link refuses a dangerous scheme', refused !== 'opened' && refused.length > 0, refused);
check('and says why', /不会打开|可执行/.test(refused), refused);

ws.close();
if (failures.length) {
  console.log(`\nFAILED: ${failures.join(', ')}`);
  process.exit(1);
}
console.log('\nlink probe: all checks passed');
