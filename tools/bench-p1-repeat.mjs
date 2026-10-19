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
 * Needs the app built with `--remote-debugging-port=9222`, which has to be removed again afterwards —
 * `tools/check.ps1` fails the build if it is left in either config file.
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
  const child = spawn(EXE, [], { stdio: 'ignore', detached: false, cwd: ROOT });
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
      failed: `:${PORT} 没有应答 —— 这个 exe 里大概没有调试端口。Windows 要加在 src-tauri/tauri.windows.conf.json，` +
        '加在 tauri.conf.json 会被平台配置覆盖（build.rs 之后可以拿 exe 里搜不搜得到 remote-debugging-port 来验）',
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

async function main() {
  const runs = [];
  const perStage = new Map();
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
