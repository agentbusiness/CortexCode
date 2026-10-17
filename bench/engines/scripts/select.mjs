#!/usr/bin/env node
// Writes results/selection.json: for each engine, the configuration chosen from its sweep (the
// choice and its reason are in selection-input.json) and the newest complete full-suite and
// quality runs of that configuration; or, for an engine that could not run the 27B, its failed run.
import fs from 'node:fs';
import path from 'node:path';
import { ENGINES_DIR } from '../harness/lib.mjs';
import { loadRuns } from '../harness/analyze.mjs';

const input = JSON.parse(fs.readFileSync(path.join(ENGINES_DIR, 'selection-input.json'), 'utf8'));
const runs = loadRuns();
const newest = (f) => runs.filter(f).sort((a, b) => b.dir.localeCompare(a.dir))[0]?.dir ?? null;
const out = { generated: new Date().toISOString(), engines: {} };
for (const [id, sel] of Object.entries(input.engines)) {
  if (sel.failed) {
    out.engines[id] = { status: 'failed', failure_reason: sel.failure_reason, failure_run: newest((r) => r.engine === id && r.meta.status === 'failed' && (!sel.failure_config || r.config === sel.failure_config) && !r.tag?.startsWith('smoke')) };
    continue;
  }
  const suite = sel.suite || 'full';
  const run = newest((r) => r.engine === id && r.config === sel.config && r.suite === suite && r.meta.status === 'complete' && !r.tag?.startsWith('smoke'));
  out.engines[id] = {
    status: 'measured', config: sel.config, suite, tuned: !!sel.tuned, reason: sel.reason, decision_engine: !!sel.decision_engine,
    ...(suite === 'full' ? { full_run: run } : { run }), gaps: sel.gaps || [],
  };
}
fs.writeFileSync(path.join(ENGINES_DIR, 'results', 'selection.json'), JSON.stringify(out, null, 1));
for (const [id, s] of Object.entries(out.engines)) console.log(id, s.status, s.full_run ?? s.run ?? s.failure_run);
