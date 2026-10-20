/**
 * 第 8 项 B 段的尺子：点开一封信，列表还在不在用户看的地方？
 *
 *     node tools/probe-open-keeps-scroll.mjs                 # 三个臂各跑 --launches 次
 *     node tools/probe-open-keeps-scroll.mjs --exe target/release/easy-email.exe --launches 3
 *
 * `tools/probe-list-reuse.mjs` 已经在启动时读到过这个现象，但它只**记录**（"这是应用一直以来的行为，
 * 不是这次改动声称的东西"）—— B 段要改的正是这个行为，所以这里把它变成断言，并且把它量成分布：
 * 一次点击不是一个数，滚动位置取决于那一行落在窗口的哪一端、点的是会话行还是子行。
 *
 * 端口用 `WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS` 在启动时给进程：不为量这个烧一次 10–15 分钟的 release，
 * 也不往用户那份二进制里留端口（`reports/performance.md` §13.1 那条口径）。
 *
 * 它只点**已读**的行：点未读会走 `mark_seen`，那是往用户真实邮箱和背后的 IMAP 服务器上写
 * （一次真的 `UID STORE +FLAGS (\Seen)`），一个只读探针没有权力改它。
 */

import { spawn, spawnSync } from 'node:child_process';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const argv = process.argv.slice(2);
const flag = (name, fallback) => {
  const eq = argv.find((a) => a.startsWith(`--${name}=`));
  if (eq) return eq.slice(name.length + 3);
  const at = argv.indexOf(`--${name}`);
  if (at >= 0 && at + 1 < argv.length && !argv[at + 1].startsWith('--')) return argv[at + 1];
  return fallback;
};

const EXE = path.resolve(ROOT, flag('exe', 'target/release/easy-email.exe'));
const LAUNCHES = Number(flag('launches', '3'));
const PORT = Number(flag('port', '9233'));
const SETTLE = Number(flag('settle', '3'));
/** How far down the list to put the user before clicking. The report's number is 2 200 px. */
const SCROLL_TO = Number(flag('scroll-to', '2200'));
const sleep = (ms) => new Promise((done) => setTimeout(done, ms));

function killTree(pid) {
  spawnSync(path.join(process.env.SystemRoot, 'System32', 'taskkill.exe'), ['/PID', String(pid), '/T', '/F'], {
    stdio: 'ignore',
  });
}

async function findTarget() {
  for (let attempt = 0; attempt < 80; attempt += 1) {
    const list = await fetch(`http://127.0.0.1:${PORT}/json`, { signal: AbortSignal.timeout(1500) })
      .then((r) => r.json())
      .catch(() => null);
    if (Array.isArray(list)) {
      const page = list
        .filter((t) => t.type === 'page' && t.webSocketDebuggerUrl && !/^(about:|edge:|chrome:)/.test(t.url))
        .pop();
      if (page) return page;
    }
    await sleep(250);
  }
  return null;
}

function connect(socket) {
  let nextId = 1;
  const pending = new Map();
  socket.addEventListener('message', (event) => {
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
      socket.send(JSON.stringify({ id, method, params }));
    });
}

/**
 * Runs inside the page. A real function stringified with `toString()` — writing page code as a template
 * literal has eaten a backtick and a backslash here twice (2026-10-20), and `node --check` cannot see
 * inside a string.
 */
async function probePage(scrollTo) {
  const settle = () =>
    new Promise((done) => requestAnimationFrame(() => requestAnimationFrame(() => requestAnimationFrame(done))));
  const list = document.querySelector('#message-list');
  const win = () => document.querySelector('.vlist-window');
  if (!list || !win()) return { problems: ['#message-list 或 .vlist-window 不在 —— 现在这个列表没开窗口化'] };

  list.scrollTop = scrollTo;
  await settle();
  const topBefore = list.scrollTop;
  const maxScroll = list.scrollHeight - list.clientHeight;
  if (topBefore < Math.min(scrollTo, maxScroll) - 4) {
    return { problems: [`滚不动：请求 ${scrollTo} px，只到 ${topBefore} px（可滚范围 ${maxScroll}）—— 这一臂没法判`] };
  }

  const readTopLevel = (el) => !el.classList.contains('unread') && !el.classList.contains('thread-child');
  const row = [...win().children].find(readTopLevel);
  if (!row) return { problems: ['窗口里没有一行是已读的顶层行：只读探针不能去点未读（那会写 mark_seen）'] };

  const uid = row.dataset.uid;
  const subject = (row.querySelector('.msg-subject')?.firstChild?.textContent ?? '').trim();
  row.dispatchEvent(new MouseEvent('click', { bubbles: true }));

  const shown = () => {
    const body = document.querySelector('#reader-body');
    const thread = document.querySelector('#thread-view');
    if (thread && !thread.hidden) return 'thread';
    if (body && !body.hidden) {
      const s = document.querySelector('#reader-subject');
      return s && s.textContent.trim() ? 'body' : 'empty';
    }
    return 'hidden';
  };
  let view = shown();
  for (let waited = 0; waited < 60 && view === 'hidden'; waited += 1) {
    await new Promise((done) => setTimeout(done, 50));
    view = shown();
  }
  // markSeen lands after the body arrives, and it re-renders too — read the offset after both, the way
  // a user who scrolls back would. Waiting on `paints` would need a second paint counter the app has.
  await new Promise((done) => setTimeout(done, 1200));
  await settle();
  const topAfter = list.scrollTop;
  const stillThere = [...win().children].some((el) => el.dataset.uid === uid);
  const highlighted = [...win().children].filter((el) => el.classList.contains('selected')).map((el) => el.dataset.uid);

  const problems = [];
  if (view === 'hidden') problems.push('点了一行，1.2 s 后正文还没出现');
  if (view !== 'hidden' && view !== 'empty' && subject) {
    const text =
      (document.querySelector('#reader-subject')?.textContent ?? '') +
      ' ' +
      (document.querySelector('#thread-view')?.textContent ?? '');
    if (!text.includes(subject)) problems.push(`点开的是别的信：${uid} 的标题「${subject}」没出现在阅读器里`);
  }
  if (stillThere && !highlighted.includes(uid)) {
    problems.push(`那一行 ${uid} 在屏却没高亮（高亮的是 ${highlighted.join(',') || '没人'}）`);
  }
  const rowHeight = win().children[0]?.offsetHeight ?? 0;
  const jumped = topBefore - topAfter;
  if (jumped > rowHeight) {
    problems.push(`点开 ${uid} 之后列表从 ${topBefore} px 跳到 ${topAfter} px（差 ${jumped} px = ${Math.round(jumped / (rowHeight || 1))} 行）`);
  }

  // Phase 2 is the other half of the rule, and the one a "never reset the scroll" change would break:
  // switching folders *must* start at the top. `keepScroll` is a per-call-site policy, not a global
  // "always preserve", so the negative case has to be measured too or the fix is just a different bug.
  const other = [...document.querySelectorAll('#mailbox-list button.row[data-mailbox]')].find(
    (el) => el.textContent && el.dataset.mailbox !== document.querySelector('#mailbox-list .active')?.dataset?.mailbox
  );
  let switched = null;
  if (other) {
    const beforeTitle = document.querySelector('#mailbox-title')?.textContent ?? '';
    other.dispatchEvent(new MouseEvent('click', { bubbles: true }));
    let moved = beforeTitle;
    for (let waited = 0; waited < 80 && moved === beforeTitle; waited += 1) {
      await new Promise((done) => setTimeout(done, 50));
      moved = document.querySelector('#mailbox-title')?.textContent ?? '';
    }
    await settle();
    switched = {
      from: beforeTitle,
      to: moved,
      topAfterSwitch: list.scrollTop,
      rows: win()?.children.length ?? 0,
    };
    if (moved === beforeTitle) {
      problems.push('点了另一个文件夹，标题没变——切换没跑起来，这一臂判不了"换文件夹仍回顶部"');
    } else if (switched.topAfterSwitch > 0) {
      problems.push(`切到「${moved}」之后列表停在 ${switched.topAfterSwitch} px：换文件夹必须从顶上开始`);
    }
  }

  return {
    problems,
    uid,
    topBefore,
    topAfter,
    jumped,
    rowHeight,
    rowStillOnScreen: stillThere,
    highlighted,
    view,
    switched,
    paints: window.__easyEmailPaint?.paints ?? null,
  };
}

const PROBE = `(${probePage.toString()})(${SCROLL_TO})`;

async function oneLaunch(index) {
  const child = spawn(EXE, [], {
    stdio: 'ignore',
    cwd: ROOT,
    env: { ...process.env, WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS: `--remote-debugging-port=${PORT}` },
  });
  let spawnError = null;
  child.on('error', (error) => {
    spawnError = error;
  });
  await sleep(SETTLE * 1000);
  if (spawnError) return { failed: `启动失败：${spawnError.message}` };
  const target = await findTarget();
  if (!target) {
    killTree(child.pid);
    return { failed: `:${PORT} 上没有页面目标（端口没起来？路径对不对：${EXE}）` };
  }
  const socket = new WebSocket(target.webSocketDebuggerUrl);
  await new Promise((resolve, reject) => {
    socket.addEventListener('open', resolve);
    socket.addEventListener('error', reject);
  });
  const send = connect(socket);
  let out = null;
  let failed = null;
  try {
    const result = await send('Runtime.evaluate', { expression: PROBE, awaitPromise: true, returnByValue: true });
    if (result.exceptionDetails) failed = '页面里抛错：' + (result.exceptionDetails.text ?? '');
    else out = result.result.value;
  } catch (error) {
    failed = String(error?.message ?? error);
  }
  socket.close();
  killTree(child.pid);
  await sleep(2000);
  return { ...out, failed };
}

const runs = [];
for (let i = 1; i <= LAUNCHES; i += 1) {
  process.stdout.write(`launch ${i}/${LAUNCHES} … `);
  const run = await oneLaunch(i);
  runs.push(run);
  if (run.failed) {
    process.stdout.write(`FAILED: ${run.failed}\n`);
  } else {
    process.stdout.write(
      `scrollTop ${run.topBefore} → ${run.topAfter}（差 ${run.jumped} px，一行 ${run.rowHeight} px）· 行仍在屏 ${run.rowStillOnScreen} · 点开 ${run.uid} · 视图 ${run.view} · ` +
        // Printed either way: a second phase that silently skipped would make the pass above look like
        // it covered the folder-switch rule when it never ran.
        (run.switched
          ? `切到「${run.switched.to}」后 scrollTop ${run.switched.topAfterSwitch}（${run.switched.rows} 行）\n`
          : '侧栏里没有第二个文件夹，第二段没跑\n')
    );
    for (const problem of run.problems ?? []) process.stdout.write(`        - ${problem}\n`);
  }
  if (i < LAUNCHES) await sleep(SETTLE * 1000);
}

const good = runs.filter((r) => !r.failed);
const jumped = good.map((r) => r.jumped);
const worst = jumped.length ? Math.max(...jumped) : null;
const switched = good.filter((r) => r.switched);
const problems = good.flatMap((r) => r.problems ?? []);
console.log(
  `\nB 段判据（${good.length}/${LAUNCHES} 次成功）：点开一封信之后，列表跳回顶部的距离 —— 最大 ${worst} px ` +
    `（一行 ${good[0]?.rowHeight ?? '?'} px）。允许一行以内的移动，超过就是"把用户放回去"没做到。` +
    `\n另一半：切文件夹必须回顶部，${switched.length}/${good.length} 次真的切到了另一个文件夹` +
    (switched.length ? `（切完读到 ${switched.map((r) => r.switched.topAfterSwitch).join('/')} px）。` : '。')
);
if (good.length && switched.length === 0) {
  console.error('判据只覆盖了半边：没有一次切到另一个文件夹，"换文件夹仍回顶部"这条没被验证');
  process.exit(1);
}
if (problems.length) {
  for (const problem of problems) console.error(`- ${problem}`);
  process.exit(1);
}
if (good.length < LAUNCHES) {
  console.error('有臂没跑成，上面的判据不算数');
  process.exit(1);
}
console.log('点开一封信不再把列表弹回顶部，且点开的仍是那一行那一封、在屏的那行仍高亮');
