#!/usr/bin/env node
// uzu speculation behaviour by kind of text, from the raw logs of its runs. uzu reports
// usage.spec_verify_ct = prefill passes (1) + decode forward passes (crates/legacy/nagare/src/chat/
// token.rs:462-466), so tokens per decode pass = completion_tokens / (spec_verify_ct - 1); without
// a speculator it is 1. The decode prompts cycle through six task kinds (corpus/make_corpus.py
// DECODE_TASKS, index mod 6); agent turns produce edit_file tool calls.
import fs from 'node:fs';
import path from 'node:path';
import { ENGINES_DIR } from '../harness/lib.mjs';
import { loadRuns, stats, decodeRate } from '../harness/analyze.mjs';

const KIND = ['rewrite with docs (copies input)', 'write unit tests (new code)', 'refactor (copies input)', 'port to Go (new code)', 'explain + list bugs (prose)', 'add logging (copies input)'];
const runs = loadRuns().filter((r) => r.engine === 'uzu' && r.meta.status === 'complete' && ['full', 'sweep'].includes(r.suite) && !r.tag?.startsWith('smoke'));
const out = { generated: new Date().toISOString(), runs: [] };
for (const run of runs) {
  const rows = run.req.filter((r) => !r.error && r.usage?.spec_verify_ct > 1 && r.kind !== 'warmup');
  const by = {};
  for (const r of rows) {
    let k;
    if (r.kind.startsWith('decode')) k = KIND[Number(r.prompt_id.split('-')[1]) % 6];
    else if (r.kind.startsWith('agent')) k = 'agent turn (edit_file tool call)';
    else continue;
    (by[k] ??= []).push({ per_pass: r.usage.completion_tokens / (r.usage.spec_verify_ct - 1), rate: r.kind === 'decode-c1' || r.kind.startsWith('agent') ? decodeRate(r) : null });
  }
  out.runs.push({ dir: run.dir, config: run.config, suite: run.suite, kinds: Object.fromEntries(Object.entries(by).map(([k, v]) => [k, { tokens_per_decode_pass: stats(v.map((x) => x.per_pass)), decode_tok_s: stats(v.map((x) => x.rate).filter((x) => x !== null)) }])) });
}
fs.writeFileSync(path.join(ENGINES_DIR, 'results/uzu-acceptance.json'), JSON.stringify(out, null, 1));
for (const r of out.runs) { console.log(r.dir); for (const [k, v] of Object.entries(r.kinds)) console.log(`  ${k}: ${v.tokens_per_decode_pass?.mean} tok/pass (n=${v.tokens_per_decode_pass?.n}), decode ${v.decode_tok_s?.mean ?? '-'} tok/s`); }
