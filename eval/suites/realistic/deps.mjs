// Dependency provisioning for realistic tasks.
//   prepare time (network allowed): install into caches keyed by lockfile content;
//   run/scoring time (offline): give a directory fresh dependencies from those caches.
import fs from 'node:fs';
import fsp from 'node:fs/promises';
import path from 'node:path';
import { tmpRoot } from '../../lib/paths.mjs';
import { cargoCmd, offlineEnv, pnpmBin, PINS, T } from '../../lib/toolchains.mjs';
import { cloneTree, ensureDir, exists, log, rmrf, run, runOk, sha256, writeJson } from '../../lib/util.mjs';

export const DEP_DIRS = { node: 'node_modules', python: '.venv', cargo: 'target' };

function subdirOf(repo, dir) {
  return repo.subdir ? path.join(dir, repo.subdir) : dir;
}

export async function nodeKey(repo, dir) {
  const root = subdirOf(repo, dir);
  const lock = await fsp.readFile(path.join(root, repo.deps.lockfile), 'utf8');
  const pkg = await fsp.readFile(path.join(root, 'package.json'), 'utf8');
  let ws = '';
  try { ws = await fsp.readFile(path.join(root, 'pnpm-workspace.yaml'), 'utf8'); } catch { /* none */ }
  return sha256(`${repo.deps.manager}\n${lock}\n${pkg}\n${ws}`).slice(0, 24);
}

/** Install node_modules for this lockfile into the cache (prepare time, network allowed). */
async function cacheNode(repo, dir) {
  const key = await nodeKey(repo, dir);
  const cacheDir = path.join(T.nodeModules, repo.id, key);
  if (await exists(path.join(cacheDir, 'node_modules'))) return { key };
  const scratch = path.join(tmpRoot(), 'cceval-npm', `${repo.id}-${key}`);
  await rmrf(scratch);
  await cloneTree(dir, scratch);
  const root = subdirOf(repo, scratch);
  await rmrf(path.join(root, 'node_modules'));
  const env = { ...process.env, npm_config_store_dir: T.pnpmStore, CI: '1' };
  log(`${repo.id}: installing node_modules (${key})`);
  if (repo.deps.manager === 'pnpm') {
    await runOk(pnpmBin(), ['install', '--frozen-lockfile', '--store-dir', T.pnpmStore, '--config.confirmModulesPurge=false'], { cwd: root, env, timeoutMs: 1800000 });
  } else {
    await runOk('npm', ['ci', '--no-audit', '--no-fund', '--loglevel=error'], { cwd: root, env, timeoutMs: 1800000 });
  }
  await ensureDir(cacheDir);
  await fsp.rename(path.join(root, 'node_modules'), path.join(cacheDir, 'node_modules'));
  await writeJson(path.join(cacheDir, 'meta.json'), { repo: repo.id, manager: repo.deps.manager, pnpm: PINS.pnpm });
  await rmrf(scratch);
  return { key };
}

const PY_ENV = () => ({ ...process.env, UV_CACHE_DIR: T.uvCache });

/** Create `.venv` with the package installed editable plus test extras. */
export async function makeVenv(repo, dir, { offline }) {
  const venv = path.join(dir, '.venv');
  await rmrf(venv);
  const env = { ...PY_ENV(), ...(offline ? { UV_OFFLINE: '1' } : {}) };
  const flag = offline ? ['--offline'] : [];
  await runOk('uv', ['venv', '-q', ...flag, '--python', PINS.python, venv], { cwd: dir, env, timeoutMs: 600000 });
  await runOk('uv', ['pip', 'install', '-q', ...flag, '--python', path.join(venv, 'bin', 'python'), '-e', dir, ...repo.deps.extras], { cwd: dir, env, timeoutMs: 1800000 });
}

/** Prepare-time dependency caching for one repository state. Returns a cache descriptor. */
export async function cacheDeps(repo, dir) {
  if (repo.deps.kind === 'node') return { kind: 'node', ...(await cacheNode(repo, dir)) };
  if (repo.deps.kind === 'python') {
    await makeVenv(repo, dir, { offline: false });
    await rmrf(path.join(dir, '.venv'));
    return { kind: 'python' };
  }
  if (repo.deps.kind === 'cargo') {
    const env = { ...process.env, ...offlineEnv(), CARGO_NET_OFFLINE: 'false' };
    await runOk(cargoCmd(), ['fetch'], { cwd: subdirOf(repo, dir), env, timeoutMs: 1800000 });
    return { kind: 'cargo' };
  }
  return { kind: 'none' };
}

/**
 * Give `dir` fresh dependencies (offline). `prebuiltTarget` (Rust) is a target directory built from
 * the task's template state, cloned so builds start incremental.
 */
export async function provideDeps(repo, task, dir) {
  const kind = repo.deps.kind;
  if (kind === 'node') {
    const src = path.join(T.nodeModules, repo.id, task.deps.key, 'node_modules');
    if (!fs.existsSync(src)) throw new Error(`node_modules cache missing for ${task.id}: ${src}`);
    await cloneTree(src, path.join(subdirOf(repo, dir), 'node_modules'));
  } else if (kind === 'python') {
    await makeVenv(repo, dir, { offline: true });
  } else if (kind === 'cargo') {
    const target = path.join(T.rustTargets, task.id);
    if (fs.existsSync(target)) await cloneTree(target, path.join(subdirOf(repo, dir), 'target'));
  }
}

export function depExcludes(repo) {
  const rel = DEP_DIRS[repo.deps.kind];
  if (!rel) return [];
  return [`/${repo.subdir ? `${repo.subdir}/` : ''}${rel}`];
}

/** Remove dependency directories (before re-provisioning a scoring copy). */
export async function stripDeps(repo, dir) {
  const rel = DEP_DIRS[repo.deps.kind];
  if (rel) await rmrf(path.join(subdirOf(repo, dir), rel));
}

/** Build the template state once so the agent and scoring start from warm, leak-free artifacts. */
export async function prebuildRust(repo, task, templateDir) {
  const target = path.join(T.rustTargets, task.id);
  await rmrf(target);
  const env = { ...process.env, ...offlineEnv(), CARGO_TARGET_DIR: target };
  const res = await run(cargoCmd(), repo.prebuild, { cwd: subdirOf(repo, templateDir), env, timeoutMs: 1800000 });
  return { ok: res.code === 0, ms: res.ms };
}
