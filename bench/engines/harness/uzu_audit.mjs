#!/usr/bin/env node
// Functional audit of uzu's server against what the model plugin (model@1) needs. Each check runs
// a real request and records the observation; nothing is inferred from documentation here.
// node bench/engines/harness/uzu_audit.mjs [--bin <uzu cli>] [--model <id|dir>] [--tag name]
// Not a performance measurement.
import fs from 'node:fs';
import path from 'node:path';
import http from 'node:http';
import { spawn } from 'node:child_process';
import { ENGINES_DIR, startEngine, stopEngine, waitReady, streamChat, sleep, now } from './lib.mjs';

const argv = process.argv.slice(2);
const opt = (k, d) => { const i = argv.indexOf('--' + k); return i >= 0 ? argv[i + 1] : d; };
const BIN = opt('bin', path.join(ENGINES_DIR, 'uzu/release-0.6.7/mirai-aarch64-apple-darwin'));
const MODEL = opt('model', 'trymirai/Qwen3.5-4B-M');
const tag = opt('tag', 'm-0.6.7');
const out = path.join(ENGINES_DIR, 'raw', 'uzu-audit', `${tag}-${new Date().toISOString().replace(/[:.]/g, '-')}`);
fs.mkdirSync(out, { recursive: true });
const results = { bin: BIN, model: MODEL, started: new Date().toISOString(), checks: {} };
const save = () => fs.writeFileSync(path.join(out, 'results.json'), JSON.stringify(results, null, 1));
const record = (k, v) => { results.checks[k] = v; save(); console.log(k, JSON.stringify(v).slice(0, 300)); };
const tools = JSON.parse(fs.readFileSync(path.join(ENGINES_DIR, 'corpus/tools.json'), 'utf8')).tools;

async function withServer(extraArgs, wrap, fn) {
  const port = 19000 + Math.floor(Math.random() * 900);
  const base = `http://127.0.0.1:${port}`;
  let cmd = BIN, args = ['server', '--model', MODEL, '--host', '127.0.0.1', '--port', String(port), ...extraArgs];
  if (wrap) { args = [...wrap.slice(1), cmd, ...args]; cmd = wrap[0]; }
  const p = startEngine({ cmd, args, env: {}, logFile: path.join(out, `server-${port}.log`) });
  try { await waitReady(base, p, 600_000); return await fn(base); } finally { await stopEngine(p); }
}
function rawPost(base, body, abortAfterChunks) {
  // streams and aborts the connection after N content chunks (client-side cancellation)
  return new Promise((resolve) => {
    const u = new URL(base + '/v1/chat/completions');
    const data = Buffer.from(JSON.stringify({ ...body, stream: true }));
    let n = 0;
    const t0 = now();
    const req = http.request({ hostname: u.hostname, port: u.port, path: u.pathname, method: 'POST', headers: { 'content-type': 'application/json', 'content-length': data.length } }, (res) => {
      res.on('data', (d) => { n += (d.toString().match(/"content"/g) || []).length; if (n >= abortAfterChunks) { req.destroy(); resolve({ aborted_after_ms: now() - t0, chunks: n }); } });
      res.on('end', () => resolve({ ended: true, chunks: n }));
    });
    req.on('error', () => {});
    req.end(data);
  });
}
const user = (t) => [{ role: 'user', content: t }];

try {
  await withServer([], null, async (base) => {
    // 1. streaming deltas with reasoning separated from content
    const r1 = await streamChat(base, { messages: user('What is 17*23? Answer with the number.'), enable_thinking: true, reasoning_effort: 'low', max_tokens: 2000, temperature: 0 });
    record('reasoning_separation', { reasoning_chars: r1.reasoning.length, content: r1.content.slice(0, 200), reasoning_first: r1.chunks.findIndex((c) => c[2]) < r1.chunks.findIndex((c) => c[1]), ok: r1.reasoning.length > 0 && r1.content.length > 0 && !r1.content.includes('<think>'), error: r1.error });
    // 2. native tool calls with raw JSON arguments
    const r2 = await streamChat(base, { messages: user('Find where the function parse_config is defined in the project.'), tools, enable_thinking: false, max_tokens: 300, temperature: 0 });
    let parsed = null; try { parsed = JSON.parse(r2.tool_calls[0]?.arguments || 'x'); } catch {}
    record('tool_calls', { tool_calls: r2.tool_calls, finish_reason: r2.finish_reason, arguments_are_json_text: typeof r2.tool_calls[0]?.arguments === 'string' && parsed !== null, ok: r2.tool_calls.length > 0 && parsed !== null, error: r2.error });
    // 3. JSON-schema constrained output
    const schema = { type: 'object', properties: { verdict: { type: 'string', enum: ['pass', 'fail'] }, reasons: { type: 'array', items: { type: 'string' } } }, required: ['verdict', 'reasons'], additionalProperties: false };
    const r3 = await streamChat(base, { messages: user('Review: the function returns len(items) but must return the sum of quantities. Give a verdict.'), response_format: { type: 'json_schema', json_schema: { name: 'review', schema, strict: true } }, enable_thinking: false, max_tokens: 400, temperature: 0 });
    let obj = null; try { obj = JSON.parse(r3.content); } catch {}
    const valid = !!obj && ['pass', 'fail'].includes(obj.verdict) && Array.isArray(obj.reasons) && Object.keys(obj).every((k) => ['verdict', 'reasons'].includes(k));
    record('json_schema', { content: r3.content.slice(0, 400), parses: !!obj, valid_against_schema: valid, error: r3.error });
    const r3t = await streamChat(base, { messages: user('Review: the function returns len(items) but must return the sum of quantities. Give a verdict.'), response_format: { type: 'json_schema', json_schema: { name: 'review', schema, strict: true } }, enable_thinking: true, reasoning_effort: 'low', max_tokens: 4000, temperature: 0 });
    let objt = null; try { objt = JSON.parse(r3t.content); } catch {}
    record('json_schema_thinking_on', { content: r3t.content.slice(0, 300), reasoning_chars: r3t.reasoning.length, parses: !!objt, valid_against_schema: !!objt && ['pass', 'fail'].includes(objt.verdict) && Array.isArray(objt.reasons), error: r3t.error });
    const r3o = await streamChat(base, { messages: user('Return a JSON object with keys a and b, both numbers.'), response_format: { type: 'json_object' }, enable_thinking: false, max_tokens: 200, temperature: 0 });
    let objo = null; try { objo = JSON.parse(r3o.content); } catch {}
    record('json_object', { content: r3o.content.slice(0, 300), parses: !!objo, error: r3o.error });
    // 3b. GBNF grammar field (llama.cpp style) - accepted or ignored?
    const r3b = await streamChat(base, { messages: user('Say yes or no.'), grammar: 'root ::= "maybe"', enable_thinking: false, max_tokens: 20, temperature: 0 });
    record('gbnf_grammar', { content: r3b.content, honoured: r3b.content.trim() === 'maybe', error: r3b.error });
    // 4. cancellation mid-generation: abort the stream, then time a fresh short request
    const long = user('Write a detailed 2000-word essay about the history of compilers.');
    const ab = await rawPost(base, { messages: long, enable_thinking: false, max_tokens: 3000, temperature: 0 }, 20);
    const t0 = now();
    const r4 = await streamChat(base, { messages: user('Reply with the single word: ready'), enable_thinking: false, max_tokens: 10, temperature: 0 });
    record('cancellation', { abort: ab, next_request_ttft_ms: r4.t_first ? r4.t_first - t0 : null, next_content: r4.content, ok: r4.t_first !== null && r4.t_first - t0 < 5000, error: r4.error });
    // 5. prefix cache across turns, and between two interleaved conversations
    const convA = [{ role: 'system', content: 'You are a terse assistant. '.repeat(80) }, ...user('Name three prime numbers.')];
    const a1 = await streamChat(base, { messages: convA, enable_thinking: false, max_tokens: 30, temperature: 0, stream_options: { include_usage: true } });
    const convA2 = [...convA, { role: 'assistant', content: a1.content }, ...user('Now three more.')];
    const a2 = await streamChat(base, { messages: convA2, enable_thinking: false, max_tokens: 30, temperature: 0, stream_options: { include_usage: true } });
    const convB = [{ role: 'system', content: 'You are a verbose assistant. '.repeat(80) }, ...user('Name a color.')];
    await streamChat(base, { messages: convB, enable_thinking: false, max_tokens: 20, temperature: 0 });
    const convA3 = [...convA2, { role: 'assistant', content: a2.content }, ...user('And one even number.')];
    const a3 = await streamChat(base, { messages: convA3, enable_thinking: false, max_tokens: 20, temperature: 0, stream_options: { include_usage: true } });
    record('prefix_cache', { turn2_usage: a2.usage, turn2_ttft_ms: a2.t_first - a2.t_send, after_other_conversation_usage: a3.usage, after_other_ttft_ms: a3.t_first - a3.t_send, cached_turn2: a2.usage?.prompt_tokens_details?.cached_tokens ?? null, cached_after_interleave: a3.usage?.prompt_tokens_details?.cached_tokens ?? null });
    // 6. concurrent sequences: two requests at once, overlap of their generation intervals
    const [c1, c2] = await Promise.all([1, 2].map((i) => streamChat(base, { messages: user(`Count from 1 to 60, variant ${i}.`), enable_thinking: false, max_tokens: 150, temperature: 0 })));
    const overlap = Math.max(0, Math.min(c1.t_last, c2.t_last) - Math.max(c1.t_first, c2.t_first));
    record('concurrency', { a: [c1.t_first - c1.t_send, c1.t_last - c1.t_send], b: [c2.t_first - c2.t_send, c2.t_last - c2.t_send], overlap_ms: overlap, interleaved: overlap > 500 });
    // 7. sampling parameters: greedy determinism and temperature/seed effects
    const g = await Promise.all([0, 1].map(async () => (await streamChat(base, { messages: user('Write one sentence about rivers.'), enable_thinking: false, max_tokens: 40, temperature: 0 })).content));
    const s = [];
    for (const seed of [1, 2]) s.push((await streamChat(base, { messages: user('Write one sentence about rivers.'), enable_thinking: false, max_tokens: 40, temperature: 1.0, top_p: 0.95, top_k: 40, seed })).content);
    const s1b = (await streamChat(base, { messages: user('Write one sentence about rivers.'), enable_thinking: false, max_tokens: 40, temperature: 1.0, top_p: 0.95, top_k: 40, seed: 1 })).content;
    record('sampling', { greedy_identical: g[0] === g[1], t1_seed1_vs_seed2_differ: s[0] !== s[1], t1_seed1_reproducible: s[0] === s1b, samples: [g[0], s[0], s[1]].map((x) => x.slice(0, 80)) });
    // 8. thinking effort levels
    const eff = {};
    for (const e of ['low', 'high']) { const r = await streamChat(base, { messages: user('Is 391 prime? Answer yes or no.'), enable_thinking: true, reasoning_effort: e, max_tokens: 4000, temperature: 0 }); eff[e] = { reasoning_chars: r.reasoning.length, error: r.error }; }
    record('thinking_effort', eff);
  });
  // 9. offline: network denied except loopback (macOS sandbox profile), model already on disk
  const profile = '(version 1)(allow default)(deny network-outbound)(allow network-outbound (remote ip "localhost:*"))(allow network-outbound (remote unix-socket))';
  try {
    await withServer([], ['/usr/bin/sandbox-exec', '-p', profile], async (base) => {
      const r = await streamChat(base, { messages: user('Reply with the word offline.'), enable_thinking: false, max_tokens: 10, temperature: 0 });
      record('offline', { ok: !!r.content && !r.error, content: r.content, error: r.error });
    });
  } catch (e) { record('offline', { ok: false, error: String(e).slice(0, 400) }); }
} finally {
  results.finished = new Date().toISOString(); save();
}
console.log(out);
