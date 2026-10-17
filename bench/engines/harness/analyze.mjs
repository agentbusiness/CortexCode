#!/usr/bin/env node
// Derives every reported speed/memory number from the raw logs under raw/<engine>/<config>/<run>/
// and writes results/speed.json. Pure arithmetic over saved timestamps and token counts.
//
// Definitions (all times from the client's SSE arrival timestamps):
//   decode tok/s (one stream)  = (generated tokens - tokens in the first chunk) / (t_last - t_first)
//   prefill tok/s              = prompt tokens / (t_first - t_send)          (TTFT includes 1 decode step)
//   aggregate decode (N streams) = total generated tokens of the N streams / |union of [t_first_i, t_last_i]|
//   end-to-end throughput      = total generated tokens / (max t_last - min t_send)
//   agent turn time            = t_last - t_send of each turn; turn 0 is the cold 25-29k-token history
// Token counts: the generated text re-tokenized with Qwen3.8's tokenizer (same counter for every engine).
import fs from 'node:fs';
import path from 'node:path';
import { ENGINES_DIR, readJsonl } from './lib.mjs';

export function stats(xs) {
  const v = xs.filter((x) => Number.isFinite(x));
  const n = v.length;
  if (!n) return null;
  const mean = v.reduce((a, b) => a + b, 0) / n;
  const sd = n > 1 ? Math.sqrt(v.reduce((a, b) => a + (b - mean) ** 2, 0) / (n - 1)) : 0;
  // two-sided 95% Student t quantiles for n-1 degrees of freedom
  const T = [NaN, 12.706, 4.303, 3.182, 2.776, 2.571, 2.447, 2.365, 2.306, 2.262, 2.228, 2.201, 2.179, 2.160, 2.145, 2.131, 2.120, 2.110, 2.101, 2.093, 2.086];
  const t = n > 1 ? (T[n - 1] ?? 1.96) : NaN;
  const half = n > 1 ? (t * sd) / Math.sqrt(n) : null;
  const s = [...v].sort((a, b) => a - b);
  return { n, mean: r3(mean), sd: r3(sd), ci95: half === null ? null : [r3(mean - half), r3(mean + half)], min: r3(s[0]), max: r3(s[n - 1]), median: r3(n % 2 ? s[(n - 1) / 2] : (s[n / 2 - 1] + s[n / 2]) / 2) };
}
const r3 = (x) => Math.round(x * 1000) / 1000;

/** True when the generated text arrived over time (more than one distinct chunk arrival time). */
export function incremental(r) {
  // >= 3/4 of the generated chunks share the final arrival time: the body was flushed at the end
  const c = r.chunks || [];
  if (c.length < 4) return true;
  const last = c[c.length - 1][0];
  return c.filter((x) => last - x[0] < 50).length < 0.75 * c.length;
}
export function decodeRate(r) {
  if (!r.t_first || !r.t_last || r.t_last <= r.t_first || !incremental(r)) return null;
  return (r.n_gen_tokens_retok - r.n_first_chunk_tokens_retok) / ((r.t_last - r.t_first) / 1000);
}
export function prefillRate(r) {
  if (!r.t_first) return null;
  return r.corpus_prompt_tokens / ((r.t_first - r.t_send) / 1000);
}
export function unionLength(intervals) {
  const iv = intervals.filter(([a, b]) => b > a).sort((x, y) => x[0] - y[0]);
  let total = 0, cs = null, ce = null;
  for (const [a, b] of iv) {
    if (cs === null) { cs = a; ce = b; continue; }
    if (a <= ce) ce = Math.max(ce, b); else { total += ce - cs; cs = a; ce = b; }
  }
  if (cs !== null) total += ce - cs;
  return total;
}

/** All runs of all engines: meta + requests + measurements. */
export function loadRuns(root = path.join(ENGINES_DIR, 'raw')) {
  const runs = [];
  for (const engine of fs.readdirSync(root)) {
    const ed = path.join(root, engine);
    if (!fs.statSync(ed).isDirectory() || engine === 'downloads') continue;
    for (const config of fs.readdirSync(ed)) {
      const cd = path.join(ed, config);
      if (!fs.statSync(cd).isDirectory()) continue;
      for (const run of fs.readdirSync(cd)) {
        const rd = path.join(cd, run);
        const mf = path.join(rd, 'meta.json');
        if (!fs.existsSync(mf)) continue;
        const meta = JSON.parse(fs.readFileSync(mf, 'utf8'));
        const req = fs.existsSync(path.join(rd, 'requests.jsonl')) ? readJsonl(path.join(rd, 'requests.jsonl')) : [];
        const meas = fs.existsSync(path.join(rd, 'measurements.jsonl')) ? readJsonl(path.join(rd, 'measurements.jsonl')) : [];
        runs.push({ dir: path.relative(ENGINES_DIR, rd), engine: meta.engine, config: meta.config, tag: meta.tag, suite: meta.suite, meta, req, meas });
      }
    }
  }
  return runs;
}

export function analyzeRun(run) {
  const bySeq = new Map(run.req.map((r) => [r.seq, r]));
  const out = { dir: run.dir, engine: run.engine, config: run.config, suite: run.suite, status: run.meta.status, knobs: run.meta.knobs,
    load: run.meta.load ? { server_ready_s: r3(run.meta.load.server_ready_s), first_token_ready_s: r3(run.meta.load.first_token_ready_s) } : null,
    memory: run.meta.memory ?? null, errors: run.req.filter((r) => r.error).length, measurements: {} };
  const groups = {};
  for (const m of run.meas) {
    const rows = m.seqs.map((s) => bySeq.get(s)).filter(Boolean);
    if (!rows.length || rows.some((r) => r.error)) continue;
    const kind = rows[0].kind;
    (groups[kind] ??= []).push({ name: m.name, rows });
  }
  const M = out.measurements;
  // an engine whose SSE delivers the whole answer at once cannot have its decode or prefill
  // separated on the client: those numbers are left out and the run is flagged
  const gen = run.req.filter((r) => r.kind?.startsWith('decode') && !r.error);
  out.non_incremental_stream = gen.length > 0 && gen.filter((r) => !incremental(r)).length * 2 > gen.length;
  if (groups['decode-c1']) M.decode_c1 = { tok_s: stats(groups['decode-c1'].map((g) => decodeRate(g.rows[0]))), gen_tokens: groups['decode-c1'].map((g) => g.rows[0].n_gen_tokens_retok) };
  if (out.non_incremental_stream) {
    // whole-answer latency is still meaningful: request -> last byte
    M.non_incremental = { decode_request_s: stats((groups['decode-c1'] || []).map((g) => (g.rows[0].t_last - g.rows[0].t_send) / 1000)), prefill2k_request_s: stats((groups.prefill2k || []).map((g) => (g.rows[0].t_last - g.rows[0].t_send) / 1000)), prefill8k_request_s: stats((groups.prefill8k || []).map((g) => (g.rows[0].t_last - g.rows[0].t_send) / 1000)) };
    return out;
  }
  for (const k of ['prefill2k', 'prefill8k']) if (groups[k]) M[k] = { tok_s: stats(groups[k].map((g) => prefillRate(g.rows[0]))), ttft_s: stats(groups[k].map((g) => (g.rows[0].t_first - g.rows[0].t_send) / 1000)) };
  for (const c of [2, 4]) {
    const g = groups[`decode-c${c}`];
    if (!g) continue;
    const agg = g.map(({ rows }) => rows.reduce((a, r) => a + r.n_gen_tokens_retok, 0) / (unionLength(rows.map((r) => [r.t_first, r.t_last])) / 1000));
    const e2e = g.map(({ rows }) => rows.reduce((a, r) => a + r.n_gen_tokens_retok, 0) / ((Math.max(...rows.map((r) => r.t_last)) - Math.min(...rows.map((r) => r.t_send))) / 1000));
    const per = g.flatMap(({ rows }) => rows.map(decodeRate));
    M[`decode_c${c}`] = { aggregate_tok_s: stats(agg), end_to_end_tok_s: stats(e2e), per_stream_tok_s: stats(per) };
  }
  for (const c of [1, 4]) {
    const g = groups[`agent-c${c}`];
    if (!g) continue;
    const rows = g.flatMap((x) => x.rows);
    const cold = rows.filter((r) => r.turn === 0), warm = rows.filter((r) => r.turn > 0);
    const tt = (r) => (r.t_last - r.t_send) / 1000;
    M[`agent_c${c}`] = {
      cold_turn_s: stats(cold.map(tt)), cold_ttft_s: stats(cold.map((r) => (r.t_first - r.t_send) / 1000)),
      warm_turn_s: stats(warm.map(tt)), warm_ttft_s: stats(warm.map((r) => (r.t_first - r.t_send) / 1000)),
      warm_appended_tokens: stats(warm.map((r) => r.appended_tokens)), warm_gen_tokens: stats(warm.map((r) => r.n_gen_tokens_retok)),
      warm_cached_tokens: stats(warm.map((r) => r.usage?.prompt_tokens_details?.cached_tokens ?? null)),
      warm_decode_tok_s: stats(warm.map(decodeRate)),
      session_wall_s: r3((Math.max(...rows.map((r) => r.t_last)) - Math.min(...rows.map((r) => r.t_send))) / 1000),
    };
  }
  return out;
}

if (import.meta.url === `file://${process.argv[1]}`) {
  const runs = loadRuns().filter((r) => r.meta.status === 'complete' && ['sweep', 'full'].includes(r.suite) && !r.tag?.startsWith('smoke'));
  const results = runs.map(analyzeRun);
  fs.mkdirSync(path.join(ENGINES_DIR, 'results'), { recursive: true });
  fs.writeFileSync(path.join(ENGINES_DIR, 'results', 'speed.json'), JSON.stringify({ generated: new Date().toISOString(), runs: results }, null, 1));
  for (const r of results) {
    const M = r.measurements;
    console.log([r.engine, r.config, r.suite, M.decode_c1?.tok_s?.mean, M.prefill2k?.tok_s?.mean, M.prefill8k?.tok_s?.mean, M.decode_c4?.aggregate_tok_s?.mean, M.agent_c1?.warm_turn_s?.mean].join('\t'));
  }
}
