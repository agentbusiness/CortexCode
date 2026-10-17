// Agent workspaces, hidden-test integrity checks, and scoring copies.
import fs from 'node:fs';
import fsp from 'node:fs/promises';
import os from 'node:os';
import path from 'node:path';
import { cloneTree, ensureDir, exists, rmrf, run, runOk, sha256, walk } from './util.mjs';

const GIT_ENV = {
  GIT_AUTHOR_NAME: 'cceval', GIT_AUTHOR_EMAIL: 'cceval@localhost',
  GIT_COMMITTER_NAME: 'cceval', GIT_COMMITTER_EMAIL: 'cceval@localhost',
  GIT_AUTHOR_DATE: '2026-01-01T00:00:00Z', GIT_COMMITTER_DATE: '2026-01-01T00:00:00Z',
};

/**
 * Create an agent workspace from a template directory: a clonefile copy, a fresh git repository
 * holding a single snapshot commit (no upstream history, so nothing from the future commit leaks),
 * and dependency links (`links`: [{rel, target, mode: 'symlink'|'clone'}]) excluded from git.
 */
export async function materializeWorkspace(template, dest, { links = [], exclude = [] } = {}) {
  await cloneTree(template, dest);
  await rmrf(path.join(dest, '.git'));
  const env = { ...process.env, ...GIT_ENV };
  await runOk('git', ['init', '-q', '-b', 'main'], { cwd: dest, env });
  const excludeText = [...links.map((l) => `/${l.rel}`), ...exclude, '/.cortex-offline'].join('\n');
  await ensureDir(path.join(dest, '.git', 'info'));
  await fsp.writeFile(path.join(dest, '.git', 'info', 'exclude'), excludeText ? excludeText + '\n' : '');
  await runOk('git', ['add', '-A'], { cwd: dest, env });
  await runOk('git', ['commit', '-q', '--allow-empty', '--no-verify', '-m', 'task snapshot'], { cwd: dest, env });
  for (const link of links) {
    const at = path.join(dest, link.rel);
    await ensureDir(path.dirname(at));
    await rmrf(at);
    if (link.mode === 'clone') await cloneTree(link.target, at);
    else await fsp.symlink(link.target, at);
  }
  // Every benchmark workspace is offline for the model (the tools sandbox's marker: no network for
  // its commands, research:// search and fetch answer "offline"); a suite may write its own marker
  // over this one (polyglot adds the folders the model may not see).
  if (!fs.existsSync(path.join(dest, '.cortex-offline'))) await fsp.writeFile(path.join(dest, '.cortex-offline'), '');
  return dest;
}

/** Hash of every hidden file; used to detect a hidden file anywhere in a workspace. */
export async function hiddenIndex(files) {
  // files: [{rel, abs}]
  const out = [];
  for (const f of files) {
    const buf = await fsp.readFile(f.abs);
    out.push({ rel: f.rel, sha256: sha256(buf), sha256_lf: sha256(buf.toString('utf8').replace(/\r\n/g, '\n')), bytes: buf.length });
  }
  return out;
}

/**
 * Integrity check after an attempt: a hidden test file is "present" when any workspace file has its
 * exact content (or its content modulo CRLF). Files at a hidden path with different content (the
 * agent wrote its own test, or the pre-commit version of a modified test) are reported as
 * `path_collisions`, which are not leaks. Symlinks are not followed; `.git` is skipped (packed
 * objects are compressed, and the agent cannot read hidden files to commit them).
 */
export async function checkHiddenAbsent(workspace, hidden, { skipDirs = ['.git', 'node_modules', 'target', '.venv', '__pycache__'] } = {}) {
  // Nothing hidden to look for (the container suites' tests never leave their harness): no file can leak.
  if (!hidden.length) return { ok: true, leaks: [], path_collisions: [], files_checked: 0, hidden_files: 0 };
  const byHash = new Map();
  for (const h of hidden) {
    byHash.set(h.sha256, h);
    byHash.set(h.sha256_lf, h);
  }
  const sizes = new Set(hidden.map((h) => h.bytes));
  const files = await walk(workspace, { skipDirs });
  const leaks = [];
  const collisions = [];
  const hiddenPaths = new Set(hidden.map((h) => h.rel));
  for (const f of files) {
    if (f.symlink) {
      const target = await fsp.readlink(path.join(workspace, f.rel)).catch(() => '');
      if (hiddenPaths.has(f.rel)) collisions.push({ rel: f.rel, symlink: target });
      continue;
    }
    const abs = path.join(workspace, f.rel);
    const st = await fsp.stat(abs);
    let match = null;
    // Only files with a plausible size are hashed (exact size, or any size for CRLF variants).
    if (sizes.has(st.size) || st.size < 4 * 1024 * 1024) {
      const buf = await fsp.readFile(abs);
      match = byHash.get(sha256(buf)) ?? byHash.get(sha256(buf.toString('utf8').replace(/\r\n/g, '\n')));
    }
    if (match) leaks.push({ rel: f.rel, hidden: match.rel });
    else if (hiddenPaths.has(f.rel)) collisions.push({ rel: f.rel });
  }
  return { ok: leaks.length === 0, leaks, path_collisions: collisions, files_checked: files.length };
}

/**
 * Fresh scoring copy: clone the workspace, then restore the hidden tests over it (and apply
 * deletions the reference commit made). The agent's workspace itself is never modified.
 */
export async function scoringCopy(workspace, dest, { hiddenFiles = [], deletions = [] } = {}) {
  await cloneTree(workspace, dest);
  for (const f of hiddenFiles) {
    const at = path.join(dest, f.rel);
    await ensureDir(path.dirname(at));
    await rmrf(at);
    await fsp.copyFile(f.abs, at);
  }
  for (const rel of deletions) await rmrf(path.join(dest, rel));
  return dest;
}

/**
 * Unified diff of the workspace against its snapshot commit (what the agent changed), computed with
 * a private index file so the agent's own git index and history are left untouched.
 */
export async function workspaceDiff(workspace, snapshot) {
  if (!await exists(path.join(workspace, '.git'))) return '';
  const idx = path.join(os.tmpdir(), `cceval-idx-${process.pid}-${Date.now()}-${Math.random().toString(16).slice(2)}`);
  const env = { ...process.env, GIT_INDEX_FILE: idx };
  try {
    const base = snapshot ?? (await run('git', ['rev-list', '--max-parents=0', 'HEAD'], { cwd: workspace })).stdout.trim().split('\n').pop();
    await run('git', ['read-tree', base], { cwd: workspace, env });
    await run('git', ['add', '-A', '.'], { cwd: workspace, env, timeoutMs: 300000 });
    const res = await run('git', ['diff', '--cached', '--no-color', '--binary', base], { cwd: workspace, env, timeoutMs: 300000 });
    return res.stdout;
  } finally {
    await rmrf(idx);
  }
}

/** The snapshot commit of a freshly materialized workspace. */
export async function snapshotSha(workspace) {
  const res = await runOk('git', ['rev-parse', 'HEAD'], { cwd: workspace });
  return res.stdout.trim();
}

export function listFilesSync(dir) {
  const out = [];
  (function visit(rel) {
    for (const e of fs.readdirSync(path.join(dir, rel), { withFileTypes: true })) {
      const r = rel ? `${rel}/${e.name}` : e.name;
      if (e.isDirectory()) visit(r);
      else out.push(r);
    }
  })('');
  return out.sort();
}
