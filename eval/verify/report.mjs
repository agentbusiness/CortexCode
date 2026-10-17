#!/usr/bin/env node
// G4: the report generator recomputes everything from raw logs. A hand-made fixture with known
// outcomes is written to a temp dir; expected numbers below were computed by hand / taken from
// textbook tables (Wilson 95% for 7/10, 3/10, 2/10; McNemar exact for 6 vs 1 discordant pairs),
// not produced by the code under test. Negative controls: a result.json that lies about passing
// must be overruled by the raw exit code, and a realistic run that passes fewer tests than the
// reference must fail.
import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { spawnSync } from 'node:child_process';
import { EVAL_ROOT } from '../lib/paths.mjs';
import { ensurePyTools } from '../lib/toolchains.mjs';

const root = fs.mkdtempSync(path.join(os.tmpdir(), 'cceval-report-fixture-'));
const results = path.join(root, 'results');
const hidden = path.join(root, 'hidden');
const write = (p, data) => { fs.mkdirSync(path.dirname(p), { recursive: true }); fs.writeFileSync(p, typeof data === 'string' ? data : JSON.stringify(data, null, 2)); };

function makeRun(suite, system, model, taskIds, perTask) {
  const dir = path.join(results, suite, system, model, 'run-1');
  write(path.join(dir, 'run.json'), { suite, system, system_key: system, model, run: 1, tasks: taskIds, task_count: taskIds.length });
  for (const id of taskIds) {
    const spec = perTask(id);
    const tdir = path.join(dir, 'tasks', id);
    spec.attempts.forEach((a, i) => {
      const adir = path.join(tdir, `attempt-${i + 1}`);
      write(path.join(adir, 'attempt.json'), { attempt: i + 1, passed: a.recorded ?? false, timed_out: !!a.timedOut, turn: { started_ms: 0, ended_ms: 1000, timed_out: !!a.timedOut, terminal: { state: 'done' } } });
      write(path.join(adir, 'hidden_check.json'), { ok: !a.leak, leaks: a.leak ? [{ rel: 'x', hidden: 'tests/x' }] : [], path_collisions: [] });
      write(path.join(adir, 'diff.patch'), a.emptyDiff ? '' : 'diff --git a/x b/x\n');
      if (a.exit !== undefined) {
        write(path.join(adir, 'score', 'result.json'), { exit_code: a.exit, timed_out: false });
        write(path.join(adir, 'score', 'output.txt'), a.output ?? `exit ${a.exit}\n`);
      }
      if (a.final !== undefined) write(path.join(adir, 'score', 'final', spec.path), a.final);
    });
    write(path.join(tdir, 'result.json'), { task_id: id, passed: spec.recordedPassed ?? false, started_ms: 1_000_000, ended_ms: 1_060_000, ...(spec.runner ? { runner: spec.runner } : {}) });
    write(path.join(tdir, 'violations.json'), { events: spec.netDenials ? [{ kind: 'net' }] : [] });
    if (system === 'cortex') {
      const n = Number(id.replace(/\D/g, ''));
      write(path.join(tdir, 'journal.json'), { s1: { entries: [
        { seq: 1, kind: 'usage', data: { prompt_tokens: n % 2 ? 1000 : 2000, cached_tokens: 0, completion_tokens: 100, prefill_ms: 500, decode_ms: 2000 } },
        { seq: 2, kind: 'tool_call', data: { tool: 'edit', args: { path: 'a', old: 'x', new: 'y' } } },
      ] } });
    }
  }
}

try {
  await ensurePyTools();
  // --- polyglot: 10 tasks, cortex vs aider ---
  const pg = Array.from({ length: 10 }, (_, i) => `pg-${i < 5 ? 'python' : 'rust'}-t${i + 1}`);
  for (const id of pg) write(path.join(hidden, 'polyglot', id, 'task.json'), { id, lang: id.split('-')[1] });
  const n = (id) => Number(id.split('-t')[1]);
  makeRun('polyglot', 'cortex', 'm1', pg, (id) => {
    const k = n(id);
    if (k <= 3) return { attempts: [{ exit: 0, recorded: true }], recordedPassed: true };
    if (k <= 7) return { attempts: [{ exit: 1 }, { exit: 0, recorded: true }], recordedPassed: true };
    if (k === 8) return { attempts: [{ exit: 1 }, { exit: 1 }] };
    if (k === 9) return { attempts: [{ exit: 0, timedOut: true }] };
    return { attempts: [{ exit: 1, emptyDiff: true }, { exit: 1, emptyDiff: true }], netDenials: true };
  });
  makeRun('polyglot', 'aider', 'm1', pg, (id) => {
    const k = n(id);
    const runner = { finished: { prompt_tokens: 3000, completion_tokens: 300 } };
    if (k === 1 || k === 8) return { attempts: [{ exit: 0, recorded: true }], recordedPassed: true, runner };
    // Negative control: the recorded summary claims a pass the raw exit code does not support.
    if (k === 9) return { attempts: [{ exit: 1 }, { exit: 1 }], recordedPassed: true, runner };
    if (k === 10) return { attempts: [{ exit: 0, leak: true }], runner };
    return { attempts: [{ exit: 1 }, { exit: 1 }], runner };
  });

  // --- editbench: exact, format-only difference, real difference ---
  const original = 'def f(a, b):\n    if a > b:\n        return a\n    return b\n';
  const eb = ['eb-python-001', 'eb-python-002', 'eb-python-003'];
  for (const id of eb) {
    write(path.join(hidden, 'editbench', id, 'task.json'), { id, lang: 'python', path: 'pkg/m.py', mutation: { type: 'operator_swap' } });
    write(path.join(hidden, 'editbench', id, 'original', 'pkg/m.py'), original);
  }
  makeRun('editbench', 'cortex-replace', 'm1', eb, (id) => ({
    path: 'pkg/m.py',
    attempts: [{ final: id.endsWith('1') ? original : id.endsWith('2') ? 'def f(a,b):\n  if a>b:\n      return a\n  return b\n' : original.replace('a > b', 'a < b') }],
  }));

  // --- realistic: count rule ---
  const rl = ['rl-click-aaaa', 'rl-click-bbbb'];
  for (const id of rl) write(path.join(hidden, 'realistic', id, 'task.json'), { id, lang: 'python', repo: 'click', origin: 'public', reference: { passed: 6 } });
  makeRun('realistic', 'cortex', 'm1', rl, (id) => ({
    attempts: [{ exit: 0, output: id.endsWith('aaaa') ? '====== 6 passed in 0.1s ======\n' : '====== 5 passed, 1 skipped in 0.1s ======\n' }],
  }));
  for (const id of rl) {
    const tdir = path.join(results, 'realistic', 'cortex', 'm1', 'run-1', 'tasks', id);
    const r = JSON.parse(fs.readFileSync(path.join(tdir, 'result.json'), 'utf8'));
    write(path.join(tdir, 'result.json'), { ...r, started_ms: 1_000_000, ended_ms: id.endsWith('aaaa') ? 1_030_000 : 1_900_000 });
  }

  write(path.join(root, 'published.json'), { entries: [{ benchmark: 'Aider Polyglot', metric: 'pass_rate_2', system_or_model: 'Example', harness: 'aider', value: 50, as_of: '2026-01-01', source_url: 'https://example.org', source_title: 'Example source' }] });

  const out = path.join(root, 'REPORT.md');
  const res = spawnSync(process.execPath, [path.join(EVAL_ROOT, 'report.mjs'), '--results', results, '--hidden', hidden, '--published', path.join(root, 'published.json'), '--out', out], { encoding: 'utf8' });
  assert.equal(res.status, 0, res.stderr);
  const md = fs.readFileSync(out, 'utf8');
  const lines = md.split('\n');
  // A table row that starts with `prefix` and contains `expected`.
  const has = (prefix, expected) => lines.some((l) => l.startsWith(prefix) && l.includes(expected));
  const show = (prefix) => lines.filter((l) => l.startsWith(prefix)).join('\n');

  // Pass rates with Wilson 95% intervals (textbook: 7/10 -> [39.7, 89.2], 3/10 -> [10.8, 60.3], 2/10 -> [5.7, 51.0]).
  const P = '| polyglot | cortex | m1 |';
  assert.ok(has(P, '| 70.0% [39.7%, 89.2%] | 7/10 | 30.0% [10.8%, 60.3%] |'), show(P));
  const A = '| polyglot | aider | m1 |';
  assert.ok(has(A, '| 20.0% [5.7%, 51.0%] | 2/10 |'), `aider (lying result.json must not count):\n${show(A)}`);
  // Paired comparison: aider vs cortex, both 1, only aider 1, only cortex 6, neither 2; exact p = 16/128.
  assert.ok(has('| aider | cortex | m1 |', '| 10 | 1 | 1 | 6 | 2 | -50.0 pts | 0.1250 |'), show('| aider | cortex |'));
  // Editbench: exact and format-only difference pass, semantic difference fails -> 2/3.
  assert.ok(has('| editbench | cortex-replace | m1 |', '| 2/3 |'), show('| editbench |'));
  // Before formatting only the byte-identical file counts: 1/3 (Wilson 1/3 -> [6.1%, 79.2%]).
  assert.ok(lines.includes('| cortex-replace | m1 | 66.7% [20.8%, 93.9%] (2/3) | 33.3% [6.1%, 79.2%] (1/3) |'), show('| cortex-replace |'));
  // Realistic: fewer passing tests than the reference fails -> 1/2.
  assert.ok(has('| realistic | cortex | m1 |', '| 1/2 |'), show('| realistic |'));
  // Tokens and wall time: prompt mean (5x1000 + 5x2000)/10 = 1500, completion 100, prefill 0.5 s, decode 2.0 s, wall 60 s.
  assert.ok(has(P, '| 1500 | 0 | 100 / 100 | 0.5 | 2.0 | 1.0 | 60 / 60 |'), show(P));
  assert.ok(has(A, '| 3000 | n/a | 300 / 300 | n/a | n/a |'), show(A));
  // Wall time uses every task: realistic cortex walls are hand-set to 30 s and 900 s -> mean and median 465 s.
  assert.ok(has('| realistic | cortex | m1 |', '| 465 / 465 |'), show('| realistic | cortex |'));
  // Failure classes: cortex polyglot has tests_failed 1 (t8), timeout 1 (t9), no_change 1 (t10).
  const fcHead = md.split('\n').find((l) => l.startsWith('| suite | system | model |') && l.includes('no_change'));
  const fcRow = md.split('\n').filter((l) => l.startsWith('| polyglot | cortex | m1 |')).find((l) => l.split('|').length === fcHead.split('|').length);
  const cols = fcHead.split('|').map((s) => s.trim());
  const vals = fcRow.split('|').map((s) => s.trim());
  const fc = Object.fromEntries(cols.map((c, i) => [c, vals[i]]));
  assert.equal(fc.tests_failed, '1');
  assert.equal(fc.timeout, '1');
  assert.equal(fc.no_change, '1');
  // Integrity: aider has one hidden-file leak, cortex one denied network attempt.
  assert.ok(lines.includes('| polyglot | aider | m1 | 1 | 0 | 0 |'), `aider leak row\n${show(A)}`);
  assert.ok(lines.includes('| polyglot | cortex | m1 | 0 | 1 | 0 |'), `cortex net denial row\n${show(P)}`);
  // Data consistency flags the lying summary.
  assert.ok(md.includes('pg-rust-t9: recorded outcome differs from recomputation'), 'inconsistency listed');
  // Published numbers sit in their own labeled section with a source link.
  const pubIdx = md.indexOf('## Published reference numbers (not measured by us)');
  assert.ok(pubIdx > 0 && md.indexOf('[Example source](https://example.org)') > pubIdx);
  assert.ok(md.indexOf('Example source') > pubIdx, 'published numbers only in the published section');
  console.log('report verification passed');
} catch (error) {
  console.error(error);
  process.exitCode = 1;
} finally {
  fs.rmSync(root, { recursive: true, force: true });
}
