#!/usr/bin/env node
// Node 1.9 / N3: a complete results set exists for every suite on the benchmark model, every number
// in eval/results/REPORT.md is recomputed from the raw logs, and the committed report matches the
// recomputation byte for byte.
//
//   node eval/verify/full_report.mjs [--model <variant, default: cortex.dev.toml's model variant>] [--results DIR]
//
// Complete means: editbench in all three edit formats x 3 runs, polyglot and realistic for
// CortexCode, aider and mini-swe-agent; each run covers the full task set of the manifest (no
// --limit) and every task has a result that is not a harness error.
import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import { EDIT_FORMATS } from '../lib/driver.mjs';
import { installedPlugins } from '../lib/frame.mjs';
import { HIDDEN, RESULTS } from '../lib/paths.mjs';
import { buildReport } from '../report.mjs';
import { parseArgs, readJson } from '../lib/util.mjs';

const args = parseArgs(process.argv.slice(2));
// Default: the variant the product ships with (the installed model plugin's variant).
const devModel = installedPlugins().find((p) => p.category === 'model')?.config?.variant;
const model = String(args.model ?? devModel ?? 'qwen3.8-27b-m');
const resultsDir = args.results ? path.resolve(args.results) : RESULTS;

const required = [
  ...EDIT_FORMATS.flatMap((f) => [1, 2, 3].map((r) => ({ suite: 'editbench', system: `cortex-${f}`, run: r }))),
  ...['cortex', 'aider', 'mini'].flatMap((s) => ['polyglot', 'realistic'].map((suite) => ({ suite, system: s, run: 1 }))),
];

const problems = [];
try {
  const manifests = {};
  for (const suite of ['editbench', 'polyglot', 'realistic']) {
    const m = await readJson(path.join(HIDDEN, suite, 'tasks.json'), null);
    if (!m) problems.push(`${suite}: task set not prepared`);
    manifests[suite] = m ? m.tasks.map((t) => t.id) : [];
  }
  for (const req of required) {
    const dir = path.join(resultsDir, req.suite, req.system, model, `run-${req.run}`);
    const meta = await readJson(path.join(dir, 'run.json'), null);
    if (!meta) { problems.push(`missing run ${path.relative(resultsDir, dir)}`); continue; }
    const want = [...manifests[req.suite]].sort();
    const have = [...meta.tasks].sort();
    if (JSON.stringify(want) !== JSON.stringify(have)) problems.push(`${path.relative(resultsDir, dir)}: covers ${have.length} of ${want.length} manifest tasks`);
    let done = 0;
    for (const id of meta.tasks) {
      const r = await readJson(path.join(dir, 'tasks', id, 'result.json'), null);
      if (r && r.failure_class !== 'harness_error') done++;
    }
    if (done !== meta.tasks.length) problems.push(`${path.relative(resultsDir, dir)}: ${done}/${meta.tasks.length} tasks finished without harness error`);
    else console.log(`${path.relative(resultsDir, dir)}: ${done} tasks complete`);
  }
  if (problems.length) throw new Error(`results incomplete:\n- ${problems.join('\n- ')}`);

  const { md, model: report } = await buildReport({ resultsDir });
  for (const s of report.series) {
    assert.equal(s.inconsistent.length, 0, `${s.suite}/${s.system}/${s.model}: recorded outcomes differ from raw logs: ${s.inconsistent.slice(0, 10)}`);
    assert.equal(s.rescore_errors.length, 0, `${s.suite}/${s.system}/${s.model}: ${s.rescore_errors.slice(0, 10)}`);
  }
  const reportPath = path.join(resultsDir, 'REPORT.md');
  assert.ok(fs.existsSync(reportPath), `${reportPath} missing: run node eval/report.mjs`);
  assert.equal(fs.readFileSync(reportPath, 'utf8'), md, 'REPORT.md differs from the recomputation from raw logs: regenerate it with node eval/report.mjs');
  for (const s of report.series.filter((x) => x.model === model)) {
    console.log(`${s.suite}/${s.system}: ${s.pooled.k}/${s.pooled.n} (${(100 * s.pooled.p).toFixed(1)}%)`);
  }
  console.log('full report verification passed');
} catch (error) {
  console.error(error.message ?? error);
  process.exitCode = 1;
}
