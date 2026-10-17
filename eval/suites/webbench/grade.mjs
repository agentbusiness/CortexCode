// Grade one Web-Bench step (task index n of a project) the way Web-Bench's evaluator does
// (tools/evaluator at the pinned commit: plugins/plugin/bench + utils/environment, utils/builder,
// plugins/utils/tester/playwright.ts), run inside the scoring copy of the project:
//   1. `npm run init`  when package.json has an `init` script   (Environment.init)
//   2. `npm run build` when package.json has a non-empty `build` script (settings.needBuild)
//   3. `npm run test -- <n>`: the project's Playwright tests for init + task-1..task-n
//      (libraries/test-util/test.sh), with EVAL_PROJECT_ROOT = the agent's src, a free port in
//      EVAL_PROJECT_PORT, IS_EVAL_PRODUCTION=true (30 s per test), retried up to 3 times only when the
//      output shows a dev-server start failure (`config.webServer` / `net::ERR_ABORTED`), 10 min cap.
// The step passes only when every stage exits 0. The feedback text for a retry is the failing
// stage's output cleaned as prettierErrorMessage/clearErrorMsg do (ANSI, `[n/m]` progress lines,
// `<` lines and blank lines removed; directories replaced by '.').
//
// usage: node grade.mjs --index <n> --src <abs src dir>     (cwd = the project copy)
// Exit 0 = pass, 1 = fail. The last stdout line is a JSON verdict.
import { spawn } from 'node:child_process';
import fs from 'node:fs';
import net from 'node:net';
import path from 'node:path';

const args = Object.fromEntries(process.argv.slice(2).reduce((acc, a, i, arr) => (a.startsWith('--') ? [...acc, [a.slice(2), arr[i + 1]]] : acc), []));
const index = Number(args.index);
const src = path.resolve(args.src);
const projectDir = process.cwd();
const RETRY_TIMES = 3;
const EXCEED_TIME = 10 * 60 * 1000;

function freePort() {
  return new Promise((resolve, reject) => {
    const s = net.createServer();
    s.unref();
    s.on('error', reject);
    s.listen(0, '127.0.0.1', () => { const { port } = s.address(); s.close(() => resolve(port)); });
  });
}

function sh(cmd, env, timeoutMs) {
  return new Promise((resolve) => {
    const child = spawn('/bin/sh', ['-c', cmd], { cwd: projectDir, env, detached: true, stdio: ['ignore', 'pipe', 'pipe'] });
    let out = '';
    let stdout = '';
    let stderr = '';
    let timedOut = false;
    const timer = setTimeout(() => { timedOut = true; try { process.kill(-child.pid, 'SIGKILL'); } catch { /* gone */ } }, timeoutMs);
    child.stdout.on('data', (d) => { out += d; stdout += d; process.stdout.write(d); });
    child.stderr.on('data', (d) => { out += d; stderr += d; process.stdout.write(d); });
    child.on('close', (code) => {
      clearTimeout(timer);
      try { process.kill(-child.pid, 'SIGKILL'); } catch { /* group gone */ }
      resolve({ code, out, stdout, stderr, timedOut });
    });
  });
}

const stripAnsi = (s) => s.replace(/\u001b\[[0-9;?]*[ -/]*[@-~]/g, '');
function prettier(text) {
  let t = stripAnsi(text ?? '').split('\n').filter((v) => !/^\[\d+\/\d+\]/.test(v) && !v.startsWith('<') && Boolean(v.trim())).join('\n');
  for (const dir of [path.join(projectDir, 'test'), src, projectDir, 'src/']) t = t.split(dir).join('.');
  return t;
}

const pkg = JSON.parse(fs.readFileSync(path.join(projectDir, 'package.json'), 'utf8'));
const scripts = pkg.scripts ?? {};
const baseEnv = { ...process.env, EVAL_PROJECT_ROOT: src, EVAL_TASK: args.task ?? '', EVAL: 'true' };
const stages = [];
let feedback = '';
let passed = true;

if (scripts.init) {
  const r = await sh('npm run init', baseEnv, EXCEED_TIME);
  stages.push({ stage: 'init', exit: r.code, timed_out: r.timedOut });
  if (r.code !== 0) { passed = false; feedback = prettier(r.stderr || r.out); }
}
if (passed && scripts.build) {
  const r = await sh('npm run build', baseEnv, EXCEED_TIME);
  stages.push({ stage: 'build', exit: r.code, timed_out: r.timedOut });
  if (r.code !== 0) { passed = false; feedback = prettier(r.stderr || r.out); }
}
if (passed) {
  let r;
  for (let i = 0; i < RETRY_TIMES; i++) {
    const port = await freePort();
    r = await sh(`npm run test -- ${index}`, { ...baseEnv, EVAL_PROJECT_PORT: String(port), IS_EVAL_PRODUCTION: 'true', FORCE_COLOR: 'true' }, EXCEED_TIME);
    stages.push({ stage: 'test', attempt: i + 1, port, exit: r.code, timed_out: r.timedOut });
    if (r.timedOut) break;
    if (r.code === 0) break;
    if (!r.out.includes('config.webServer') && !r.out.includes('net::ERR_ABORTED')) break;
  }
  if (r.code !== 0) {
    passed = false;
    feedback = r.timedOut ? 'Test execution timed out. Check for infinite loops or high code complexity.' : prettier(r.stdout || r.out);
  }
}
console.log('\n' + JSON.stringify({ verdict: passed ? 'pass' : 'fail', index, stages, feedback: feedback.slice(-20000) }));
process.exit(passed ? 0 : 1);
