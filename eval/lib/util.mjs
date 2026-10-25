// Small process, filesystem and hashing helpers shared by the harness.
import { spawn } from 'node:child_process';
import crypto from 'node:crypto';
import fs from 'node:fs';
import fsp from 'node:fs/promises';
import path from 'node:path';

export const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

export function log(...args) {
  const ts = new Date().toISOString().slice(11, 19);
  console.error(`[${ts}]`, ...args);
}

/**
 * Run a command to completion. stdout and stderr are captured separately and also merged in
 * arrival order (`output`), which is what test logs show. The child gets its own process group so a
 * timeout kills everything it started.
 */
export function run(cmd, args = [], opts = {}) {
  const { cwd, env, timeoutMs, input, onData } = opts;
  return new Promise((resolve) => {
    const started = Date.now();
    let child;
    try {
      child = spawn(cmd, args, {
        cwd,
        env: env ?? process.env,
        detached: true,
        stdio: [input === undefined ? 'ignore' : 'pipe', 'pipe', 'pipe'],
      });
    } catch (error) {
      resolve({ code: null, signal: null, stdout: '', stderr: String(error), output: String(error), timedOut: false, ms: 0, error: String(error) });
      return;
    }
    const out = [];
    const err = [];
    const merged = [];
    let timedOut = false;
    let timer;
    if (timeoutMs) {
      timer = setTimeout(() => {
        timedOut = true;
        killGroup(child.pid, 'SIGTERM');
        setTimeout(() => killGroup(child.pid, 'SIGKILL'), 5000).unref();
      }, timeoutMs);
    }
    child.stdout.on('data', (d) => { out.push(d); merged.push(d); onData?.(d); });
    child.stderr.on('data', (d) => { err.push(d); merged.push(d); onData?.(d); });
    if (input !== undefined) {
      child.stdin.on('error', () => {});
      child.stdin.end(input);
    }
    child.on('error', (error) => {
      clearTimeout(timer);
      resolve({ code: null, signal: null, stdout: '', stderr: String(error), output: String(error), timedOut, ms: Date.now() - started, error: String(error) });
    });
    child.on('close', (code, signal) => {
      clearTimeout(timer);
      // Reap anything the command left behind in its group.
      killGroup(child.pid, 'SIGKILL');
      resolve({
        code,
        signal,
        stdout: Buffer.concat(out).toString('utf8'),
        stderr: Buffer.concat(err).toString('utf8'),
        output: Buffer.concat(merged).toString('utf8'),
        timedOut,
        ms: Date.now() - started,
      });
    });
  });
}

/** Run and throw unless the exit code is 0. */
export async function runOk(cmd, args = [], opts = {}) {
  const res = await run(cmd, args, opts);
  if (res.code !== 0) {
    const tail = res.output.slice(-4000);
    throw new Error(`command failed (${res.code ?? res.signal}${res.timedOut ? ', timed out' : ''}): ${cmd} ${args.join(' ')}\n${tail}`);
  }
  return res;
}

export function killGroup(pid, signal) {
  if (!pid) return;
  try { process.kill(-pid, signal); } catch { /* already gone */ }
}

export function sha256(data) {
  return crypto.createHash('sha256').update(data).digest('hex');
}

export async function sha256File(file) {
  return sha256(await fsp.readFile(file));
}

export async function ensureDir(dir) {
  await fsp.mkdir(dir, { recursive: true });
  return dir;
}

export async function rmrf(target) {
  await fsp.rm(target, { recursive: true, force: true, maxRetries: 3 });
}

export async function exists(p) {
  try { await fsp.lstat(p); return true; } catch { return false; }
}

export async function readJson(file, fallback) {
  try {
    return JSON.parse(await fsp.readFile(file, 'utf8'));
  } catch (error) {
    if (fallback !== undefined) return fallback;
    throw error;
  }
}

/** Atomic JSON write (temp file + rename), so an interrupted run never leaves a torn file. */
export async function writeJson(file, value) {
  await ensureDir(path.dirname(file));
  const tmp = `${file}.tmp-${process.pid}-${crypto.randomBytes(4).toString('hex')}`;
  await fsp.writeFile(tmp, JSON.stringify(value, null, 2) + '\n');
  await fsp.rename(tmp, file);
}

export async function writeText(file, text) {
  await ensureDir(path.dirname(file));
  const tmp = `${file}.tmp-${process.pid}-${crypto.randomBytes(4).toString('hex')}`;
  await fsp.writeFile(tmp, text);
  await fsp.rename(tmp, file);
}

export async function appendJsonl(file, value) {
  await ensureDir(path.dirname(file));
  await fsp.appendFile(file, JSON.stringify(value) + '\n');
}

export async function readJsonl(file) {
  let text;
  try { text = await fsp.readFile(file, 'utf8'); } catch { return []; }
  const rows = [];
  for (const line of text.split('\n')) {
    if (!line.trim()) continue;
    rows.push(JSON.parse(line));
  }
  return rows;
}

/**
 * Walk a directory without following symlinks. Returns relative POSIX paths of regular files and
 * symlinks (symlinks are reported, never traversed). `skipDirs` are directory basenames to skip.
 */
export async function walk(root, { skipDirs = [] } = {}) {
  const outFiles = [];
  async function visit(rel) {
    const abs = path.join(root, rel);
    let entries;
    try { entries = await fsp.readdir(abs, { withFileTypes: true }); } catch { return; }
    for (const entry of entries) {
      const childRel = rel ? `${rel}/${entry.name}` : entry.name;
      if (entry.isDirectory()) {
        if (skipDirs.includes(entry.name)) continue;
        await visit(childRel);
      } else if (entry.isFile() || entry.isSymbolicLink()) {
        outFiles.push({ rel: childRel, symlink: entry.isSymbolicLink() });
      }
    }
  }
  await visit('');
  outFiles.sort((a, b) => (a.rel < b.rel ? -1 : a.rel > b.rel ? 1 : 0));
  return outFiles;
}

/**
 * Copy a directory tree using APFS clonefile (`cp -c`), which is instant and shares blocks until
 * written. Falls back to a plain recursive copy on other filesystems.
 */
export async function cloneTree(src, dst) {
  await ensureDir(path.dirname(dst));
  await rmrf(dst);
  const res = await run('/bin/cp', ['-cRP', src, dst]);
  if (res.code !== 0) {
    await fsp.cp(src, dst, { recursive: true, verbatimSymlinks: true });
  }
}

/** Deterministic PRNG (mulberry32) so task selection is reproducible from a seed. */
export function prng(seed) {
  let a = typeof seed === 'number' ? seed >>> 0 : parseInt(sha256(String(seed)).slice(0, 8), 16);
  return () => {
    a = (a + 0x6d2b79f5) >>> 0;
    let t = a;
    t = Math.imul(t ^ (t >>> 15), t | 1);
    t ^= t + Math.imul(t ^ (t >>> 7), t | 61);
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
  };
}

export function shuffle(items, rand) {
  const arr = items.slice();
  for (let i = arr.length - 1; i > 0; i--) {
    const j = Math.floor(rand() * (i + 1));
    [arr[i], arr[j]] = [arr[j], arr[i]];
  }
  return arr;
}

export function parseArgs(argv) {
  const args = { _: [] };
  for (let i = 0; i < argv.length; i++) {
    const a = argv[i];
    if (a.startsWith('--')) {
      const eq = a.indexOf('=');
      if (eq > 0) { args[a.slice(2, eq)] = a.slice(eq + 1); continue; }
      const key = a.slice(2);
      const next = argv[i + 1];
      if (next === undefined || next.startsWith('--')) args[key] = true;
      else { args[key] = next; i++; }
    } else {
      args._.push(a);
    }
  }
  return args;
}

export function fileExistsSync(p) {
  try { fs.accessSync(p); return true; } catch { return false; }
}

/** Run async `fn` over `items` with at most `limit` in flight; results keep input order. */
export async function pool(items, limit, fn) {
  const results = new Array(items.length);
  let next = 0;
  const workers = Array.from({ length: Math.max(1, Math.min(limit, items.length)) }, async () => {
    while (true) {
      const i = next++;
      if (i >= items.length) return;
      results[i] = await fn(items[i], i);
    }
  });
  await Promise.all(workers);
  return results;
}
