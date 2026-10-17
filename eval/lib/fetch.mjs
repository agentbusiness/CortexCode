// Prepare-time downloads (network is used only here, never during runs or scoring). Every
// artifact is pinned by URL and SHA-256; a mismatch aborts preparation. The returned record
// ({name, url, revision, sha256, bytes}) goes into the suite's tasks.json and eval/manifest.json.
import fs from 'node:fs';
import path from 'node:path';
import { ensureDir, exists, log, rmrf, run, runOk, sha256File } from './util.mjs';

/**
 * Download `url` to `dest` unless it is already there with the pinned hash.
 * @param {object} a
 * @param {string} a.url
 * @param {string} a.dest
 * @param {string} a.sha256     pinned SHA-256 (hex); required
 * @param {string} [a.name]
 * @param {string} [a.revision] upstream revision the URL is pinned to (commit, tag, release)
 */
export async function fetchPinned({ url, dest, sha256, name, revision }) {
  if (!/^[0-9a-f]{64}$/.test(sha256 ?? '')) throw new Error(`fetchPinned ${url}: a pinned sha256 is required`);
  if (await exists(dest)) {
    const have = await sha256File(dest);
    if (have === sha256) return record();
    log(`${dest}: sha256 ${have} != pinned ${sha256}; downloading again`);
    await rmrf(dest);
  }
  await ensureDir(path.dirname(dest));
  const tmp = `${dest}.part`;
  await rmrf(tmp);
  log(`downloading ${url}`);
  await runOk('/usr/bin/curl', ['-fsSL', '--retry', '3', '-o', tmp, url], { timeoutMs: 3 * 3600 * 1000 });
  const got = await sha256File(tmp);
  if (got !== sha256) {
    await rmrf(tmp);
    throw new Error(`${url}: sha256 ${got} does not match the pinned ${sha256}`);
  }
  fs.renameSync(tmp, dest);
  return record();

  function record() {
    return { name: name ?? path.basename(dest), url, revision: revision ?? null, sha256, bytes: fs.statSync(dest).size, path: dest };
  }
}

/** Clone (or fetch) a git repository and make sure `commit` is present. Returns a source record. */
export async function ensureGitCommit(dir, url, commit) {
  if (!await exists(dir)) await runOk('git', ['clone', '-q', url, dir], { timeoutMs: 3600000 });
  const has = await run('git', ['cat-file', '-e', `${commit}^{commit}`], { cwd: dir });
  if (has.code !== 0) await runOk('git', ['fetch', '-q', 'origin'], { cwd: dir, timeoutMs: 3600000 });
  await runOk('git', ['cat-file', '-e', `${commit}^{commit}`], { cwd: dir });
  return { name: path.basename(dir), url, revision: commit, sha256: null, bytes: null, path: dir };
}

/** Extract the tree of `commit` into `dest` (no .git), via git archive. */
export async function gitExport(repoDir, commit, dest, paths = []) {
  await rmrf(dest);
  await ensureDir(dest);
  const quoted = paths.map((p) => `'${p.replace(/'/g, "'\\''")}'`).join(' ');
  await runOk('/bin/sh', ['-c', `git -C '${repoDir}' archive --format=tar ${commit} ${quoted} | tar -x -C '${dest}'`], { timeoutMs: 3600000 });
  return dest;
}
