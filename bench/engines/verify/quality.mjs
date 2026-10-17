#!/usr/bin/env node
// G3 check: the reference (llama.cpp, Q8_0, greedy) and every measured qualifying engine ran the
// same fixed quality set (50 held-out code prompts, 40 tool-call prompts); the agreement and
// tool-call numbers in results/quality.json are recomputed here from the raw outputs with the
// model's tokenizer; the parser/validator rejects malformed calls (negative controls).
import fs from 'node:fs';
import path from 'node:path';
import { spawnSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';

const DIR = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const fails = [];
const check = (c, m) => { if (!c) fails.push(m); };
const jl = (f) => fs.readFileSync(f, 'utf8').split('\n').filter(Boolean).map((l) => JSON.parse(l));
const quality = JSON.parse(fs.readFileSync(path.join(DIR, 'results/quality.json'), 'utf8'));
const selection = JSON.parse(fs.readFileSync(path.join(DIR, 'results/selection.json'), 'utf8'));
const survey = JSON.parse(fs.readFileSync(path.join(DIR, 'survey.json'), 'utf8'));
const report = fs.readFileSync(path.join(DIR, 'REPORT.md'), 'utf8');
const tools = JSON.parse(fs.readFileSync(path.join(DIR, 'corpus/tools.json'), 'utf8')).tools;
const QC = jl(path.join(DIR, 'corpus/quality_code.jsonl')), QT = jl(path.join(DIR, 'corpus/quality_tools.jsonl'));
const TOKENIZER = path.join(process.env.HOME, '.cortexcode/models/trymirai-Qwen3.8-27B-M/8b878f57094a616c0ea4c214fe774af0cbfb4353/tokenizer.json');

// ---- independent structural tool-call parsing (Qwen3.8 XML grammar or engine JSON)
function xmlCalls(text) {
  const out = [];
  for (const m of text.matchAll(/<tool_call>\s*<function=([^>\n]+)>([\s\S]*?)<\/function>\s*<\/tool_call>/g)) {
    const args = {};
    for (const p of m[2].matchAll(/<parameter=([^>\n]+)>\n?([\s\S]*?)\n?<\/parameter>/g)) args[p[1].trim()] = p[2];
    out.push({ name: m[1].trim(), arguments: args });
  }
  return out;
}
function calls(r) {
  if (r.tool_calls?.length) return r.tool_calls.map((t) => { try { return { name: t.name, arguments: typeof t.arguments === 'string' ? JSON.parse(t.arguments || '{}') : t.arguments }; } catch { return { name: t.name, arguments: null }; } });
  return xmlCalls(r.content || '');
}
function valid(c) {
  if (!c?.arguments || typeof c.arguments !== 'object') return false;
  const t = tools.find((x) => x.function.name === c.name);
  if (!t) return false;
  const p = t.function.parameters;
  return (p.required || []).every((k) => k in c.arguments) && Object.keys(c.arguments).every((k) => k in p.properties);
}
const nv = (v) => (typeof v === 'string' ? v.trim() : JSON.stringify(v));
function same(a, b) { if (!a || !b || a.name !== b.name || !a.arguments || !b.arguments) return false; const ka = Object.keys(a.arguments).sort(), kb = Object.keys(b.arguments).sort(); return ka.join() === kb.join() && ka.every((k) => nv(a.arguments[k]) === nv(b.arguments[k])); }

// ---- tokenization (one batch through the model's tokenizer)
function tokenizeAll(texts) {
  const py = 'import json,sys\nfrom tokenizers import Tokenizer\nt=Tokenizer.from_file(sys.argv[1])\nx=json.load(sys.stdin)\nprint(json.dumps([e.ids for e in t.encode_batch(x, add_special_tokens=False)]))';
  const r = spawnSync('uv', ['run', '-q', '--with', 'tokenizers', 'python', '-I', '-c', py, TOKENIZER], { input: JSON.stringify(texts), encoding: 'utf8', maxBuffer: 1 << 28 });
  if (r.status !== 0) throw new Error(r.stderr);
  return JSON.parse(r.stdout);
}
const prefix = (a, b) => { let i = 0; while (i < a.length && i < b.length && a[i] === b[i]) i++; return i; };

function load(dir) {
  const meta = JSON.parse(fs.readFileSync(path.join(DIR, dir, 'meta.json'), 'utf8'));
  const req = jl(path.join(DIR, dir, 'requests.jsonl')).filter((r) => !r.error);
  const meas = jl(path.join(DIR, dir, 'measurements.jsonl'));
  return { meta, req, meas, code: new Map(req.filter((r) => r.kind === 'qcode').map((r) => [r.prompt_id, r])), tool: new Map(req.filter((r) => r.kind === 'qtool').map((r) => [r.prompt_id, r])) };
}

const ref = quality.results.find((r) => r.is_reference);
check(ref && ref.engine === 'llamacpp' && ref.config === 'ref-q8', 'no llama.cpp Q8_0 reference run');
const refRun = ref ? load(ref.dir) : null;
if (refRun) {
  check(/Q8_0\.gguf/.test(refRun.meta.launch.args.join(' ')), 'reference run did not load the Q8_0 GGUF');
  check(refRun.code.size === QC.length && refRun.tool.size === QT.length, `reference run incomplete (${refRun.code.size}/${QC.length} code, ${refRun.tool.size}/${QT.length} tool)`);
}
// every qualifying engine that produced speed numbers has a quality run on the same set
const measured = Object.entries(selection.engines).filter(([, s]) => s.status !== 'failed').map(([id, s]) => ({ id, s }));
const quals = new Set(survey.engines.filter((e) => e.verdict === 'candidate').map((e) => e.id));
for (const { id, s } of measured) {
  if (!quals.has(id)) continue;
  const q = quality.results.find((r) => r.dir === s.quality_run);
  check(q && q.engine === id, `${id}: no quality run recorded (results/selection.json quality_run)`);
}
// recompute every engine's numbers
const texts = [];
const plan = [];
for (const q of quality.results) {
  const run = load(q.dir);
  check(run.meta.status === 'complete' && run.meta.suite === 'quality', `${q.engine}/${q.config}: quality run not complete`);
  plan.push({ q, run });
  for (const it of QC) { const r = run.code.get(it.id); if (r) texts.push(r.reasoning + r.content); }
}
const refTexts = refRun ? QC.map((it) => { const r = refRun.code.get(it.id); return r ? r.reasoning + r.content : null; }) : [];
const allIds = tokenizeAll([...texts, ...refTexts.filter((t) => t !== null)]);
let k = 0;
const idsOf = [];
for (const { run } of plan) { const m = new Map(); for (const it of QC) if (run.code.get(it.id)) m.set(it.id, allIds[k++].slice(0, 128)); idsOf.push(m); }
const refIds = new Map(); for (const it of QC) if (refRun?.code.get(it.id)) refIds.set(it.id, allIds[k++].slice(0, 128));
plan.forEach(({ q, run }, i) => {
  const pre = [], id32 = [];
  for (const it of QC) {
    const a = idsOf[i].get(it.id), b = refIds.get(it.id);
    if (!a || !b) continue;
    const p = prefix(a, b);
    pre.push(p); id32.push(p >= Math.min(32, b.length));
  }
  const meanPre = pre.reduce((x, y) => x + y, 0) / pre.length;
  check(pre.length === q.code.n && Math.abs(meanPre - q.code.prefix_tokens.mean) < 1e-2, `${q.engine}/${q.config}: prefix agreement ${q.code.prefix_tokens?.mean} != recomputed ${meanPre.toFixed(3)}`);
  check(id32.filter(Boolean).length === q.code.identical_32, `${q.engine}/${q.config}: identical-32 count differs`);
  let v = 0, mt = 0, n = 0;
  for (const it of QT) {
    const r = run.tool.get(it.id), rr = refRun?.tool.get(it.id);
    if (!r) continue;
    n++;
    const c = calls(r);
    if (c.length && c.every(valid)) v++;
    const rc = rr ? calls(rr)[0] : null;
    if (rc && same(c[0], rc)) mt++;
  }
  check(n === q.tools.n && v === q.tools.valid && mt === q.tools.match, `${q.engine}/${q.config}: tool numbers differ (valid ${v}/${n} vs ${q.tools.valid}/${q.tools.n}, match ${mt} vs ${q.tools.match})`);
  if (q.is_reference) check(pre.every((p, j) => p === Math.min(128, [...refIds.values()][j].length)), 'reference does not agree with itself');
  check(report.includes(`${q.code.identical_32}/${q.code.n}`) && report.includes(`${q.tools.valid}/${q.tools.n}`), `REPORT.md lacks ${q.engine}/${q.config} quality numbers`);
});
// negative controls
check(!valid({ name: 'read_file', arguments: { path: 'x', bogus: 1 } }), 'negative control: unknown parameter accepted');
check(!valid({ name: 'write_file', arguments: { path: 'x' } }), 'negative control: missing required parameter accepted');
check(!valid({ name: 'delete_everything', arguments: {} }), 'negative control: unknown function accepted');
check(xmlCalls('<tool_call>\n<function=search>\n<parameter=pattern>\nfoo\n</parameter>\n</function>\n</tool_call>')[0]?.arguments.pattern === 'foo', 'positive control: XML parser failed on a well-formed call');
check(prefix([1, 2, 3, 4], [1, 2, 9, 4]) === 2, 'negative control: prefix agreement ignores a divergence');

if (fails.length) { console.error('engine quality verification FAILED:\n- ' + fails.join('\n- ')); process.exit(1); }
console.log(`${quality.results.length} quality runs recomputed against the Q8_0 reference`);
console.log('engine quality verification passed');
