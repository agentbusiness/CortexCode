// macOS sandbox-exec confinement for everything that runs agent-influenced code:
// the frame and its plugins, the baseline harnesses, and scoring test runs.
//
// Policy (structural, enforced by the kernel):
//   - outbound network is denied except to localhost (IP) and Unix sockets;
//   - shared toolchain caches (node_modules, venvs, prebuilt targets) are read-only, so one task
//     cannot alter what another task or a scoring run executes;
//   - reads of the hidden areas (hidden tests, originals, upstream clones, results, the owner's
//     original reference repository, the run's scoring copies) are denied;
//   - every denial carries a message tag `cceval:<tag>:<kind>` and is reported by the kernel to the
//     unified log, which `violations()` reads back so attempts to reach the network or read hidden
//     paths are recorded per attempt.
import fs from 'node:fs';
import path from 'node:path';
import { run, writeText } from './util.mjs';

export const SANDBOX_EXEC = '/usr/bin/sandbox-exec';

function real(p) {
  try { return fs.realpathSync(p); } catch { return path.resolve(p); }
}

function sbString(s) {
  return '"' + s.replace(/\\/g, '\\\\').replace(/"/g, '\\"') + '"';
}

/** Build a profile. `denyRead` paths are hidden; `tag` labels every denial in the log. */
export function buildProfile({ tag, denyRead = [], denyWrite = [], allowNetwork = false }) {
  if (!/^[A-Za-z0-9_.-]+$/.test(tag)) throw new Error(`invalid sandbox tag ${tag}`);
  const lines = ['(version 1)', '(allow default)'];
  if (!allowNetwork) {
    lines.push(`(deny network-outbound (with message ${sbString(`cceval:${tag}:net`)}))`);
    lines.push('(allow network-outbound (remote ip "localhost:*"))');
    lines.push('(allow network-outbound (remote unix-socket))');
  }
  const seen = new Set();
  for (const p of denyRead) {
    for (const variant of [path.resolve(p), real(p)]) {
      if (seen.has(variant)) continue;
      seen.add(variant);
      lines.push(`(deny file-read* (subpath ${sbString(variant)}) (with message ${sbString(`cceval:${tag}:hidden`)}))`);
    }
  }
  for (const p of denyWrite) {
    for (const variant of new Set([path.resolve(p), real(p)])) {
      lines.push(`(deny file-write* (subpath ${sbString(variant)}) (with message ${sbString(`cceval:${tag}:cachewrite`)}))`);
    }
  }
  return lines.join('\n') + '\n';
}

export async function writeProfile(file, opts) {
  await writeText(file, buildProfile(opts));
  return file;
}

/** Prefix a command so it runs under the profile. */
export function wrap(profileFile, cmd, args = []) {
  return { cmd: SANDBOX_EXEC, args: ['-f', profileFile, cmd, ...args] };
}

function logTime(ms) {
  const d = new Date(ms);
  const pad = (n) => String(n).padStart(2, '0');
  return `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())} ${pad(d.getHours())}:${pad(d.getMinutes())}:${pad(d.getSeconds())}`;
}

/**
 * Read sandbox denials carrying `tag` between `startMs` and `endMs` from the unified log.
 * Kernel reports can lag slightly, so the window is padded and callers query after the attempt.
 * Returns [{ts, kind, process, operation, target, message}].
 */
export async function violations(tag, startMs, endMs) {
  const res = await run('/usr/bin/log', [
    'show', '--style', 'ndjson',
    '--start', logTime(startMs - 2000),
    '--end', logTime(endMs + 5000),
    '--predicate', `eventMessage CONTAINS "cceval:${tag}:"`,
  ], { timeoutMs: 120000 });
  const out = [];
  for (const line of res.stdout.split('\n')) {
    if (!line.startsWith('{')) continue;
    let rec;
    try { rec = JSON.parse(line); } catch { continue; }
    const msg = rec.eventMessage;
    if (typeof msg !== 'string' || !msg.includes(`cceval:${tag}:`)) continue;
    if (msg.startsWith('log run noninteractively')) continue;
    // Structural parse of the kernel's report: "Sandbox: proc(pid) deny(1) op target\ncceval:tag:kind"
    const m = msg.match(/Sandbox: (.+?)\((\d+)\) deny\(\d+\) (\S+)\s*([^\n]*)\n?cceval:[^:]+:(\w+)/);
    out.push({
      ts: rec.timestamp,
      duplicate: msg.includes('duplicate report'),
      process: m ? m[1] : null,
      pid: m ? Number(m[2]) : null,
      operation: m ? m[3] : null,
      target: m ? m[4].trim() : null,
      kind: m ? m[5] : (msg.includes(':net') ? 'net' : 'hidden'),
      message: msg,
    });
  }
  return out;
}
