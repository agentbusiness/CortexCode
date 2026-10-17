#!/usr/bin/env node
// Output-quality parity (results/quality.json): every engine's greedy output on the fixed quality
// set is compared with the reference run (llama.cpp, Q8_0 GGUF, greedy).
//   code continuation (50 held-out prompts x 128 tokens): token ids of each output (Qwen3.8
//     tokenizer); agreement = length of the common token prefix with the reference output,
//     and the share of prompts identical over the first 32 / 64 / 128 tokens.
//   tool calls (40 agentic prompts): a call is VALID when it parses (engine-native tool_calls
//     JSON, else Qwen3.8's XML) into a known function with its required parameters and no
//     unknown ones; it MATCHES when name and every argument equal the reference call.
// Parsing is structural (JSON / the template's XML grammar); no judgement of meaning.
import fs from 'node:fs';
import path from 'node:path';
import { ENGINES_DIR, TokCounter, parseToolCallsXml } from './lib.mjs';
import { loadRuns, stats } from './analyze.mjs';

export const REFERENCE = { engine: 'llamacpp', config: 'ref-q8' };

export function toolCallsOf(r) {
  if (r.tool_calls?.length) {
    return r.tool_calls.map((tc) => {
      let args = null;
      try { args = typeof tc.arguments === 'string' ? JSON.parse(tc.arguments || '{}') : tc.arguments; } catch { args = null; }
      return { name: tc.name, arguments: args };
    });
  }
  return parseToolCallsXml(r.content || '').map((c) => ({ name: c.name, arguments: c.arguments }));
}
export function validCall(call, tools) {
  if (!call || !call.arguments || typeof call.arguments !== 'object') return false;
  const t = tools.find((x) => x.function.name === call.name);
  if (!t) return false;
  const props = t.function.parameters.properties, req = t.function.parameters.required || [];
  if (!req.every((k) => k in call.arguments)) return false;
  return Object.keys(call.arguments).every((k) => k in props);
}
const norm = (v) => (typeof v === 'string' ? v.trim() : JSON.stringify(v));
export function sameCall(a, b) {
  if (!a || !b || a.name !== b.name || !a.arguments || !b.arguments) return false;
  const ka = Object.keys(a.arguments).sort(), kb = Object.keys(b.arguments).sort();
  return ka.length === kb.length && ka.every((k, i) => k === kb[i] && norm(a.arguments[k]) === norm(b.arguments[k]));
}
export function commonPrefix(a, b) { let i = 0; while (i < a.length && i < b.length && a[i] === b[i]) i++; return i; }

export async function analyzeQuality() {
  const tools = JSON.parse(fs.readFileSync(path.join(ENGINES_DIR, 'corpus', 'tools.json'), 'utf8')).tools;
  const runs = loadRuns().filter((r) => r.suite === 'quality' && r.meta.status === 'complete');
  const latest = new Map();
  for (const r of runs) { const k = `${r.engine}/${r.config}`; if (!latest.has(k) || latest.get(k).dir < r.dir) latest.set(k, r); }
  const ref = latest.get(`${REFERENCE.engine}/${REFERENCE.config}`);
  if (!ref) throw new Error('no reference quality run (llamacpp/ref-q8)');
  const tok = new TokCounter();
  const ids = async (text) => (await tok.count([text], true)).ids[0];
  const pick = (run, kind) => new Map(run.req.filter((x) => x.kind === kind && !x.error).map((x) => [x.prompt_id, x]));
  const refCode = pick(ref, 'qcode'), refTool = pick(ref, 'qtool');
  const refIds = new Map();
  for (const [k, v] of refCode) refIds.set(k, (await ids(v.reasoning + v.content)).slice(0, 128));
  const results = [];
  for (const run of latest.values()) {
    const code = pick(run, 'qcode'), tool = pick(run, 'qtool');
    const per = [];
    for (const [pid, rr] of refIds) {
      const x = code.get(pid);
      if (!x) { per.push({ prompt_id: pid, missing: true }); continue; }
      const xi = (await ids(x.reasoning + x.content)).slice(0, 128);
      const n = Math.min(128, rr.length);
      per.push({ prompt_id: pid, prefix: commonPrefix(xi, rr), ref_len: rr.length, len: xi.length, same32: commonPrefix(xi, rr) >= Math.min(32, n), same64: commonPrefix(xi, rr) >= Math.min(64, n), same128: commonPrefix(xi, rr) >= n });
    }
    const ok = per.filter((p) => !p.missing);
    const tper = [];
    for (const [pid, rr] of refTool) {
      const x = tool.get(pid);
      const rc = toolCallsOf(rr)[0] || null;
      if (!x) { tper.push({ prompt_id: pid, missing: true }); continue; }
      const xc = toolCallsOf(x);
      tper.push({ prompt_id: pid, n_calls: xc.length, valid: xc.length > 0 && xc.every((c) => validCall(c, tools)), name_match: !!rc && xc[0]?.name === rc.name, match: !!rc && sameCall(xc[0], rc), ref_valid: !!rc && validCall(rc, tools) });
    }
    const tok_ = tper.filter((p) => !p.missing);
    results.push({
      engine: run.engine, config: run.config, dir: run.dir, is_reference: run === ref,
      code: { n: ok.length, missing: per.length - ok.length, prefix_tokens: stats(ok.map((p) => p.prefix)), identical_32: ok.filter((p) => p.same32).length, identical_64: ok.filter((p) => p.same64).length, identical_128: ok.filter((p) => p.same128).length, per_prompt: per },
      tools: { n: tok_.length, missing: tper.length - tok_.length, valid: tok_.filter((p) => p.valid).length, name_match: tok_.filter((p) => p.name_match).length, match: tok_.filter((p) => p.match).length, ref_valid: tok_.filter((p) => p.ref_valid).length, per_prompt: tper },
    });
  }
  tok.close();
  return results;
}

if (import.meta.url === `file://${process.argv[1]}`) {
  const results = await analyzeQuality();
  fs.mkdirSync(path.join(ENGINES_DIR, 'results'), { recursive: true });
  fs.writeFileSync(path.join(ENGINES_DIR, 'results', 'quality.json'), JSON.stringify({ generated: new Date().toISOString(), reference: REFERENCE, results }, null, 1));
  for (const r of results) console.log([r.engine, r.config, `prefix ${r.code.prefix_tokens?.mean}`, `id32 ${r.code.identical_32}/${r.code.n}`, `tools valid ${r.tools.valid}/${r.tools.n} match ${r.tools.match}`].join('\t'));
}
