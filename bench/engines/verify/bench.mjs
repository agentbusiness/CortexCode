#!/usr/bin/env node
// G2 check: every qualifying engine (survey verdict "candidate") has a complete full-suite run of
// Qwen3.8-27B on this machine (or an evidence-backed failure record), every reported number in
// results/speed.json and REPORT.md is recomputed here from the raw logs (own arithmetic, not the
// harness's), and prompts were never repeated within one engine process.
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const DIR = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const fails = [];
const check = (c, m) => { if (!c) fails.push(m); };
const jl = (f) => fs.readFileSync(f, 'utf8').split('\n').filter(Boolean).map((l) => JSON.parse(l));
const close = (a, b) => a === b || (Number.isFinite(a) && Number.isFinite(b) && Math.abs(a - b) <= 1e-3 * Math.max(1, Math.abs(b)));

const survey = JSON.parse(fs.readFileSync(path.join(DIR, 'survey.json'), 'utf8'));
const speed = JSON.parse(fs.readFileSync(path.join(DIR, 'results/speed.json'), 'utf8'));
const selection = JSON.parse(fs.readFileSync(path.join(DIR, 'results/selection.json'), 'utf8'));
const report = fs.readFileSync(path.join(DIR, 'REPORT.md'), 'utf8');
const corpusTokens = new Map();
for (const f of ['decode.jsonl', 'prefill2k.jsonl', 'prefill8k.jsonl']) for (const r of jl(path.join(DIR, 'corpus', f))) corpusTokens.set(r.id, r.n_prompt_tokens);
for (const s of jl(path.join(DIR, 'corpus', 'agent.jsonl'))) for (const t of s.turns) corpusTokens.set(`${s.id}/t${t.turn}`, t.n_prompt_tokens);

// ---- independent metric computation
const mean = (v) => v.reduce((a, b) => a + b, 0) / v.length;
function union(iv) {
  iv = iv.filter(([a, b]) => b > a).sort((x, y) => x[0] - y[0]);
  let tot = 0, s = null, e = null;
  for (const [a, b] of iv) { if (s === null) { s = a; e = b; } else if (a <= e) e = Math.max(e, b); else { tot += e - s; s = a; e = b; } }
  return s === null ? 0 : tot + e - s;
}
export function recompute(runDir) {
  const meta = JSON.parse(fs.readFileSync(path.join(runDir, 'meta.json'), 'utf8'));
  const req = jl(path.join(runDir, 'requests.jsonl'));
  const meas = jl(path.join(runDir, 'measurements.jsonl'));
  const bySeq = new Map(req.map((r) => [r.seq, r]));
  const g = {};
  for (const m of meas) {
    const rows = m.seqs.map((s) => bySeq.get(s));
    if (rows.some((r) => !r || r.error)) continue;
    (g[rows[0].kind] ??= []).push(rows);
  }
  const dec = (r) => (r.n_gen_tokens_retok - r.n_first_chunk_tokens_retok) / ((r.t_last - r.t_first) / 1000);
  const out = { meta, req };
  if (g['decode-c1']) out.decode_c1 = mean(g['decode-c1'].map((rows) => dec(rows[0])));
  for (const k of ['prefill2k', 'prefill8k']) if (g[k]) out[k] = mean(g[k].map(([r]) => corpusTokens.get(r.prompt_id) / ((r.t_first - r.t_send) / 1000)));
  for (const c of [2, 4]) if (g[`decode-c${c}`]) out[`decode_c${c}`] = mean(g[`decode-c${c}`].map((rows) => rows.reduce((a, r) => a + r.n_gen_tokens_retok, 0) / (union(rows.map((r) => [r.t_first, r.t_last])) / 1000)));
  for (const c of [1, 4]) {
    if (!g[`agent-c${c}`]) continue;
    const rows = g[`agent-c${c}`].flat();
    const warm = rows.filter((r) => r.turn > 0);
    out[`agent_c${c}_warm_turn_s`] = mean(warm.map((r) => (r.t_last - r.t_send) / 1000));
    out[`agent_c${c}_cold_turn_s`] = mean(rows.filter((r) => r.turn === 0).map((r) => (r.t_last - r.t_send) / 1000));
  }
  out.counts = Object.fromEntries(Object.entries(g).map(([k, v]) => [k, v.length]));
  return out;
}

// ---- checks
const quals = survey.engines.filter((e) => e.verdict === 'candidate');
check(quals.length >= 4, 'fewer than 4 qualifying engines in the survey');
const speedByDir = new Map(speed.runs.map((r) => [r.dir, r]));
const reportNumbers = [];
for (const e of quals) {
  const sel = selection.engines?.[e.id];
  if (!sel) { fails.push(`${e.id}: qualifying engine has no entry in results/selection.json`); continue; }
  if (sel.status === 'failed') {
    // an engine that could not run the 27B must show it with its own raw logs
    const ev = path.join(DIR, sel.failure_run || '');
    check(sel.failure_run && fs.existsSync(path.join(ev, 'meta.json')) && fs.existsSync(path.join(ev, 'server.log')), `${e.id}: failure claimed without raw evidence`);
    if (fs.existsSync(path.join(ev, 'meta.json'))) {
      const m = JSON.parse(fs.readFileSync(path.join(ev, 'meta.json'), 'utf8'));
      check(m.status === 'failed' && m.engine === e.id && !(m.launch?.args || []).join(' ').includes('4B'), `${e.id}: failure record is not a failed 27B run of this engine`);
    }
    check((sel.failure_reason || '').length > 20 && report.includes(sel.failure_reason.slice(0, 40)), `${e.id}: failure reason missing or not stated in REPORT.md`);
    continue;
  }
  const runRel = sel.full_run || sel.run;
  const runDir = path.join(DIR, runRel || '');
  if (!runRel || !fs.existsSync(path.join(runDir, 'meta.json'))) { fails.push(`${e.id}: selected run missing`); continue; }
  const rc = recompute(runDir);
  const m = rc.meta;
  const gaps = new Set((sel.gaps || []).map((g) => g.measurement));
  for (const g of sel.gaps || []) check((g.reason || '').length > 20 && report.includes(g.reason.slice(0, 40)), `${e.id}: gap ${g.measurement} without a reason stated in REPORT.md`);
  check(m.engine === e.id && ['full', 'sweep'].includes(m.suite) && m.status === 'complete', `${e.id}: selected run is not a complete measurement run`);
  check(m.suite === m.suite && (sel.full_run ? m.suite === 'full' : m.suite === 'sweep'), `${e.id}: selection suite mismatch`);
  check(!(m.launch.args || []).some((a) => /Qwen3\.5-4B|4B-M/.test(a)) && !/4B/.test(m.launch.env?.MODEL_ID || ''), `${e.id}: selected run did not load the 27B`);
  const need = m.suite === 'full' ? { 'decode-c1': 3, prefill2k: 3, prefill8k: 3, 'decode-c2': 3, 'decode-c4': 3, 'agent-c1': 1, 'agent-c4': 1 } : { 'decode-c1': 3, prefill2k: 2, prefill8k: 1, 'decode-c4': 1 };
  for (const [k, n] of Object.entries(need)) if (!gaps.has(k)) check((rc.counts[k] || 0) >= n, `${e.id}: too few ${k} measurements (${rc.counts[k] || 0} < ${n})`);
  check(m.load?.server_ready_s > 0 && m.load?.first_token_ready_s > 0, `${e.id}: load time missing`);
  check(m.memory?.peak_rss > 1e9 && m.memory?.peak_sys_delta > 1e9, `${e.id}: peak memory missing or implausibly small for a 27B`);
  if (sel.decision_engine) check(m.suite === 'full', `${e.id}: the chosen engine needs the full suite (agent turns at 1 and 4)`);
  // distinct prompts within the engine process
  const ids = rc.req.map((r) => r.prompt_id);
  check(new Set(ids).size === ids.length, `${e.id}: a prompt was sent twice to one engine process`);
  // the analyzed numbers equal the recomputation
  const a = speedByDir.get(runRel);
  if (!a) { fails.push(`${e.id}: selected run not in results/speed.json`); continue; }
  const M = a.measurements;
  const pairs = [['decode_c1', M.decode_c1?.tok_s?.mean], ['prefill2k', M.prefill2k?.tok_s?.mean], ['prefill8k', M.prefill8k?.tok_s?.mean], ['decode_c2', M.decode_c2?.aggregate_tok_s?.mean], ['decode_c4', M.decode_c4?.aggregate_tok_s?.mean], ['agent_c1_warm_turn_s', M.agent_c1?.warm_turn_s?.mean], ['agent_c4_warm_turn_s', M.agent_c4?.warm_turn_s?.mean], ['agent_c1_cold_turn_s', M.agent_c1?.cold_turn_s?.mean]];
  for (const [k, v] of pairs) {
    if (v === undefined && rc[k] === undefined) continue;
    check(close(v, rc[k]), `${e.id}: ${k} in speed.json (${v}) differs from the raw-log recomputation (${rc[k]})`);
    reportNumbers.push({ engine: e.id, k, v: rc[k] });
  }
}
// every number in the REPORT's main speed table is a recomputed one (rounded to one decimal)
for (const { engine, k, v } of reportNumbers) {
  if (!Number.isFinite(v)) continue;
  check(report.includes(v.toFixed(1)), `REPORT.md does not show ${engine} ${k} = ${v.toFixed(1)}`);
}
// negative control: shifting one decode timestamp must change the recomputed decode rate
{
  const sel = Object.values(selection.engines).find((s) => s.full_run || s.run);
  if (sel) {
    const tmp = fs.mkdtempSync(path.join(process.env.TMPDIR || '/tmp', 'bench-neg-'));
    for (const f of ['meta.json', 'requests.jsonl', 'measurements.jsonl']) fs.copyFileSync(path.join(DIR, sel.full_run || sel.run, f), path.join(tmp, f));
    const base = recompute(tmp).decode_c1;
    const rows = jl(path.join(tmp, 'requests.jsonl'));
    const r = rows.find((x) => x.kind === 'decode-c1');
    r.t_last += 5000;
    fs.writeFileSync(path.join(tmp, 'requests.jsonl'), rows.map((x) => JSON.stringify(x)).join('\n') + '\n');
    check(!close(recompute(tmp).decode_c1, base), 'negative control: a perturbed timestamp did not change the recomputed decode rate');
    fs.rmSync(tmp, { recursive: true, force: true });
  }
}

if (fails.length) { console.error('engine bench verification FAILED:\n- ' + fails.join('\n- ')); process.exit(1); }
console.log(`${quals.length} qualifying engines; ${reportNumbers.length} reported numbers recomputed from raw logs`);
console.log('engine bench verification passed');
