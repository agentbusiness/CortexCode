#!/usr/bin/env node
// Renders the measured tables of REPORT.md from results/*.json and replaces the block between
// <!-- generated:begin --> and <!-- generated:end --> in REPORT.md (the narrative around it is
// written by hand). Every number here comes from results/speed.json / quality.json, which
// harness/analyze.mjs and harness/quality.mjs derive from the raw logs.
import fs from 'node:fs';
import path from 'node:path';
import { ENGINES_DIR } from '../harness/lib.mjs';

const J = (f) => JSON.parse(fs.readFileSync(path.join(ENGINES_DIR, f), 'utf8'));
const speed = J('results/speed.json'), selection = J('results/selection.json');
const quality = fs.existsSync(path.join(ENGINES_DIR, 'results/quality.json')) ? J('results/quality.json') : { results: [] };
const survey = J('survey.json');
const name = (id) => survey.engines.find((e) => e.id === id)?.name ?? id;
const f1 = (x) => (Number.isFinite(x) ? x.toFixed(1) : '–');
const ci = (s) => (s?.ci95 ? ` (${s.ci95[0].toFixed(1)}–${s.ci95[1].toFixed(1)}, n=${s.n})` : s ? ` (n=${s.n})` : '');
const gb = (b) => (b ? (b / 2 ** 30).toFixed(1) : '–');
const byDir = new Map(speed.runs.map((r) => [r.dir, r]));

let md = '';
md += `### Main comparison (each engine at its best measured configuration, full suite)\n\n`;
md += `Mean with 95% t-interval over repeated runs (distinct prompts). Decode: one stream, 256 tokens. Prefill: 2k / 8k-token prompts, TTFT-based. Aggregate decode: N concurrent streams. Agent turn: append 1–3k tokens of tool output to a 25–43k-token history and generate a tool call (cap 300 tokens); cold = first turn (whole history).\n\n`;
md += `| Engine | config | decode tok/s | prefill 2k tok/s | prefill 8k tok/s | 2 streams agg tok/s | 4 streams agg tok/s | agent turn 1 agent (s) | agent turn 4 agents (s) | cold 27k turn (s) | load (s) | peak RSS GB | peak sys GB |\n|---|---|---|---|---|---|---|---|---|---|---|---|---|\n`;
for (const [id, sel] of Object.entries(selection.engines)) {
  if (sel.status === 'failed') { md += `| ${name(id)} | – | did not run: ${sel.failure_reason} |||||||||||\n`; continue; }
  const r = byDir.get(sel.full_run);
  if (!r) continue;
  const M = r.measurements;
  md += `| ${name(id)} | \`${r.config}\` | ${f1(M.decode_c1?.tok_s?.mean)}${ci(M.decode_c1?.tok_s)} | ${f1(M.prefill2k?.tok_s?.mean)}${ci(M.prefill2k?.tok_s)} | ${f1(M.prefill8k?.tok_s?.mean)}${ci(M.prefill8k?.tok_s)} | ${f1(M.decode_c2?.aggregate_tok_s?.mean)}${ci(M.decode_c2?.aggregate_tok_s)} | ${f1(M.decode_c4?.aggregate_tok_s?.mean)}${ci(M.decode_c4?.aggregate_tok_s)} | ${f1(M.agent_c1?.warm_turn_s?.mean)}${ci(M.agent_c1?.warm_turn_s)} | ${f1(M.agent_c4?.warm_turn_s?.mean)}${ci(M.agent_c4?.warm_turn_s)} | ${f1(M.agent_c1?.cold_turn_s?.mean)} | ${f1(r.load?.first_token_ready_s)} | ${gb(r.memory?.peak_rss)} | ${gb(r.memory?.peak_sys_delta)} |\n`;
}
md += `\nRaw runs: ${Object.values(selection.engines).filter((s) => s.full_run).map((s) => `\`${s.full_run}\``).join(', ')}.\n`;

md += `\n### Configuration sweeps (sweep suite: 3 decode, 2 prefill-2k, 1 prefill-8k, one 4-stream run; agent configs add one 1-agent session)\n`;
const engines = [...new Set(speed.runs.map((r) => r.engine))];
for (const id of engines) {
  const runs = speed.runs.filter((r) => r.engine === id && r.suite === 'sweep').sort((a, b) => a.config.localeCompare(b.config));
  if (!runs.length) continue;
  md += `\n**${name(id)}**\n\n| config | knobs | decode tok/s | prefill 2k | prefill 8k | 4-stream agg | agent warm turn (s) | load (s) | peak RSS GB | run |\n|---|---|---|---|---|---|---|---|---|---|\n`;
  for (const r of runs) {
    const M = r.measurements;
    const knobs = JSON.stringify(r.knobs).replace(/\|/g, '/').slice(0, 120);
    md += `| \`${r.config}\` | \`${knobs}\` | ${f1(M.decode_c1?.tok_s?.mean)} | ${f1(M.prefill2k?.tok_s?.mean)} | ${f1(M.prefill8k?.tok_s?.mean)} | ${f1(M.decode_c4?.aggregate_tok_s?.mean)} | ${f1(M.agent_c1?.warm_turn_s?.mean)} | ${f1(r.load?.first_token_ready_s)} | ${gb(r.memory?.peak_rss)} | \`${r.dir}\` |\n`;
  }
}

md += `\n### Output quality parity (greedy; reference = llama.cpp with the Q8_0 GGUF)\n\n| Engine / config | code: mean common-prefix tokens (of 128) | identical first 32 tokens | identical 64 | identical 128 | tool calls valid | same call as reference | run |\n|---|---|---|---|---|---|---|---|\n`;
for (const q of quality.results) {
  md += `| ${name(q.engine)} / \`${q.config}\`${q.is_reference ? ' (reference)' : ''} | ${f1(q.code.prefix_tokens?.mean)}${ci(q.code.prefix_tokens)} | ${q.code.identical_32}/${q.code.n} | ${q.code.identical_64}/${q.code.n} | ${q.code.identical_128}/${q.code.n} | ${q.tools.valid}/${q.tools.n} | ${q.tools.match}/${q.tools.n} | \`${q.dir}\` |\n`;
}

const rp = path.join(ENGINES_DIR, 'REPORT.md');
const cur = fs.existsSync(rp) ? fs.readFileSync(rp, 'utf8') : '<!-- generated:begin -->\n<!-- generated:end -->\n';
const a = cur.indexOf('<!-- generated:begin -->'), b = cur.indexOf('<!-- generated:end -->');
if (a < 0 || b < 0) throw new Error('REPORT.md lacks the generated markers');
fs.writeFileSync(rp, cur.slice(0, a) + '<!-- generated:begin -->\n' + md + cur.slice(b));
console.log('REPORT.md tables regenerated');
