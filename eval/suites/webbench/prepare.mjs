// Prepare Web-Bench: the pinned repository, its dependencies installed once with the repository's
// own tooling (Rush install-run-rush.js -> pnpm, lockfile common/config/rush/pnpm-lock.yaml) into
// eval/cache/toolchains/web-bench (reference solutions and tests stripped from that agent-readable
// copy), Playwright's headless Chromium (the version the projects pin) into
// eval/cache/toolchains/ms-playwright, the task store (one task per stable project), and controls:
//   positive: the project's reference solution (projects/<p>/src) passes all its tests
//             (`npm test -- <last>`, the repository's own `src:test` check) offline in the sandbox;
//   negative: the starting state (src-init, or an empty src/ when the project has an init task)
//             fails the same tests.
// A project whose reference fails offline is excluded with the recorded reason (e.g. it needs a
// database server binary or installs packages from the registry at evaluation time).
import fs from 'node:fs';
import fsp from 'node:fs/promises';
import path from 'node:path';
import { ensureGitCommit, gitExport } from '../../lib/fetch.mjs';
import { SRC, tmpRoot, TOOLCHAINS } from '../../lib/paths.mjs';
import { ensureManagedVenv, T } from '../../lib/toolchains.mjs';
import { cloneTree, ensureDir, exists, log, pool, readJson, rmrf, runOk, sha256, walk, writeJson, writeText } from '../../lib/util.mjs';
import { materializeWorkspace } from '../../lib/workspace.mjs';
import { projectToolchainDir, WB_DIR, WEBBENCH, webbenchScore } from './suite.mjs';

const HERE = path.dirname(new URL(import.meta.url).pathname);
const REPO = path.join(SRC, 'web-bench');
const PREP_VENV = path.join(TOOLCHAINS, 'bench-prep');
const STRIP = ['src', 'test', 'eval', 'tasks.jsonl', 'tasks.yml', 'readme.md'];

async function treeHash(dir) {
  const parts = [];
  for (const f of await walk(dir)) {
    const abs = path.join(dir, f.rel);
    parts.push(`${f.rel}\0${f.symlink ? `symlink:${await fsp.readlink(abs)}` : sha256(await fsp.readFile(abs))}`);
  }
  return sha256(parts.join('\n'));
}

/** Install the repository's dependencies (Rush + pnpm, frozen lockfile) and Playwright's browser. */
async function ensureToolchain() {
  const stamp = path.join(T.webbench, '.cceval-stamp.json');
  const want = JSON.stringify({ commit: WEBBENCH.commit, strip: STRIP, playwright: WEBBENCH.playwright, prismaEngineCopy: 1 });
  if (!(await exists(stamp) && fs.readFileSync(stamp, 'utf8') === want)) {
    log('webbench: installing the repository toolchain (rush install)');
    await gitExport(REPO, WEBBENCH.commit, T.webbench);
    for (const e of await fsp.readdir(path.join(T.webbench, 'projects'), { withFileTypes: true })) {
      if (!e.isDirectory()) continue;
      for (const s of STRIP) await rmrf(path.join(T.webbench, 'projects', e.name, s));
    }
    // eval/package.json declares "type": "module"; Rush's CommonJS scripts need their own marker.
    await writeText(path.join(T.webbench, 'common', 'scripts', 'package.json'), '{"type":"commonjs"}\n');
    const gitEnv = { ...process.env, GIT_AUTHOR_NAME: 'cceval', GIT_AUTHOR_EMAIL: 'cceval@localhost', GIT_COMMITTER_NAME: 'cceval', GIT_COMMITTER_EMAIL: 'cceval@localhost' };
    await runOk('git', ['init', '-q', '.'], { cwd: T.webbench, env: gitEnv });
    await runOk('git', ['add', '-A'], { cwd: T.webbench, env: gitEnv, timeoutMs: 600000 });
    await runOk('git', ['commit', '-q', '-m', 'snapshot'], { cwd: T.webbench, env: gitEnv });
    const env = {
      ...process.env, npm_config_cache: path.join(TOOLCHAINS, 'web-bench-npm-cache'),
      RUSH_PNPM_STORE_PATH: path.join(TOOLCHAINS, 'web-bench-pnpm-store'), PLAYWRIGHT_SKIP_BROWSER_DOWNLOAD: '1',
    };
    await runOk(process.execPath, ['common/scripts/install-run-rush.js', 'install'], { cwd: T.webbench, env, timeoutMs: 3 * 3600 * 1000 });
    // `prisma generate` copies the query engine (installed by @prisma/engines) into the prisma CLI
    // package on first use; do that copy now, while the toolchain is writable (read-only in runs).
    const pnpmDir = path.join(T.webbench, 'common', 'temp', 'node_modules', '.pnpm');
    for (const d of (await fsp.readdir(pnpmDir)).filter((x) => /^prisma@/.test(x))) {
      const cli = path.join(pnpmDir, d, 'node_modules', 'prisma');
      for (const e of (await fsp.readdir(pnpmDir)).filter((x) => /^@prisma\+engines@/.test(x))) {
        const lib = path.join(pnpmDir, e, 'node_modules', '@prisma', 'engines', 'libquery_engine-darwin-arm64.dylib.node');
        if (fs.existsSync(lib) && !fs.existsSync(path.join(cli, path.basename(lib)))) await fsp.copyFile(lib, path.join(cli, path.basename(lib)));
      }
    }
    await runOk(path.join(projectToolchainDir('react'), 'node_modules', '.bin', 'playwright'), ['install', '--only-shell', 'chromium'], {
      cwd: projectToolchainDir('react'), env: { ...process.env, PLAYWRIGHT_BROWSERS_PATH: T.playwright }, timeoutMs: 3600000,
    });
    await writeText(stamp, want);
  }
  const browsers = (await fsp.readdir(T.playwright)).filter((d) => !d.startsWith('.')).sort();
  const pw = JSON.parse(await fsp.readFile(path.join(projectToolchainDir('react'), 'node_modules', '@playwright', 'test', 'package.json'), 'utf8')).version;
  const lock = await fsp.readFile(path.join(T.webbench, 'common', 'config', 'rush', 'pnpm-lock.yaml'));
  return [
    { name: 'web-bench dependencies (rush install, frozen pnpm lockfile)', url: WEBBENCH.repo, revision: WEBBENCH.commit, sha256: sha256(lock), bytes: lock.length, note: 'sha256 of common/config/rush/pnpm-lock.yaml' },
    { name: `Playwright ${pw} browsers (${browsers.join(', ')})`, url: 'https://playwright.azureedge.net (Playwright CDN, via `playwright install --only-shell chromium`)', revision: browsers.join(','), sha256: await treeHash(T.playwright), bytes: null, note: 'sha256 = content hash of the installed browser tree' },
  ];
}

/** Read a project's task list (tasks.jsonl, or tasks.yml via PyYAML in the prepare venv). */
async function readTasks(projDir) {
  if (fs.existsSync(path.join(projDir, 'tasks.jsonl'))) {
    return (await fsp.readFile(path.join(projDir, 'tasks.jsonl'), 'utf8')).split('\n').filter((l) => l.trim()).map((l) => JSON.parse(l));
  }
  const res = await runOk(path.join(PREP_VENV, 'bin', 'python'), ['-I', '-c', 'import json,sys,yaml; print(json.dumps(yaml.safe_load(open(sys.argv[1], encoding="utf-8")), default=str))', path.join(projDir, 'tasks.yml')]);
  return JSON.parse(res.stdout);
}

async function copyFiltered(from, to, skip) {
  await ensureDir(to);
  for (const e of await fsp.readdir(from, { withFileTypes: true })) {
    if (skip(e.name)) continue;
    await fsp.cp(path.join(from, e.name), path.join(to, e.name), { recursive: true, verbatimSymlinks: true });
  }
}

export async function validate(task) {
  const out = { id: task.id };
  const tmp = path.join(tmpRoot(), 'cceval-wb-validate', task.id);
  await rmrf(tmp);
  const last = task.steps.at(-1).index;
  const dir = path.join(WB_DIR, task.id);
  // The reference runs twice: a positive control must pass deterministically.
  // The negative control grades the starting state (src-init, or an empty src/ when the project
  // has an init task) on task-1 (its tests plus init.spec): every later step's tests include these.
  // If the starting state already passes task-1 (some projects' src-init does), the negative control
  // is the full test set instead (initial_last).
  for (const mode of ['initial', 'initial_last', 'reference', 'reference_repeat']) {
    if (mode === 'initial_last' && out.initial !== 'pass') continue;
    const ws = path.join(tmp, `${mode}-ws`);
    await materializeWorkspace(path.join(dir, 'template'), ws);
    const res = await webbenchScore(task, ws, path.join(tmp, `${mode}-score`), path.join(tmp, `${mode}-log`), `wbval-${mode}`, {
      index: mode === 'initial' ? task.steps.find((x) => !x.init).index : last, srcOverride: mode.startsWith('reference') ? path.join(dir, 'reference', 'src') : null,
    });
    out[mode] = res.passed ? 'pass' : 'fail';
    out[`${mode}_ms`] = res.ms;
    out[`${mode}_stages`] = res.stages;
    if (mode.startsWith('reference') && !res.passed) {
      out[`${mode}_tail`] = res.output.slice(-2500);
      // Structural classification of the recorded output (error names / messages), for the label only.
      out.reference_network = !!out.reference_network || /ENOTFOUND|EAI_AGAIN|getaddrinfo|registry\.npmjs\.org|fastdl\.mongodb\.org|MongoMemoryServer|ECONNREFUSED (?!127\.0\.0\.1|::1)/.test(res.output);
    }
  }
  await rmrf(tmp);
  return out;
}

export async function prepareWebbench({ force = false, validateAll = true, only = null } = {}) {
  const manifestPath = path.join(WB_DIR, 'tasks.json');
  if (!force && await exists(manifestPath)) {
    log('webbench: already prepared');
    return JSON.parse(await fsp.readFile(manifestPath, 'utf8'));
  }
  const src = await ensureGitCommit(REPO, WEBBENCH.repo, WEBBENCH.commit);
  await ensureManagedVenv(PREP_VENV, '3.12', [['pyarrow==21.0.0', 'pyyaml==6.0.2', 'packaging==25.0']]);
  const toolSources = await ensureToolchain();
  const tree = path.join(tmpRoot(), 'cceval-wb-tree');
  await gitExport(REPO, WEBBENCH.commit, tree);
  await rmrf(WB_DIR);
  await ensureDir(WB_DIR);
  const tasks = [];
  const skipped = [];
  for (const project of (await fsp.readdir(path.join(tree, 'projects'))).sort()) {
    const projDir = path.join(tree, 'projects', project);
    if (!fs.statSync(projDir).isDirectory()) continue;
    const pkg = await readJson(path.join(projDir, 'package.json'), null);
    // apps/eval utils/project.ts getStableProject: projects whose package.json has eval.stable.
    if (!pkg?.eval?.stable) { skipped.push({ project, reason: pkg ? 'not marked eval.stable (not part of the benchmark set)' : 'no package.json' }); continue; }
    if (only && !only.includes(project)) continue;
    const raw = await readTasks(projDir);
    const hasInit = raw.some((t) => t.id === 'init');
    // tools/evaluator utils/task.ts tasks2setting: index = position (with init) or position + 1.
    const steps = raw.map((t, i) => ({ id: t.id, index: hasInit ? i : i + 1, init: t.id === 'init', level: t.level ?? null, date: t.date ?? null, description: String(t.description) }));
    const id = `wb-${project}`;
    const dir = path.join(WB_DIR, id);
    await copyFiltered(projDir, path.join(dir, 'pristine'), (n) => [...STRIP, 'src-init', 'node_modules'].includes(n));
    await copyFiltered(projDir, path.join(dir, 'template'), (n) => [...STRIP, 'src-init', 'node_modules'].includes(n));
    const initDir = path.join(projDir, 'src-init');
    const initFiles = fs.existsSync(initDir) ? (await walk(initDir)).map((f) => f.rel) : [];
    if (fs.existsSync(initDir)) await fsp.cp(initDir, path.join(dir, 'src-init'), { recursive: true, verbatimSymlinks: true });
    else await ensureDir(path.join(dir, 'src-init'));
    // bench settings: with an init task the run starts from an empty directory, else from src-init.
    if (!hasInit && fs.existsSync(initDir)) await fsp.cp(initDir, path.join(dir, 'template', 'src'), { recursive: true, verbatimSymlinks: true });
    // The project's assets/ are part of every task's output directory (bench settings).
    if (fs.existsSync(path.join(projDir, 'assets'))) await fsp.cp(path.join(projDir, 'assets'), path.join(dir, 'template', 'src', 'assets'), { recursive: true });
    await fsp.cp(path.join(projDir, 'test'), path.join(dir, 'tests', 'test'), { recursive: true, verbatimSymlinks: true });
    await fsp.cp(path.join(projDir, 'src'), path.join(dir, 'reference', 'src'), { recursive: true, verbatimSymlinks: true });
    // Integrity index: every test file, and reference files of 512+ bytes that src-init does not
    // already contain (tiny or shared files would match innocent workspace files).
    const initHashes = new Set();
    for (const f of initFiles) initHashes.add(sha256(await fsp.readFile(path.join(initDir, f))));
    const hiddenIndex = (await walk(path.join(dir, 'tests'))).filter((f) => !f.symlink).map((f) => `tests/${f.rel}`);
    for (const f of await walk(path.join(dir, 'reference'))) {
      if (f.symlink) continue;
      const buf = await fsp.readFile(path.join(dir, 'reference', f.rel));
      if (buf.length >= 512 && !initHashes.has(sha256(buf))) hiddenIndex.push(`reference/${f.rel}`);
    }
    const task = {
      id, suite: 'webbench', lang: 'web', group: project, project, package_name: pkg.name,
      steps, n_tasks: steps.filter((s) => !s.init).length, has_init: hasInit, init_files: initFiles,
      scripts: pkg.scripts ?? {}, hidden_index: hiddenIndex, solution_files: ['src/'], file_list: 'src/',
    };
    const { stepPrompt } = await import('./suite.mjs');
    task.prompt = stepPrompt(task, 0);
    await writeJson(path.join(dir, 'task.json'), task);
    tasks.push(task);
  }
  await rmrf(tree);
  let validation = [];
  if (validateAll) {
    log(`webbench: validating ${tasks.length} projects (reference passes all tests, starting state fails)`);
    validation = await pool(tasks, 3, (t) => validate(t));
  }
  const byId = new Map(validation.map((v) => [v.id, v]));
  const entries = tasks.map((t) => {
    const v = byId.get(t.id);
    let excluded = null;
    if (v && (v.reference !== 'pass' || v.reference_repeat !== 'pass')) {
      excluded = v.reference_network
        ? 'reference solution needs the network at evaluation time (package registry or a service binary download); runs are offline'
        : v.reference === 'pass' || v.reference_repeat === 'pass'
          ? 'reference solution passes only some runs (nondeterministic tests; see validation detail)'
          : 'reference solution fails its own tests here (see validation detail)';
    } else if (v && v.initial !== 'fail' && v.initial_last !== 'fail') excluded = 'starting state already passes every test';
    return { id: t.id, lang: t.lang, group: t.group, n_tasks: t.n_tasks, ...(excluded ? { excluded } : {}) };
  });
  const counts = {};
  for (const e of entries.filter((x) => !x.excluded)) counts[e.lang] = (counts[e.lang] ?? 0) + 1;
  const manifest = {
    suite: 'webbench', sources: [{ ...src, path: undefined }, ...toolSources], counts,
    subtasks: entries.filter((x) => !x.excluded).reduce((a, e) => a + e.n_tasks, 0),
    grading: { source: `${WEBBENCH.repo} tools/evaluator (bench plugin, playwright tester) + libraries/test-util/test.sh`, commit: WEBBENCH.commit, protocol: 'sequential, retry 2, stop at the second failure; init task never stops the project', metrics: 'pass@1, pass@2 (projects/readme.md)' },
    controls: { positive: 'reference src passes all tests (npm test -- <last>), twice', negative: 'starting state (src-init, or empty with an init task) fails task-1 (or, where it passes task-1, the full test set)' },
    skipped, tasks: entries, validation,
  };
  await writeJson(manifestPath, manifest);
  return manifest;
}
