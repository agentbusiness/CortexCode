// Scoring runs: always in a fresh copy of the agent's workspace with the hidden tests restored,
// under a sandbox that denies outbound network (tests must not reach the internet either).
import path from 'node:path';
import { scoringCopy } from './workspace.mjs';
import { writeProfile } from './sandbox.mjs';
import { ensureDir, rmrf, run, writeJson, writeText } from './util.mjs';

/**
 * @param {object} o
 * @param {string} o.workspace     agent workspace (read only here)
 * @param {string} o.scoreDir      where the fresh copy is made (deleted afterwards unless keep)
 * @param {string} o.outDir        where raw scoring logs are written (cmd, output, result)
 * @param {Array}  o.hiddenFiles   [{rel, abs}] restored over the copy
 * @param {Array}  o.deletions     paths the reference removed
 * @param {Function} o.setup       async (dir) => void, language-specific preparation
 * @param {string[]} o.command     argv; run with cwd = copy (+ o.subdir)
 * @param {object} o.env
 * @param {number} o.timeoutMs
 * @param {string} o.tag           sandbox tag for violation reports
 */
export async function scoreInCopy(o) {
  const started = Date.now();
  await rmrf(o.scoreDir);
  await scoringCopy(o.workspace, o.scoreDir, { hiddenFiles: o.hiddenFiles, deletions: o.deletions ?? [] });
  if (o.setup) await o.setup(o.scoreDir);
  const profile = await writeProfile(path.join(o.outDir, 'score.sb'), { tag: o.tag, denyRead: o.denyRead ?? [], denyWrite: o.denyWrite ?? [] });
  const cwd = o.subdir ? path.join(o.scoreDir, o.subdir) : o.scoreDir;
  const res = await run('/usr/bin/sandbox-exec', ['-f', profile, ...o.command], {
    cwd, env: { ...process.env, ...(o.env ?? {}) }, timeoutMs: o.timeoutMs,
  });
  const result = {
    command: o.command, cwd_rel: o.subdir ?? '.', exit_code: res.code, signal: res.signal,
    timed_out: res.timedOut, ms: res.ms, passed: res.code === 0 && !res.timedOut,
    started_ms: started, ended_ms: Date.now(),
  };
  await ensureDir(o.outDir);
  await writeText(path.join(o.outDir, 'output.txt'), res.output);
  await writeJson(path.join(o.outDir, 'result.json'), result);
  if (!o.keep) await rmrf(o.scoreDir);
  return { ...result, output: res.output };
}
