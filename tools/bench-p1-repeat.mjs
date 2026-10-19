#!/usr/bin/env node
/**
 * P1 is a distribution, not a number.
 *
 * `tools/bench-app.ps1` starts the app once per pass, and the eight launches of 2026-10-19 spread from
 * 1 858 ms to 10 038 ms — so a single pass cannot tell an optimization from the machine having something
 * else running. This script launches the app N times, takes the page's own first-row stamp plus the boot
 * stage marks (`window.__easyEmailBoot`, `src/app.js`) every time, and prints the per-launch table and the
 * medians.
 *
 *     node tools/bench-p1-repeat.mjs --exe target/release/easy-email.exe --launches 8
 *
 * Two legs of the same number are printed apart on purpose: `hostLeg` is process start -> the page's
 * `timeOrigin` (WebView2 bringing up the window, which this repo's JS cannot touch), and the stage table
 * is what happens after it. `design/15` P1's ❌ was 1 858 ms split roughly 1 000 / 866, and only the
 * second half is ours.
 *
 * Needs a debugging port on the webview. **It gets one without touching the build**: the port is passed
 * to the process it starts through `WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS`, so it exists only for the
 * launch under measurement and dies with it. The older route — `--remote-debugging-port` in
 * `src-tauri/tauri.windows.conf.json` — costs a release build to add and another to remove, and leaves
 * a port in the binary the user runs; pass `--env-port=0` to use it anyway (that is what the 2026-10-19
 * and 2026-10-20 batches did). `tools/check.ps1` keeps failing any build that has the flag baked in.
 *
 * Zero dependencies, same CDP sampler as `tools/bench-first-paint.mjs` (which this script drives).
 */

import { spawn, spawnSync } from 'node:child_process';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const SAMPLER = path.join(ROOT, 'tools', 'bench-first-paint.mjs');

const argv = process.argv.slice(2);
const flag = (name, fallback) => {
  const eq = argv.find((a) => a.startsWith(`--${name}=`));
  if (eq) return eq.slice(name.length + 3);
  const at = argv.indexOf(`--${name}`);
  if (at >= 0 && at + 1 < argv.length && !argv[at + 1].startsWith('--')) return argv[at + 1];
  return fallback;
};

const EXE = path.resolve(ROOT, flag('exe', 'target/release/easy-email.exe'));
const LAUNCHES = Number(flag('launches', '5'));
const PORT = Number(flag('port', '9222'));
/**
 * Give the launched process its debugging port through the environment instead of the build.
 *
 * `WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS` is read by the WebView2 loader when the process starts, so
 * `--env-port=1` (the default) measures a binary with no port in it and needs no release rebuild.
 * Set `--env-port=0` for a build that carries the flag in `tauri.windows.conf.json`.
 */
const ENV_PORT = flag('env-port', '1') !== '0';
/** Seconds to let the machine settle between launches; the port and the renderer need to be gone. */
const SETTLE = Number(flag('settle', '3'));
/** Budget for the judgment line at the end (`design/15` §4 P1). */
const BUDGET = Number(flag('budget', '1500'));

const sleep = (ms) => new Promise((done) => setTimeout(done, ms));

function median(values) {
  const sorted = [...values].sort((a, b) => a - b);
  if (sorted.length === 0) return null;
  const mid = Math.floor(sorted.length / 2);
  return sorted.length % 2 ? sorted[mid] : Math.round((sorted[mid - 1] + sorted[mid]) / 2);
}

function killTree(pid) {
  // The host is one process; the renderers that hold the page are its children. Killing only the host
  // leaves a WebView2 child holding :9222, and the next launch then reports the previous session's page.
  const taskkill = path.join(process.env.SystemRoot ?? 'C:\\Windows', 'System32', 'taskkill.exe');
  const r = spawnSync(taskkill, ['/PID', String(pid), '/T', '/F']);
  return r.status === 0;
}

/**
 * Fail fast when the debugging port never comes up.
 *
 * Without this, a wrong config file costs the sampler's own 60 s retry loop per launch, and the message
 * blames the app rather than the build. On Windows the browser args live in
 * `src-tauri/tauri.windows.conf.json`: Tauri merges per platform, so the same key in `tauri.conf.json`
 * is overwritten and the flag silently does not exist in the binary.
 */
async function waitForPort(timeoutMs) {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    const targets = await fetch(`http://127.0.0.1:${PORT}/json`, { signal: AbortSignal.timeout(1000) })
      .then((r) => r.json())
      .catch(() => null);
    if (Array.isArray(targets) && targets.length > 0) return true;
    await sleep(200);
  }
  return false;
}

/** One cold start: start the clock, launch, let the sampler answer, tear the tree down. */
async function oneLaunch(index) {
  const since = Date.now();
  const env = ENV_PORT
    ? { ...process.env, WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS: `--remote-debugging-port=${PORT}` }
    : process.env;
  const child = spawn(EXE, [], { stdio: 'ignore', detached: false, cwd: ROOT, env });
  // Without this an unresolvable --exe dies as an unhandled 'error' event, and the run looks like a
  // measurement failure rather than the typo it is.
  let spawnError = null;
  child.on('error', (error) => {
    spawnError = String(error?.message ?? error);
  });
  // The 'error' event lands on the next tick; checking synchronously would always say "no error".
  await sleep(150);
  if (spawnError) return { index, failed: `cannot start ${EXE}: ${spawnError}`, since };

  if (!(await waitForPort(20000))) {
    if (child.pid) {
      child.kill();
      killTree(child.pid);
    }
    return {
      index,
      failed: `:${PORT} 没有应答 —— 调试端口没起来。默认是 --env-port=1（靠 WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS ` +
        '给这个进程加端口，不需要重建）；要走"烧进构建"那条老路就 --env-port=0，并且必须加在 ' +
        'src-tauri/tauri.windows.conf.json（加在 tauri.conf.json 会被平台配置覆盖），编完可以拿 exe 里搜不搜得到 ' +
        'remote-debugging-port 来验',
      since,
    };
  }
  const sampler = spawn(
    process.execPath,
    [SAMPLER, '--port', String(PORT), '--since', String(since), '--paint', '1', '--switch', '0'],
    { stdio: ['ignore', 'pipe', 'pipe'], cwd: ROOT }
  );
  let out = '';
  let err = '';
  sampler.stdout.on('data', (chunk) => (out += chunk));
  sampler.stderr.on('data', (chunk) => (err += chunk));
  const code = await new Promise((resolve) => sampler.on('close', resolve));

  const line = out.split('\n').find((l) => l.startsWith('P1P2 '));
  const verdict = line ? JSON.parse(line.slice('P1P2 '.length).trim()) : null;
  if (child.pid) {
    child.kill();
    killTree(child.pid);
  }

  if (spawnError) {
    return { index, failed: `cannot start ${EXE}: ${spawnError}`, since };
  }
  if (!verdict) {
    return { index, failed: `no P1P2 line (exit ${code}) ${err.trim().slice(0, 200)}`, since };
  }
  return { index, since, exitCode: code, verdict, stderr: err.trim() };
}

/** Stage-to-stage cost, in ms, relative to the previous mark; `boot` is relative to the page origin. */
function stageDeltas(verdict) {
  const stages = verdict.p1.bootStages ?? [];
  const origin = verdict.p1.pageOriginEpoch;
  const rows = [];
  let previous = origin;
  for (const stage of stages) {
    rows.push({ name: stage.name, ms: Math.round(stage.at - previous), sinceStart: Math.round(stage.at - origin) });
    previous = stage.at;
  }
  return rows;
}

/**
 * The `boot` stage split into the parts that have different fixes.
 *
 * `boot` is one number covering four unrelated things: the host serving `index.html`, the stylesheet, the
 * twelve script fetches a blocking parser can only discover one at a time, and executing the modules. They
 * have four different fixes, so the parts get measured separately — from the browser's own timing entries,
 * which need no rebuild to read. Measured 2026-10-20, eight launches per arm: the transfers are nearly free
 * (5–15 ms each, and `defer` overlaps them so the last one lands 155 ms earlier), the host's `index.html`
 * is ~221 ms of the leg, and what is left — executing the modules before `app.js` — is ~130–165 ms.
 *
 * `appExec` is the only derived figure that was there first: DCL cannot fire until every classic script
 * has run, so it is `dclStart − app.js.responseEnd`. With blocking tags `app.js` is fetched last and runs
 * last, so that reads as "executing app.js" and the other eleven modules show up as `wait`. With `defer`
 * the same expression changes meaning: the transfers overlap, `app.js` arrives early, and everything the
 * other modules execute falls into `appExec` instead. `tailExec` (`dclStart − chainEnd`) is the one figure
 * that means the same thing in both shapes — how long the main thread still needed after the last byte.
 */
function bootSplit(verdict) {
  const p1 = verdict.p1 ?? {};
  const nav = p1.nav;
  const assets = p1.assets ?? [];
  if (!nav || assets.length === 0) return null;
  const app = assets.find((a) => a.file === 'app.js');
  const css = assets.find((a) => a.isCss);
  const others = assets.filter((a) => a !== app && a !== css);
  const firstStart = Math.min(...assets.map((a) => a.start));
  const chainEnd = Math.max(...others.map((a) => a.respEnd), css?.respEnd ?? 0);
  const sumDur = assets.reduce((n, a) => n + a.dur, 0);
  const stages = p1.bootStages ?? [];
  return {
    leg: stages.length ? Math.round(stages[0].at - p1.pageOriginEpoch) : null,
    doc: nav.respEnd,
    css: css?.dur ?? null,
    scripts: assets.filter((a) => a.isJs).length,
    chainEnd,
    wait: Math.round(chainEnd - firstStart - sumDur),
    tailExec: Math.round(nav.dclStart - chainEnd),
    appFetch: app?.dur ?? null,
    appExec: app ? Math.round(nav.dclStart - app.respEnd) : null,
    appKb: app?.kb ?? null,
  };
}

async function main() {
  const runs = [];
  const perStage = new Map();
  const perSplit = new Map();
  for (let i = 1; i <= LAUNCHES; i += 1) {
    process.stdout.write(`launch ${i}/${LAUNCHES} … `);
    const run = await oneLaunch(i);
    runs.push(run);
    if (run.failed) {
      process.stdout.write(`FAILED: ${run.failed}\n`);
    } else {
      const p1 = run.verdict.p1;
      const firstRow = p1.msSinceProcessStart ?? p1.msSincePageStart;
      process.stdout.write(
        `P1 ${Math.round(firstRow ?? -1)} ms (${p1.source ?? 'no row'}), rows ${p1.rows}, ` +
          `boot marks ${p1.bootStagesPresent ? `${(p1.bootStages ?? []).length}` : 'absent'}\n`
      );
      // Printed as it is measured, not after the loop: the eight launches are the expensive part, and a
      // crash in the summary must not be allowed to throw them away.
      const hostLeg = p1.pageOriginEpoch ? p1.pageOriginEpoch - run.since : null;
      process.stdout.write(
        `  #${i}  pageOrigin-processStart ${hostLeg} ms  stamp-vs-dom ${p1.stampVsDomMs ?? '-'} ms  ` +
          `cache-mark ${p1.refreshingMark ? 'yes' : 'no'}\n`
      );
      for (const stage of stageDeltas(run.verdict)) {
        process.stdout.write(
          `        ${stage.name.padEnd(18)} ${String(stage.ms).padStart(6)}   (+${stage.sinceStart} since page origin)\n`
        );
        if (!perStage.has(stage.name)) perStage.set(stage.name, []);
        perStage.get(stage.name).push(stage.ms);
      }
      const split = bootSplit(run.verdict);
      if (split) {
        process.stdout.write(
          `        boot 腿拆成：文档 ${split.doc} · css ${split.css} · ${split.scripts} 个脚本的最后一个到达 ${split.chainEnd} · ` +
            `之后到 DCL ${split.tailExec}（主线程执行）· 传输之外的等待 ${split.wait} · ` +
            `app.js 取回 ${split.appFetch}（${split.appKb} KB）· DCL 减 app.js 到达 ${split.appExec}（推导）\n`
        );
        for (const [key, value] of Object.entries(split)) {
          if (!Number.isFinite(value)) continue;
          if (!perSplit.has(key)) perSplit.set(key, []);
          perSplit.get(key).push(value);
        }
      }
    }
    if (i < LAUNCHES) await sleep(SETTLE * 1000);
  }

  const good = runs.filter((r) => r.verdict && (r.verdict.p1.msSinceProcessStart ?? r.verdict.p1.msSincePageStart) != null);
  if (good.length === 0) {
    console.error('no launch produced a first-row number — is the app built with --remote-debugging-port?');
    process.exit(1);
  }

  console.log('\nstage medians:');
  for (const [name, values] of perStage) {
    console.log(`  ${name.padEnd(18)} ${String(median(values)).padStart(6)} ms   over ${values.length} launches`);
  }
  if (perSplit.size) {
    console.log('\nboot 腿拆开（同一次启动的时间线条目，中位 / 范围）：');
    for (const [key, values] of perSplit) {
      const sorted = [...values].sort((a, b) => a - b);
      // `scripts` is a count and `appKb` a size — printing "12 ms" / "217 ms" for them would be a unit
      // error in the tool, not in the measurement.
      const unit = key === 'scripts' ? '' : key === 'appKb' ? ' KB' : ' ms';
      console.log(
        `  ${key.padEnd(10)} ${String(median(values)).padStart(6)}${unit}   ` +
          `范围 ${sorted[0]}–${sorted[sorted.length - 1]}，${values.length} 次`
      );
    }
    console.log('  `doc` / `chainEnd` 是从页面 origin 起算的时刻，其余是时长。`tailExec` = DCL − 最后一个资源到达，两种标签形态同义；' +
      '`appExec` = DCL − app.js 到达，阻塞式标签下它约等于 app.js 自己执行，defer 下它是十二个模块一起执行。' +
      '`wait` = 最后一个资源到达 − 第一个资源开跑 − 全部传输时长，阻塞式标签下是解析器一个一个发现脚本的串行等待，' +
      'defer 让传输重叠后它会变负（负得越多重叠得越好）。');
  }
  const p1s = good.map((r) => Math.round(r.verdict.p1.msSinceProcessStart ?? r.verdict.p1.msSincePageStart));
  console.log(
    `\nP1: median ${median(p1s)} ms, min ${Math.min(...p1s)}, max ${Math.max(...p1s)} ` +
      `over ${p1s.length}/${LAUNCHES} launches — budget ≤ ${BUDGET} ms\n` +
      `read this as a distribution: one launch is not a number (2026-10-19: 1 858 … 10 038 ms).`
  );
  if (median(p1s) > BUDGET) process.exit(2);
}

main().catch((error) => {
  console.error(String(error?.message ?? error));
  process.exit(1);
});
