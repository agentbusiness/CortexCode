// Prepare Terminal-Bench 2.0: the Harbor registry entry that defines terminal-bench@2.0 (pinned
// registry.json), the task repository at the commit that entry names, one Harbor task directory per
// task copied into the task store (Harbor runs it from there with `trial start -p`), and the Harbor
// venv (harbor 0.24.0 from a hashed lock, uv-managed CPython; never system Python or Homebrew).
//
// `--validate` (opt-in, Docker): run Harbor's oracle agent (the task's solution/solve.sh) on every
// task in Harbor's own docker environment and record the reward (positive control of the images
// and tests on this machine). Tasks are never dropped on its basis; the controls are recorded.
import fsp from 'node:fs/promises';
import path from 'node:path';
import { ensureGitCommit, fetchPinned, gitExport } from '../../lib/fetch.mjs';
import { dockerEnv } from '../../lib/docker.mjs';
import { SRC } from '../../lib/paths.mjs';
import { ensureManagedVenv, T } from '../../lib/toolchains.mjs';
import { cloneTree, ensureDir, exists, log, pool, readJson, rmrf, run, sha256, writeJson } from '../../lib/util.mjs';
import { harborBin, harborTaskDir, parseTrialResult, TB, TB_DIR, tbTaskFromDir } from './suite.mjs';

/** The terminal-bench@2.0 entry of a Harbor registry.json: [{name, git_url, git_commit_id, path}]. */
export function registryTasks(registry, name = 'terminal-bench', version = '2.0') {
  const entry = registry.find((d) => d.name === name && d.version === version);
  if (!entry) throw new Error(`registry has no ${name}@${version}`);
  return entry.tasks;
}

export async function ensureHarborVenv() {
  const lockText = await fsp.readFile(TB.requirements, 'utf8');
  await ensureManagedVenv(T.harbor, TB.python, [['--require-hashes', '-r', TB.requirements]], { stampExtra: { lock_sha256: sha256(lockText) } });
  return { lock_sha256: sha256(lockText) };
}

export async function prepareTerminalbench({ force = false, validate = false, only = null, workers = 2 } = {}) {
  const manifestPath = path.join(TB_DIR, 'tasks.json');
  const venv = await ensureHarborVenv();
  if (force || !await exists(manifestPath)) await buildStore(venv);
  else log('terminalbench: already prepared');
  if (validate) await validateOracle({ only, workers });
  return readJson(manifestPath);
}

async function buildStore(venv) {
  const reg = await fetchPinned({
    url: TB.registry.url, dest: path.join(SRC, 'datasets', 'harbor-registry.json'), sha256: TB.registry.sha256,
    name: 'Harbor registry (terminal-bench@2.0)', revision: TB.registry.revision,
  });
  const entries = registryTasks(JSON.parse(await fsp.readFile(reg.path, 'utf8')));
  const bad = entries.filter((t) => t.git_url !== TB.repo || t.git_commit_id !== TB.commit);
  if (bad.length || entries.length !== TB.count) {
    throw new Error(`terminal-bench@2.0 registry entry changed: ${entries.length} tasks, ${bad.length} not at ${TB.repo}@${TB.commit}`);
  }
  const repoDir = path.join(SRC, 'terminal-bench-2');
  const repo = await ensureGitCommit(repoDir, TB.repo, TB.commit);
  const exportDir = path.join(TB_DIR, '.export');
  await rmrf(TB_DIR);
  await gitExport(repoDir, TB.commit, exportDir);
  const tasks = [];
  for (const e of entries) {
    const dest = path.join(TB_DIR, e.name, 'task');
    await cloneTree(path.join(exportDir, e.path), dest);
    const task = await tbTaskFromDir(dest, e.name);
    await writeJson(path.join(TB_DIR, e.name, 'task.json'), task);
    // The container plugin mounts one workdir; compose environments (sidecar services) have no
    // equivalent there. None of the 89 TB 2.0 tasks has one; any that did would be listed here.
    tasks.push({ id: task.id, lang: task.lang, group: task.group, ...(task.has_compose ? { excluded: 'docker-compose environment (the container plugin attaches one container)' } : {}) });
  }
  await rmrf(exportDir);
  const manifest = {
    suite: 'terminalbench',
    sources: [reg, { ...repo, name: 'laude-institute/terminal-bench-2' }, { name: 'harbor', url: 'https://pypi.org/project/harbor/0.24.0/', revision: TB.harness, sha256: null, bytes: null, venv_lock_sha256: venv.lock_sha256 }],
    grading: { harness: TB.harness, call: 'harbor trial start -p <task> (Harbor verifier: tests/test.sh -> /logs/verifier/reward.txt)', metric: 'reward == 1, one trial' },
    tasks,
  };
  await writeJson(path.join(TB_DIR, 'tasks.json'), manifest);
  log(`terminalbench: ${tasks.length} tasks`);
}

async function validateOracle({ only, workers }) {
  const manifest = await readJson(path.join(TB_DIR, 'tasks.json'));
  const ids = manifest.tasks.filter((t) => !t.excluded).map((t) => t.id).filter((id) => !only || only.includes(id));
  const trialsDir = path.join(TB_DIR, '.oracle-trials');
  await ensureDir(trialsDir);
  const controls = {};
  await pool(ids, workers, async (id) => {
    const task = await readJson(path.join(TB_DIR, id, 'task.json'));
    const trialName = `${id}-oracle`.slice(0, 60);
    await rmrf(path.join(trialsDir, trialName));
    const res = await run(harborBin(), ['trial', 'start', '-p', harborTaskDir(task), '--agent', 'oracle', '--trials-dir', trialsDir, '--trial-name', trialName, '--no-delete'], {
      env: { ...process.env, ...(await dockerEnv()), HARBOR_TELEMETRY: '0' }, cwd: trialsDir,
      timeoutMs: ((task.build_timeout_sec ?? 600) + (task.agent_timeout_sec ?? 3600) + (task.verifier_timeout_sec ?? 3600) + 900) * 1000,
    });
    const v = parseTrialResult(await readJson(path.join(trialsDir, trialName, 'result.json'), null));
    controls[id] = { oracle_reward: v.reward, passed: v.passed, status: v.status, exception_type: v.exception_type, harbor_exit: res.code };
    log(`terminalbench oracle ${id}: ${v.passed ? 'pass' : `fail (${v.status})`}`);
  });
  manifest.controls = { positive: 'Harbor oracle agent (solution/solve.sh) in Harbor\'s docker environment', results: controls };
  await writeJson(path.join(TB_DIR, 'tasks.json'), manifest);
}
