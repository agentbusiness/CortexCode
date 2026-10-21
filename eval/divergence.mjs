#!/usr/bin/env node
// Where runs of one task parted: for every task that passed in some runs and failed in others,
// the first step at which a passing run's journal and a failing run's journal differ, with the
// steps both took before it and each side's next steps. No model is involved. Where repeated runs
// diverge, the harness left a decision to chance; the step before the divergence is where to look
// for the cue.
//
//   node eval/divergence.mjs [--results DIR] [--suite NAME] [--system cortex] [--model VARIANT] [--out FILE]
//
// A step is one stage decision (`route` entries), one model tool call (host reads left out) with
// whether it failed, or one host note to the model (by purpose). Commands and paths are compared
// with numbers taken out, so a moved line or a timing does not count as a difference.
import fs from 'node:fs';
import path from 'node:path';
import { RESULTS } from './lib/paths.mjs';
import { findRuns } from './lib/reportlib.mjs';
import { parseArgs, readJson, writeText } from './lib/util.mjs';

const SHARED = 3;
const NEXT = 4;

function dirs(p) {
  try { return fs.readdirSync(p, { withFileTypes: true }).filter((d) => d.isDirectory()).map((d) => d.name).sort(); } catch { return []; }
}

const squash = (text, max = 120) => String(text ?? '').replace(/\d+/g, 'N').replace(/\s+/g, ' ').trim().slice(0, max);

/** The steps of a task's journal (its first session, the one the runner opened), in order. */
function steps(journals, sessionId) {
  const entries = journals?.[sessionId]?.entries ?? Object.values(journals ?? {})[0]?.entries ?? [];
  const results = new Map(entries.filter((e) => e.kind === 'tool_result').map((e) => [e.data?.call_id, e.data]));
  const out = [];
  for (const e of entries) {
    const d = e.data ?? {};
    if (e.kind === 'route') {
      const choice = d.route ?? d.detail?.route ?? '';
      out.push({ key: `${d.stage ?? 'stage'} -> ${choice}`, text: `${d.stage ?? 'stage'} chose ${choice}`, seq: e.seq });
    } else if (e.kind === 'tool_call' && d.host !== true) {
      const args = d.args ?? {};
      const target = args.command ?? args.path ?? args.target ?? '';
      const result = results.get(d.call_id);
      const failed = result && result.ok === false;
      const kind = failed ? ` (failed${result.error_kind ? `: ${result.error_kind}` : ''})` : '';
      out.push({ key: `${d.tool} ${squash(target)}${failed ? ' !' : ''}`, text: `${d.tool} ${String(target).slice(0, 160)}${kind}${d.intent ? ` -- ${String(d.intent).slice(0, 160)}` : ''}`, seq: e.seq });
    } else if (e.kind === 'note' && d.host_message === true && d.purpose) {
      out.push({ key: `note ${d.purpose}`, text: `host note (${d.purpose}): ${squash(d.text, 160)}`, seq: e.seq });
    }
  }
  return out;
}

/** One task of one run: passed or not, and the steps of its first attempt. */
async function readTask(runDir, taskId) {
  const dir = path.join(runDir, 'tasks', taskId);
  const rec = await readJson(path.join(dir, 'result.json'), null);
  if (!rec || rec.aborted) return null;
  const journals = await readJson(path.join(dir, 'attempt-1', 'journal.json'), null);
  if (!journals) return null;
  return { passed: (rec.attempts ?? []).some((a) => a.passed), steps: steps(journals, rec.session_id) };
}

function firstDifference(a, b) {
  const n = Math.min(a.length, b.length);
  for (let i = 0; i < n; i++) if (a[i].key !== b[i].key) return i;
  return n;
}

async function main() {
  const args = parseArgs(process.argv.slice(2));
  const root = args.results ?? RESULTS;
  const runs = findRuns(root).filter((r) => (!args.suite || r.suite === args.suite) && (!args.system || r.system === args.system) && (!args.model || r.model === args.model));
  const groups = new Map();
  for (const run of runs) {
    const key = `${run.suite}/${run.system}/${run.model}`;
    if (!groups.has(key)) groups.set(key, []);
    groups.get(key).push(run);
  }
  const L = ['# Where runs of one task parted', '', `From \`${path.relative(process.cwd(), root) || root}\`. A task is listed when some runs passed and others failed.`, ''];
  let listed = 0;
  for (const [group, list] of groups) {
    const tasks = new Set(list.flatMap((run) => dirs(path.join(run.dir, 'tasks'))));
    for (const taskId of [...tasks].sort()) {
      const outcomes = [];
      for (const run of list) {
        const t = await readTask(run.dir, taskId);
        if (t) outcomes.push({ run: run.run, ...t });
      }
      const pass = outcomes.filter((o) => o.passed);
      const fail = outcomes.filter((o) => !o.passed);
      if (!pass.length || !fail.length) continue;
      listed += 1;
      L.push(`## ${group} ${taskId}: passed ${pass.length}/${outcomes.length}`, '');
      // Each failing run against the passing run it shares the longest start with.
      for (const f of fail) {
        const p = pass.map((o) => ({ o, at: firstDifference(o.steps, f.steps) })).sort((x, y) => y.at - x.at)[0];
        const at = p.at;
        L.push(`### ${f.run} (failed) against ${p.o.run} (passed): parted at step ${at + 1}`, '');
        const before = f.steps.slice(Math.max(0, at - SHARED), at);
        if (before.length) {
          L.push('Both took:', ...before.map((s) => `- ${s.text}`), '');
        }
        L.push(`${p.o.run} (passed) went on:`, ...(p.o.steps.slice(at, at + NEXT).map((s) => `- ${s.text}`)), '');
        L.push(`${f.run} (failed) went on (journal seq ${f.steps[at]?.seq ?? 'end'}):`, ...(f.steps.slice(at, at + NEXT).map((s) => `- ${s.text}`)), '');
        L.push(`Steps: ${p.o.steps.length} passed, ${f.steps.length} failed.`, '');
      }
    }
  }
  if (!listed) L.push('No task passed in some runs and failed in others.', '');
  const md = L.join('\n');
  if (args.out) {
    await writeText(args.out, md);
    console.log(`wrote ${args.out} (${listed} tasks)`);
  } else {
    process.stdout.write(md);
  }
}

main().catch((error) => {
  console.error(error);
  process.exit(1);
});
