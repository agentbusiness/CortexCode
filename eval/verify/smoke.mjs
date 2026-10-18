#!/usr/bin/env node
// G3: smoke through the real frame and plugins (small model): each suite runs 3 tasks end to end
// and produces scored results; aider and mini-swe-agent run the same polyglot and realistic tasks
// through the model plugin's OpenAI-compatible endpoint. Results go to eval/results/smoke/ and a
// smoke report is generated from them.
//
// The frame is built when its sources exist; plugin executables are waited for (polling with
// sleeps) because other leaves build them concurrently.
import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import { FRAME_BIN, REPO_ROOT, RESULTS } from '../lib/paths.mjs';
import { runSuite } from '../lib/runner.mjs';
import { buildReport } from '../report.mjs';
import { log, readJson, rmrf, run, writeText } from '../lib/util.mjs';

// The small test model of the default engine (uzu's converted Qwen3.5-4B); override with CCEVAL_SMOKE_MODEL.
const MODEL = process.env.CCEVAL_SMOKE_MODEL ?? 'qwen3.5-4b-m';
const OUT = path.join(RESULTS, 'smoke');
const LIMIT = 3;
const TIMEOUT_MS = 20 * 60000;

async function buildFrame() {
  const frameDir = path.join(REPO_ROOT, 'frame');
  const deadline = Date.now() + 6 * 3600 * 1000;
  while (!fs.existsSync(path.join(frameDir, 'Cargo.toml'))) {
    if (Date.now() > deadline) throw new Error('frame sources never appeared');
    log('waiting for frame sources');
    await new Promise((r) => setTimeout(r, 60000));
  }
  log('building the frame (cargo build --release)');
  const res = await run('cargo', ['build', '--release'], { cwd: frameDir, timeoutMs: 3600000 });
  assert.equal(res.code, 0, `frame build failed:\n${res.output.slice(-3000)}`);
  assert.ok(fs.existsSync(FRAME_BIN));
}

function checkRunDir(dir, { system, suite }) {
  const runMeta = JSON.parse(fs.readFileSync(path.join(dir, 'run.json'), 'utf8'));
  const status = JSON.parse(fs.readFileSync(path.join(dir, 'status.json'), 'utf8'));
  assert.equal(runMeta.tasks.length, LIMIT, `${dir}: ${runMeta.tasks.length} tasks`);
  assert.ok(status.complete, `${dir}: incomplete (${JSON.stringify(status)})`);
  assert.ok(fs.existsSync(path.join(dir, 'frame.sb')), `${dir}: frame sandbox profile missing`);
  assert.ok(fs.readFileSync(path.join(dir, 'frame.sb'), 'utf8').includes('(deny network-outbound'), 'frame ran under the network-denying profile');
  const stats = { toolCalls: 0, usage: 0, passed: 0 };
  for (const id of runMeta.tasks) {
    const tdir = path.join(dir, 'tasks', id);
    const result = JSON.parse(fs.readFileSync(path.join(tdir, 'result.json'), 'utf8'));
    assert.ok(!result.error, `${id}: harness error ${result.error}`);
    assert.ok(result.attempts.length >= 1, `${id}: no attempts`);
    for (let k = 1; k <= result.attempts.length; k++) {
      const adir = path.join(tdir, `attempt-${k}`);
      const hidden = JSON.parse(fs.readFileSync(path.join(adir, 'hidden_check.json'), 'utf8'));
      assert.ok(hidden.ok, `${id} attempt ${k}: hidden files present in workspace`);
      assert.ok(fs.existsSync(path.join(adir, 'score', 'result.json')), `${id} attempt ${k}: not scored`);
      assert.ok(fs.existsSync(path.join(adir, 'diff.patch')), `${id} attempt ${k}: no diff recorded`);
    }
    assert.ok(fs.existsSync(path.join(tdir, 'violations.json')), `${id}: violations not recorded`);
    if (system === 'cortex') {
      const journals = JSON.parse(fs.readFileSync(path.join(tdir, 'journal.json'), 'utf8'));
      const entries = Object.values(journals).flatMap((j) => j.entries ?? []);
      assert.ok(entries.some((e) => e.kind === 'user_message'), `${id}: no user message in journal`);
      assert.ok(entries.some((e) => e.kind === 'status' && ['done', 'failed', 'cancelled', 'stopped'].includes(e.data?.state)) || result.attempts.at(-1).timed_out, `${id}: run never ended`);
      const usage = entries.filter((e) => e.kind === 'usage').length;
      assert.ok(usage > 0, `${id}: the model was never called`);
      stats.usage += usage;
      stats.toolCalls += entries.filter((e) => e.kind === 'tool_call' && !e.data?.host).length;
    } else {
      const fin = result.runner?.finished;
      assert.ok(fin, `${id}: ${system} runner did not finish (${JSON.stringify(result.runner?.exit)})`);
      assert.ok(Number(fin.prompt_tokens) > 0, `${id}: ${system} never reached the model endpoint`);
      stats.usage += 1;
    }
    if (result.passed) stats.passed++;
  }
  return stats;
}

try {
  await buildFrame();
  await rmrf(OUT);
  const plan = [
    { suite: 'editbench', system: 'cortex', editFormat: 'hashline' },
    { suite: 'editbench', system: 'cortex', editFormat: 'replace' },
    { suite: 'editbench', system: 'cortex', editFormat: 'whole' },
    { suite: 'polyglot', system: 'cortex' },
    { suite: 'polyglot', system: 'aider' },
    { suite: 'polyglot', system: 'mini' },
    { suite: 'realistic', system: 'cortex' },
    { suite: 'realistic', system: 'aider' },
    { suite: 'realistic', system: 'mini' },
  ];
  const summary = [];
  let cortexToolCalls = 0;
  for (const p of plan) {
    log(`smoke: ${p.suite} / ${p.system}${p.editFormat ? ` (${p.editFormat})` : ''}`);
    const [res] = await runSuite({ ...p, model: MODEL, runs: 1, limit: LIMIT, timeoutMs: TIMEOUT_MS, resultsDir: OUT, tagSuffix: 'smoke' });
    const stats = checkRunDir(res.runDir, p);
    if (p.system === 'cortex') cortexToolCalls += stats.toolCalls;
    summary.push(`${p.suite}/${p.system}${p.editFormat ? `-${p.editFormat}` : ''}: ${stats.passed}/${LIMIT} passed, ${p.system === 'cortex' ? `${stats.usage} model calls, ${stats.toolCalls} tool calls` : 'endpoint used'}`);
  }
  // Same tasks for every system on a suite.
  for (const suite of ['polyglot', 'realistic']) {
    const sets = ['cortex', 'aider', 'mini'].map((s) => JSON.stringify(JSON.parse(fs.readFileSync(path.join(OUT, suite, s, MODEL, 'run-1', 'run.json'), 'utf8')).tasks));
    assert.ok(sets.every((x) => x === sets[0]), `${suite}: systems ran different task sets`);
  }
  assert.ok(cortexToolCalls > 0, 'CortexCode never executed a model tool call in the whole smoke');
  const { md, model } = await buildReport({ resultsDir: OUT });
  await writeText(path.join(OUT, 'REPORT.md'), md);
  for (const s of model.series) {
    assert.equal(s.inconsistent.length, 0, `${s.suite}/${s.system}: recorded outcomes differ from recomputation: ${s.inconsistent}`);
    assert.equal(s.rescore_errors.length, 0, `${s.suite}/${s.system}: ${s.rescore_errors}`);
  }
  console.log(summary.join('\n'));
  console.log(`smoke report: ${path.join(OUT, 'REPORT.md')}`);
  console.log('smoke verification passed');
} catch (error) {
  console.error(error);
  process.exitCode = 1;
}
