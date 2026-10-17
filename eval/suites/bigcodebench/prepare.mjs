// Prepare BigCodeBench-Hard: pinned dataset file, the official checker, the library venv (uv,
// managed CPython 3.10 under eval/cache/toolchains; never system Python or Homebrew), the task
// store (148 tasks x {complete, instruct}), and the controls:
//   positive: the task's canonical solution (complete_prompt + canonical_solution) must pass;
//   negative: the untouched template (complete: signature + docstring, the body returns None;
//             instruct: an empty file, which calibration turns into code_prompt + `pass`) must fail.
// A task whose canonical solution fails here (offline, macOS) is excluded with the checker's
// recorded reason; the official leaderboard documents the same class of environment-dependent
// ground-truth failures.
import fsp from 'node:fs/promises';
import path from 'node:path';
import readline from 'node:readline';
import fs from 'node:fs';
import { ensureGitCommit, fetchPinned } from '../../lib/fetch.mjs';
import { SRC, TOOLCHAINS, tmpRoot } from '../../lib/paths.mjs';
import { ensureManagedVenv, T } from '../../lib/toolchains.mjs';
import { ensureDir, exists, log, pool, rmrf, runOk, sha256, writeJson, writeText } from '../../lib/util.mjs';
import { materializeWorkspace } from '../../lib/workspace.mjs';
import { BCB, BCB_DIR, BCB_MPLCONFIG, bcbEnv, bcbPrompt, bcbScore } from './suite.mjs';

const HERE = path.dirname(new URL(import.meta.url).pathname);
const DATA = path.join(SRC, 'datasets', 'bcb-hard');
const PREP_VENV = path.join(TOOLCHAINS, 'bench-prep');

async function ensureChecker() {
  const repo = path.join(SRC, 'bigcodebench');
  const src = await ensureGitCommit(repo, BCB.repo, BCB.commit);
  const lockText = await fsp.readFile(BCB.lock, 'utf8');
  await ensureManagedVenv(T.bcb, BCB.python, [['--compile-bytecode', '-r', BCB.lock]], { stampExtra: { lock_sha256: sha256(lockText) } });
  const dest = path.join(T.bcb, 'src');
  await rmrf(dest);
  const files = [];
  for (const rel of BCB.checkerFiles) {
    const res = await runOk('git', ['show', `${BCB.commit}:${rel}`], { cwd: repo });
    await writeText(path.join(dest, rel), res.stdout);
    files.push({ path: rel, sha256: sha256(res.stdout) });
  }
  // Build matplotlib's font cache once (it takes ~30 s and every task imports matplotlib).
  await ensureDir(BCB_MPLCONFIG);
  await runOk(path.join(T.bcb, 'bin', 'python'), ['-I', '-c', 'import matplotlib.pyplot'], { env: { ...process.env, ...bcbEnv() }, timeoutMs: 600000 });
  return { ...src, files, lock_sha256: sha256(lockText) };
}

async function* rows(file) {
  const rl = readline.createInterface({ input: fs.createReadStream(file, { encoding: 'utf8' }), crlfDelay: Infinity });
  for await (const line of rl) if (line.trim()) yield JSON.parse(line);
}

export async function validate(task, row) {
  const out = { id: task.id };
  const tmp = path.join(tmpRoot(), 'cceval-bcb-validate', task.id);
  await rmrf(tmp);
  // The canonical solution runs twice: a positive control must pass deterministically.
  for (const mode of ['template', 'canonical', 'canonical_repeat']) {
    const ws = path.join(tmp, `${mode}-ws`);
    await materializeWorkspace(path.join(BCB_DIR, task.id, 'template'), ws);
    const override = mode.startsWith('canonical')
      ? (dir) => fsp.writeFile(path.join(dir, 'solution.py'), `${row.complete_prompt}\n${row.canonical_solution}`)
      : undefined;
    const res = await bcbScore(task, ws, path.join(tmp, `${mode}-score`), path.join(tmp, `${mode}-log`), `bcbval-${mode}`, { solutionOverride: override });
    const verdict = lastJson(res.output);
    out[mode] = res.passed ? 'pass' : 'fail';
    out[`${mode}_ms`] = res.ms;
    // Structural classification of the full checker output (exception names), for exclusion labels only.
    if (!res.passed) out[`${mode}_network`] = /URLError|NewConnectionError|ConnectionError|Operation not permitted|nodename nor servname/.test(res.output);
    if (!res.passed || mode === 'template') {
      out[`${mode}_detail`] = verdict
        ? { status: verdict.status ?? verdict.verdict, details: Object.fromEntries(Object.entries(verdict.details ?? {}).map(([k, v]) => [k, String(v).slice(-600)])), error: verdict.error ?? null }
        : { tail: res.output.slice(-600), timed_out: res.timed_out };
    }
  }
  await rmrf(tmp);
  return out;
}

/**
 * Exclusion per task from the recorded controls. Exactly one of the two canonical runs passing
 * means a nondeterministic test; then every split of that BigCodeBench task is excluded (both
 * splits run the same test code).
 */
export function bcbExclusions(tasks, byId) {
  const flaky = new Set(tasks.filter((t) => {
    const v = byId.get(t.id);
    return v && (v.canonical === 'pass') !== (v.canonical_repeat === 'pass');
  }).map((t) => t.bcb_task_id));
  return tasks.map((t) => {
    const v = byId.get(t.id);
    let excluded = null;
    if (flaky.has(t.bcb_task_id)) excluded = 'canonical solution passes only some runs (nondeterministic test)';
    else if (v && v.canonical !== 'pass') {
      excluded = v.canonical_network
        ? 'canonical solution needs internet access (tests fetch remote URLs); runs are offline'
        : 'canonical solution fails in this environment (macOS arm64; see validation detail)';
    } else if (v && v.template !== 'fail') excluded = 'untouched template already passes';
    return { id: t.id, lang: t.lang, group: t.group, ...(excluded ? { excluded } : {}) };
  });
}

export function lastJson(output) {
  const lines = output.trim().split('\n');
  for (let i = lines.length - 1; i >= 0; i--) {
    if (lines[i].startsWith('{')) { try { return JSON.parse(lines[i]); } catch { /* keep looking */ } }
  }
  return null;
}

export async function prepareBigcodebench({ force = false, validateAll = true } = {}) {
  const manifestPath = path.join(BCB_DIR, 'tasks.json');
  if (!force && await exists(manifestPath)) {
    log('bigcodebench: already prepared');
    return JSON.parse(await fsp.readFile(manifestPath, 'utf8'));
  }
  const sources = [await fetchPinned({
    url: `https://huggingface.co/datasets/${BCB.dataset}/resolve/${BCB.datasetRevision}/${BCB.file.name}`,
    dest: path.join(DATA, path.basename(BCB.file.name)), sha256: BCB.file.sha256,
    name: `${BCB.dataset} ${BCB.file.version}`, revision: BCB.datasetRevision,
  })];
  const checker = await ensureChecker();
  for (const pkg of BCB.nltkPackages) {
    const rec = await fetchPinned({
      url: `https://raw.githubusercontent.com/nltk/nltk_data/${BCB.nltkCommit}/packages/${pkg.path}.zip`,
      dest: path.join(DATA, 'nltk_data', `${pkg.path}.zip`), sha256: pkg.sha256, name: `nltk_data ${pkg.path}`, revision: BCB.nltkCommit,
    });
    const into = path.join(T.bcb, 'nltk_data', path.dirname(pkg.path));
    await ensureDir(into);
    await fsp.copyFile(rec.path, path.join(into, path.basename(rec.path)));
    await runOk('/usr/bin/unzip', ['-q', '-o', rec.path, '-d', into]);
    sources.push(rec);
  }
  sources.push({ name: 'BigCodeBench checker (bigcodebench/eval)', url: BCB.repo, revision: BCB.commit, sha256: null, bytes: null, files: checker.files, venv_lock_sha256: checker.lock_sha256 });
  await ensureManagedVenv(PREP_VENV, '3.12', [['pyarrow==21.0.0', 'pyyaml==6.0.2', 'packaging==25.0']]);
  await rmrf(BCB_DIR);
  await ensureDir(BCB_DIR);
  const extracted = path.join(BCB_DIR, '.rows.jsonl');
  await runOk(path.join(PREP_VENV, 'bin', 'python'), ['-I', path.join(HERE, 'extract.py'), sources[0].path, extracted]);
  const tasks = [];
  const rowsById = new Map();
  for await (const row of rows(extracted)) {
    const n = row.task_id.split('/')[1];
    for (const split of BCB.splits) {
      const id = `bcb-${split}-${n}`;
      const dir = path.join(BCB_DIR, id);
      await writeText(path.join(dir, 'template', 'solution.py'), split === 'complete' ? row.complete_prompt : '');
      await writeJson(path.join(dir, 'tests', 'task-test.json'), { task_id: row.task_id, test: row.test, entry_point: row.entry_point, code_prompt: row.code_prompt });
      await writeJson(path.join(dir, 'reference', 'canonical.json'), { complete_prompt: row.complete_prompt, canonical_solution: row.canonical_solution });
      const task = {
        id, suite: 'bigcodebench', lang: 'python', group: split, split, bcb_task_id: row.task_id, libs: row.libs,
        entry_point: row.entry_point, solution_files: ['solution.py'], file_list: 'solution.py', prompt: bcbPrompt(split, row),
      };
      await writeJson(path.join(dir, 'task.json'), task);
      tasks.push(task);
      rowsById.set(id, row);
    }
  }
  await rmrf(extracted);
  let validation = [];
  if (validateAll) {
    log(`bigcodebench: validating ${tasks.length} tasks (canonical passes, template fails)`);
    validation = await pool(tasks, 8, (t) => validate(t, rowsById.get(t.id)));
  }
  const byId = new Map(validation.map((v) => [v.id, v]));
  const entries = bcbExclusions(tasks, byId);
  const counts = {};
  for (const e of entries.filter((x) => !x.excluded)) counts[e.group] = (counts[e.group] ?? 0) + 1;
  const manifest = {
    suite: 'bigcodebench', sources, counts,
    grading: { source: `${BCB.repo} bigcodebench/eval untrusted_check (calibrated, evaluate() defaults)`, commit: BCB.commit, metric: 'pass@1 (one attempt)' },
    environment: { python: BCB.python, requirements: path.relative(path.join(HERE, '..', '..'), BCB.requirements), lock: path.relative(path.join(HERE, '..', '..'), BCB.lock), matplotlib_backend: 'Agg' },
    controls: { positive: 'canonical solution (complete_prompt + canonical_solution), twice', negative: 'untouched template (complete: signature + docstring; instruct: empty file)' },
    tasks: entries, validation,
  };
  await writeJson(manifestPath, manifest);
  return manifest;
}
