// Realistic suite task protocol: workspace from the reverted commit, hidden commit tests, scoring.
import fs from 'node:fs';
import path from 'node:path';
import { LOCAL_REFS, HIDDEN, SRC } from '../../lib/paths.mjs';
import { scoringCopy } from '../../lib/workspace.mjs';
import { writeProfile } from '../../lib/sandbox.mjs';
import { offlineEnv, T } from '../../lib/toolchains.mjs';
import { ensureDir, readJson, rmrf, run, writeJson, writeText } from '../../lib/util.mjs';
import { COUNTERS, REALISTIC_REPOS } from './repos.mjs';
import { depExcludes, provideDeps, stripDeps } from './deps.mjs';

export const REALISTIC_DIR = path.join(HIDDEN, 'realistic');

export function repoById(id) {
  const r = REALISTIC_REPOS.find((x) => x.id === id);
  if (!r) throw new Error(`unknown realistic repo ${id}`);
  return r;
}

export async function loadRealisticTask(id) {
  return readJson(path.join(REALISTIC_DIR, id, 'task.json'));
}

export function templateDir(task) {
  return path.join(REALISTIC_DIR, task.id, 'template');
}

export function hiddenFiles(task, taskDir = path.join(REALISTIC_DIR, task.id)) {
  return task.hidden_tests.map((rel) => ({ rel, abs: path.join(taskDir, 'hidden', rel) }));
}

/** The reference (upstream) source change; never shown to agents, used to verify the task. */
export function solutionFiles(task) {
  return task.source_files.filter((f) => f.status !== 'D').map((f) => ({ rel: f.path, abs: path.join(REALISTIC_DIR, task.id, 'solution', f.path) }));
}

export function agentPath() {
  return [path.join(T.bats, 'bin')];
}

export function workspaceExcludes(task) {
  return depExcludes(repoById(task.repo));
}

/** Give a freshly materialized agent workspace its dependencies (offline). */
export async function provisionWorkspace(task, dir) {
  await provideDeps(repoById(task.repo), task, dir);
}

export function realisticEnv(task, dir) {
  const repo = repoById(task.repo);
  const env = offlineEnv(agentPath());
  if (repo.deps.kind === 'cargo') {
    // Mining verifies many states of one repository at one fixed path; it shares a target dir so
    // builds are incremental. Task workspaces and scoring copies use their own target dir.
    env.CARGO_TARGET_DIR = process.env.CCEVAL_MINING_CARGO_TARGET ?? path.join(repo.subdir ? path.join(dir, repo.subdir) : dir, 'target');
  }
  return env;
}

/**
 * Score a workspace: fresh copy, dependency dirs replaced from caches, hidden tests restored,
 * commit test deletions applied, then the repo's command on the commit's runnable tests.
 * Pass requires exit 0, no failures, and at least as many passing tests as the reference run, so
 * an agent cannot pass by making the test runner select fewer tests.
 */
export async function realisticScore(task, workspace, scoreDir, outDir, tag, { taskDir } = {}) {
  const repo = repoById(task.repo);
  const started = Date.now();
  await rmrf(scoreDir);
  await scoringCopy(workspace, scoreDir, { hiddenFiles: hiddenFiles(task, taskDir), deletions: task.test_deletions });
  await stripDeps(repo, scoreDir);
  await provideDeps(repo, task, scoreDir);
  await ensureDir(outDir);
  const profile = await writeProfile(path.join(outDir, 'score.sb'), { tag: tag.replace(/[^A-Za-z0-9_.-]/g, '_'), denyRead: [HIDDEN, SRC, ...LOCAL_REFS], denyWrite: [T.nodeModules, T.rustTargets, T.uvCache, T.bats] });
  const command = repo.testCommand(task.runnable_tests);
  const cwd = repo.subdir ? path.join(scoreDir, repo.subdir) : scoreDir;
  const env = { ...process.env, ...realisticEnv(task, scoreDir) };
  const res = await run('/usr/bin/sandbox-exec', ['-f', profile, ...command], { cwd, env, timeoutMs: repo.testTimeoutMs });
  const counts = COUNTERS[repo.counter](res.output);
  const minPassed = task.reference?.passed ?? 1;
  const passed = res.code === 0 && !res.timedOut && counts.found && counts.failed === 0 && counts.passed >= minPassed;
  const result = {
    command, cwd_rel: repo.subdir ?? '.', exit_code: res.code, signal: res.signal, timed_out: res.timedOut,
    ms: res.ms, counts, min_passed: minPassed, passed, started_ms: started, ended_ms: Date.now(),
  };
  await writeText(path.join(outDir, 'output.txt'), res.output);
  await writeJson(path.join(outDir, 'result.json'), result);
  await rmrf(scoreDir);
  return { ...result, output: res.output };
}

export function realisticPrompt(task) {
  let text = 'Make the following change in this repository. It is described by its commit message';
  text += task.issues.length ? ' and the issue it resolves.\n\n' : '.\n\n';
  text += `Commit message:\n\n${task.subject}\n`;
  if (task.body.trim()) text += `\n${task.body.trim()}\n`;
  for (const issue of task.issues) {
    text += `\nIssue #${issue.number}: ${issue.title}\n\n${(issue.body ?? '').trim()}\n`;
  }
  return text;
}

export function taskExists(id) {
  return fs.existsSync(path.join(REALISTIC_DIR, id, 'task.json'));
}
