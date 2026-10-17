// Web-Bench (bytedance/web-bench): 50 web projects (React, Vue, Angular, Svelte, Next.js, Nuxt,
// Express, Fastify, Prisma, Sequelize, lowdb, plain DOM/CSS/SVG/canvas, bundlers, ...), each 20
// sequential tasks graded by Playwright end-to-end tests. One harness task = one project, run with
// Web-Bench's own protocol (tools/evaluator, taskMode 'sequential', retry 2):
//   for each task in order: the agent implements it in src/; the step is graded (init/build/test
//   for tasks 1..n); a failure gets one retry with the test error; a second failure ends the project
//   (except the optional `init` task, which never ends it: its directory falls back to src-init).
// Metrics (projects/readme.md): pass@1 = tasks passed on the first attempt before the first failure
// / tasks; pass@2 = tasks passed within two attempts / tasks.
import fs from 'node:fs';
import fsp from 'node:fs/promises';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { makeAdapter } from '../../lib/adapter.mjs';
import { LOCAL_REFS, HIDDEN, SRC } from '../../lib/paths.mjs';
import { writeProfile } from '../../lib/sandbox.mjs';
import { offlineEnv, T } from '../../lib/toolchains.mjs';
import { cloneTree, ensureDir, rmrf, run, walk, writeJson, writeText } from '../../lib/util.mjs';

const HERE = path.dirname(fileURLToPath(import.meta.url));

export const WEBBENCH = {
  repo: 'https://github.com/bytedance/web-bench.git',
  commit: '7b31ca2b786eef120dd49ce63dd03d0c0006046d',
  playwright: '1.57.0',
  retry: 2,
  // Playwright workers per test run, through the projects' own MAX_TEST_WORKERS knob. The default
  // (half the logical CPUs: 9 here) lets parallel tests race on a project's shared server state:
  // measured, the expressjs reference solution failed 2 of 2 runs at the default (a different test
  // each time) and passed 2 of 2 with one worker. One worker makes grading deterministic.
  workers: 1,
};

export const WB_DIR = path.join(HIDDEN, 'webbench');
export const GRADER = path.join(HERE, 'grade.mjs');

export function projectToolchainDir(project) {
  return path.join(T.webbench, 'projects', project);
}

/**
 * A private, writable node_modules for one workspace or scoring copy: the project's installed
 * packages as absolute symlinks (pnpm's own links are relative to the toolchain tree) plus copies of
 * its .bin shims. Dev servers and builds write caches into node_modules (vite: node_modules/.vite),
 * which must not land in the shared read-only toolchain.
 */
export async function buildNodeModules(project, dest) {
  const from = path.join(projectToolchainDir(project), 'node_modules');
  await rmrf(dest);
  await ensureDir(dest);
  for (const e of await fsp.readdir(from, { withFileTypes: true })) {
    const at = path.join(from, e.name);
    if (e.name === '.bin') { await fsp.cp(at, path.join(dest, '.bin'), { recursive: true }); continue; }
    if (e.name.startsWith('.')) continue;
    if (e.isSymbolicLink()) { await fsp.symlink(fs.realpathSync(at), path.join(dest, e.name)); continue; }
    if (e.isDirectory()) {
      await ensureDir(path.join(dest, e.name));
      for (const c of await fsp.readdir(at)) await fsp.symlink(fs.realpathSync(path.join(at, c)), path.join(dest, e.name, c));
    }
  }
}

export function webbenchEnv() {
  const env = { ...offlineEnv([]), PLAYWRIGHT_BROWSERS_PATH: T.playwright, NEXT_TELEMETRY_DISABLED: '1', NUXT_TELEMETRY_DISABLED: '1', npm_config_update_notifier: 'false', PRISMA_HIDE_UPDATE_MESSAGE: '1', CHECKPOINT_DISABLE: '1' };
  // The projects' playwright.config.js switch to retries=2 and the html reporter when CI is set;
  // Web-Bench's evaluator runs without it.
  delete env.CI;
  env.MAX_TEST_WORKERS = String(WEBBENCH.workers);
  return env;
}

/** Text of step k's prompt: the task description (Web-Bench sends only the current task). */
export function stepPrompt(task, k, { error = null } = {}) {
  const step = task.steps[k];
  const n = task.steps.filter((s) => !s.init).length;
  let p = `This workspace is the Web-Bench project \`${task.project}\`. Its code lives in \`src/\`; only \`src/\` is evaluated (the files outside it are the fixed project setup). `
    + `Each task is graded by the project's Playwright end-to-end tests for this task and all earlier ones.\n\n`;
  p += step.init ? `Initial task (before task 1 of ${n}):\n\n` : `Task ${step.index} of ${n} (${step.id}):\n\n`;
  p += `${step.description.trim()}\n`;
  if (step.init && task.init_files?.length) p += `\nThe existing files: ${task.init_files.join()}\n`;
  if (error !== null) {
    p += `\nThe previous attempt at this task failed the evaluation with the following error. Fix the code in src/ so the task passes:\n\n${error}\n`;
  }
  return p;
}

export function hiddenFiles(task) {
  const dir = path.join(WB_DIR, task.id);
  return task.hidden_index.map((rel) => ({ rel: rel.replace(/^tests\//, '').replace(/^reference\//, ''), abs: path.join(dir, rel) }));
}

/**
 * Score step `index`: a fresh copy of the pristine project (config files), the agent's src/, the
 * hidden tests and the shared node_modules, then grade.mjs under the network-denying sandbox.
 */
export async function webbenchScore(task, workspace, scoreRoot, outDir, tag, { index, srcOverride = null } = {}) {
  const started = Date.now();
  const dir = path.join(WB_DIR, task.id);
  // The copy keeps the repository layout (<root>/projects/<project>, <root>/libraries): some tests
  // import '../../../libraries/test-util/src' relative to their own directory.
  await rmrf(scoreRoot);
  const scoreDir = path.join(scoreRoot, 'projects', task.project);
  await cloneTree(path.join(dir, 'pristine'), scoreDir);
  await fsp.symlink(path.join(T.webbench, 'libraries'), path.join(scoreRoot, 'libraries'));
  const srcFrom = srcOverride ?? path.join(workspace, 'src');
  await rmrf(path.join(scoreDir, 'src'));
  if (fs.existsSync(srcFrom)) await cloneTree(srcFrom, path.join(scoreDir, 'src'));
  else await ensureDir(path.join(scoreDir, 'src'));
  // bench settings updateSettingBeforeRunTask: the project's assets/ are copied into every task's
  // output directory before the previous output is merged over them.
  // (Files the output already has, including a src/assets symlink, win.)
  if (fs.existsSync(path.join(scoreDir, 'assets'))) await mergeMissing(path.join(scoreDir, 'assets'), path.join(scoreDir, 'src', 'assets'));
  await cloneTree(path.join(dir, 'tests', 'test'), path.join(scoreDir, 'test'));
  await buildNodeModules(task.project, path.join(scoreDir, 'node_modules'));
  await ensureDir(outDir);
  const profile = await writeProfile(path.join(outDir, 'score.sb'), {
    tag: tag.replace(/[^A-Za-z0-9_.-]/g, '_'), denyRead: [HIDDEN, SRC, ...LOCAL_REFS], denyWrite: [T.webbench, T.playwright],
  });
  // Real paths: TMPDIR (/var/folders/...) is a symlink to /private/var/...; bundlers that mix the two
  // compute paths outside the project (vite: "../../private/..." chunk names).
  const real = fs.realpathSync(scoreDir);
  const env = { ...process.env, ...webbenchEnv() };
  delete env.CI;
  const res = await run('/usr/bin/sandbox-exec', ['-f', profile, process.execPath, GRADER, '--index', String(index), '--src', path.join(real, 'src'), '--task', task.steps.find((s) => s.index === index)?.id ?? ''], {
    cwd: real, env, timeoutMs: 35 * 60 * 1000,
  });
  const verdict = lastJson(res.output);
  const result = {
    command: ['node', 'grade.mjs', '--index', String(index)], index, exit_code: res.code, signal: res.signal, timed_out: res.timedOut,
    ms: res.ms, passed: res.code === 0 && !res.timedOut, started_ms: started, ended_ms: Date.now(), stages: verdict?.stages ?? null,
  };
  await writeText(path.join(outDir, 'output.txt'), res.output);
  await writeJson(path.join(outDir, 'result.json'), result);
  await rmrf(scoreRoot);
  return { ...result, output: res.output, feedback: verdict?.feedback ?? res.output.slice(-20000) };
}

/** Copy files from `from` into `to` that `to` does not have (by lstat), recursively. */
async function mergeMissing(from, to) {
  let st = null;
  try { st = await fsp.lstat(to); } catch { /* absent */ }
  if (!st) { await fsp.cp(from, to, { recursive: true, verbatimSymlinks: true }); return; }
  if (!st.isDirectory()) return;
  for (const e of await fsp.readdir(from)) await mergeMissing(path.join(from, e), path.join(to, e));
}

export function lastJson(output) {
  const lines = output.trim().split('\n');
  for (let i = lines.length - 1; i >= 0; i--) {
    if (lines[i].startsWith('{')) { try { return JSON.parse(lines[i]); } catch { /* keep looking */ } }
  }
  return null;
}

/** Sequential protocol state: {k: step position, attempt: 1|2}. */
export const sequential = {
  initialState: () => ({ k: 0, attempt: 1 }),
  maxTries: (task) => WEBBENCH.retry * task.steps.length,
  stepInfo: (state) => ({ k: state.k, attempt: state.attempt }),
  async advance(task, state, score, workspace) {
    const step = task.steps[state.k];
    const next = (k) => (k < task.steps.length ? { state: { k, attempt: 1 }, prompt: stepPrompt(task, k) } : null);
    if (score.passed) return next(state.k + 1);
    if (state.attempt < WEBBENCH.retry) {
      return { state: { k: state.k, attempt: state.attempt + 1 }, prompt: stepPrompt(task, state.k, { error: score.feedback ?? '' }) };
    }
    if (step.init) {
      // InitTaskRunner: a failed init task does not stop the project; the next task starts from src-init.
      await rmrf(path.join(workspace, 'src'));
      await cloneTree(path.join(WB_DIR, task.id, 'src-init'), path.join(workspace, 'src'));
      return next(state.k + 1);
    }
    return null;
  },
};

export const webbench = makeAdapter({
  name: 'webbench',
  title: 'Web-Bench (JS/TS web projects: React, Vue, Angular, Svelte, Next.js, Nuxt, Express, Fastify, ...)',
  tries: 1,
  defaultTimeoutMs: 8 * 3600 * 1000,
  hiddenFiles,
  score: (task, ws, scoreDir, outDir, tag, state) => webbenchScore(task, ws, scoreDir, outDir, tag, { index: task.steps[state?.k ?? 0].index }),
  exclude: () => ['/node_modules'],
  provision: (task, dest) => buildNodeModules(task.project, path.join(dest, 'node_modules')),
  agentEnvExtra: () => ({ PLAYWRIGHT_BROWSERS_PATH: T.playwright, NEXT_TELEMETRY_DISABLED: '1', NUXT_TELEMETRY_DISABLED: '1' }),
  sequential,
  reportNote: 'One task = one project of 20 sequential tasks (Web-Bench protocol: retry once with the test error, stop at the second failure). pass@1 and pass@2 follow projects/readme.md and are recomputed from the per-step test results; the "pass rate" column counts fully completed projects. Projects whose reference solution fails offline, or that need a backend service, are excluded (see the suite manifest).',
});
// The first prompt of a project is step 0's.
webbench.prompt = (task) => stepPrompt(task, 0);
