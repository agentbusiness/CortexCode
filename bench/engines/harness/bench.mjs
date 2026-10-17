#!/usr/bin/env node
// Bake-off driver: launches one engine configuration, runs a suite against its streaming
// OpenAI-compatible chat endpoint, and saves raw logs. Numbers are derived later from the raw
// logs (harness/analyze.mjs) and independently re-derived by verify/*.mjs.
//
// node bench/engines/harness/bench.mjs --engine <id> --config <name> --suite <sweep|full|quality|smoke>
//      [--model-override <path>] [--tag <label>]
//
// Owner rule implemented here: distinct prompts (every request in one engine process uses a
// different corpus prompt).
import fs from 'node:fs';
import path from 'node:path';
import os from 'node:os';
import { execFileSync } from 'node:child_process';
import {
  ENGINES_DIR, MemSampler,
  startEngine, stopEngine, killGroup, waitReady, streamChat, generatedText, TokCounter, readJsonl, now, sleep, toWire,
} from './lib.mjs';
import { ENGINES } from './engines.mjs';

const argv = process.argv.slice(2);
const opt = (k, d) => { const i = argv.indexOf('--' + k); return i >= 0 ? argv[i + 1] : d; };
const engineId = opt('engine');
const configName = opt('config', 'default');
const suite = opt('suite', 'sweep');
const tag = opt('tag', '');
const eng = ENGINES[engineId];
if (!eng) { console.error(`unknown engine ${engineId}; known: ${Object.keys(ENGINES).join(', ')}`); process.exit(2); }
const cfg = eng.configs[configName];
if (!cfg) { console.error(`unknown config ${configName}; known: ${Object.keys(eng.configs).join(', ')}`); process.exit(2); }
const smoke = suite === 'smoke';

const corpus = (n) => readJsonl(path.join(ENGINES_DIR, 'corpus', n));
const D = corpus('decode.jsonl'), P2 = corpus('prefill2k.jsonl'), P8 = corpus('prefill8k.jsonl'), A = corpus('agent.jsonl');
const QC = corpus('quality_code.jsonl'), QT = corpus('quality_tools.jsonl');

const stamp = new Date().toISOString().replace(/[:.]/g, '-');
const outDir = path.join(ENGINES_DIR, 'raw', engineId, `${configName}${tag ? '-' + tag : ''}`, `${suite}-${stamp}`);
fs.mkdirSync(outDir, { recursive: true });
const log = (...a) => { const s = `[${new Date().toISOString()}] ${a.join(' ')}`; console.log(s); fs.appendFileSync(path.join(outDir, 'driver.log'), s + '\n'); };
const reqFile = path.join(outDir, 'requests.jsonl');
const meta = { engine: engineId, config: configName, suite, tag, started: new Date().toISOString(), host: os.hostname(), knobs: cfg, status: 'running' };
const saveMeta = () => fs.writeFileSync(path.join(outDir, 'meta.json'), JSON.stringify(meta, null, 1));

let engineProc = null, mem = null, memoryTrip = null;
async function cleanup(status) {
  if (mem) meta.memory = mem.stop();
  await stopEngine(engineProc);
  meta.status = status; meta.finished = new Date().toISOString(); saveMeta();
}
for (const s of ['SIGINT', 'SIGTERM']) process.on(s, async () => { await cleanup('interrupted'); process.exit(130); });

const port = 18000 + Math.floor(Math.random() * 2000);
const base = `http://127.0.0.1:${port}`;
const launch = eng.launch(cfg, port, { modelOverride: opt('model-override') });
meta.launch = { cmd: launch.cmd, args: launch.args, env: launch.env || {}, cwd: launch.cwd };
meta.engine_info = eng.info?.() ?? null;
saveMeta();

const tok = new TokCounter();
let reqSeq = 0;

async function one(kind, item, body, extra = {}) {
  const wire = { ...body, messages: toWire(body.messages) };
  const full = eng.body({ temperature: 0, max_tokens: body.max_tokens, stream_options: { include_usage: true }, model: launch.modelName || 'model', ...wire }, cfg);
  const rec = await streamChat(base, full, { headers: launch.headers || {} });
  const text = generatedText(rec);
  const firstChunkText = rec.chunks.length ? rec.chunks[0][1] + rec.chunks[0][2] + rec.chunks[0][3] : '';
  const [cAll, cFirst] = (await tok.count([text, firstChunkText])).counts;
  const row = {
    seq: reqSeq++, kind, prompt_id: item?.id ?? null, corpus_prompt_tokens: item?.n_prompt_tokens ?? null, ...extra,
    t_send: rec.t_send, t_first: rec.t_first, t_last: rec.t_last, t_end: rec.t_end ?? null,
    n_gen_tokens_retok: cAll, n_first_chunk_tokens_retok: cFirst, usage: rec.usage, finish_reason: rec.finish_reason,
    content: rec.content, reasoning: rec.reasoning, tool_calls: rec.tool_calls.filter(Boolean), engine_extra: rec.extra, error: rec.error,
    chunks: rec.chunks, max_tokens: body.max_tokens,
  };
  fs.appendFileSync(reqFile, JSON.stringify(row) + '\n');
  if (rec.error) log(`ERROR ${kind} ${item?.id}: ${rec.error.slice(0, 300)}`);
  return row;
}

async function measured(name, fn) {
  const t0 = now();
  const res = await fn();
  fs.appendFileSync(path.join(outDir, 'measurements.jsonl'), JSON.stringify({ name, t0, t1: now(), seqs: res }) + '\n');
  return res;
}

const msgs = (it) => it.messages;
async function decodeRun(items, maxTokens = 256) {
  const rows = await Promise.all(items.map((it) => one(`decode-c${items.length}`, it, { messages: msgs(it), max_tokens: maxTokens }, { concurrency: items.length })));
  return rows.map((r) => r.seq);
}
async function prefillRun(it, label) {
  const r = await one(label, it, { messages: msgs(it), max_tokens: 8 });
  return [r.seq];
}
async function agentRun(sessions, label) {
  // Each session plays its scripted turns in order; sessions run concurrently.
  const seqs = [];
  await Promise.all(sessions.map(async (s) => {
    for (const turn of s.turns) {
      const r = await one(label, { id: `${s.id}/t${turn.turn}`, n_prompt_tokens: turn.n_prompt_tokens }, { messages: turn.messages, tools: turn.tools, max_tokens: 300 }, { session: s.id, turn: turn.turn, appended_tokens: turn.appended_tokens, concurrency: sessions.length });
      seqs.push(r.seq);
    }
  }));
  return seqs;
}
const QCODE_INSTR = 'Continue the following source file from exactly where it stops. Output only the continuation as plain code, with no commentary and no code fences.';
export const qcodeMessages = (it) => [{ role: 'user', content: `${QCODE_INSTR}\n\n${it.prompt}` }];

async function main() {
  const t_spawn = now();
  engineProc = startEngine({ ...launch, logFile: path.join(outDir, 'server.log') });
  meta.engine_pid = engineProc.pid;
  mem = new MemSampler(engineProc.pid, path.join(outDir, 'memory.jsonl'), (t) => {
    memoryTrip = t;
    log(`MEMORY GUARD: footprint ${(t.phys_footprint / 2 ** 30).toFixed(1)} GB, available ${(t.available_share * 100).toFixed(1)}% - killing the engine`);
    killGroup(engineProc, 'SIGKILL');
  });
  const t_ready = await waitReady(base, engineProc, launch.readyTimeoutMs ?? 900_000, launch.headers || {});
  meta.load = { t_spawn, t_ready, server_ready_s: (t_ready - t_spawn) / 1000 };
  log(`ready in ${meta.load.server_ready_s.toFixed(1)} s`);
  // warmup (kernel compilation etc.): its own prompt, never reused
  const w = await one('warmup', D[899], { messages: msgs(D[899]), max_tokens: 16 });
  meta.load.first_token_ready_s = (w.t_first - t_spawn) / 1000;
  saveMeta();

  const conc = eng.concurrency !== false;
  if (smoke) {
    await measured('smoke-decode', () => decodeRun([D[0]], 32));
    if (conc) await measured('smoke-c2', () => decodeRun([D[1], D[2]], 32));
    for (const it of [QT[0], QT[3]]) await measured(`smoke-${it.id}`, async () => [(await one('qtool', it, { messages: it.messages, tools: it.tools, max_tokens: 200 })).seq]);
  } else if (suite === 'sweep') {
    for (let i = 0; i < 3; i++) await measured(`decode-c1-${i}`, () => decodeRun([D[i]]));
    for (let i = 0; i < 2; i++) await measured(`prefill2k-${i}`, () => prefillRun(P2[i], 'prefill2k'));
    await measured('prefill8k-0', () => prefillRun(P8[0], 'prefill8k'));
    if (conc) await measured('decode-c4-0', () => decodeRun(D.slice(3, 7)));
    if (cfg.sweepAgent) await measured('agent-c1', () => agentRun([A[0]].map(agentMsgs), 'agent-c1'));
  } else if (suite === 'full') {
    for (let i = 0; i < 5; i++) await measured(`decode-c1-${i}`, () => decodeRun([D[10 + i]]));
    for (let i = 0; i < 5; i++) await measured(`prefill2k-${i}`, () => prefillRun(P2[10 + i], 'prefill2k'));
    for (let i = 0; i < 3; i++) await measured(`prefill8k-${i}`, () => prefillRun(P8[10 + i], 'prefill8k'));
    for (let i = 0; i < 3; i++) await measured(`decode-c2-${i}`, () => decodeRun(D.slice(20 + 2 * i, 22 + 2 * i)));
    for (let i = 0; i < 3; i++) await measured(`decode-c4-${i}`, () => decodeRun(D.slice(30 + 4 * i, 34 + 4 * i)));
    await measured('agent-c1', () => agentRun([A[10]].map(agentMsgs), 'agent-c1'));
    await measured('agent-c4', () => agentRun(A.slice(11, 15).map(agentMsgs), 'agent-c4'));
  } else if (suite === 'quality') {
    for (const it of QC) await measured(`qcode-${it.id}`, async () => [(await one('qcode', it, { messages: qcodeMessages(it), max_tokens: 128 })).seq]);
    for (const it of QT) await measured(`qtool-${it.id}`, async () => [(await one('qtool', it, { messages: it.messages, tools: it.tools, max_tokens: 1024 })).seq]);
  } else throw new Error(`unknown suite ${suite}`);
}

// agent sessions: turn k sends the session's first n_messages messages (append-only history).
function agentMsgs(s) {
  return { id: s.id, turns: s.turns.map((t) => ({ ...t, messages: s.messages.slice(0, t.n_messages), tools: s.tools })) };
}

let code = 0;
try {
  await main();
  if (memoryTrip) throw new Error('memory guard tripped');
  await cleanup('complete');
  log('done', outDir);
} catch (e) {
  if (memoryTrip) { log('FAILED (memory guard)'); meta.failure_class = 'memory'; meta.memory_guard = memoryTrip; meta.error = String(e?.stack || e); await cleanup('failed'); code = 1; }
  else { log('FAILED', e?.stack || e); meta.error = String(e?.stack || e); await cleanup('failed'); code = 1; }
}
tok.close();
console.log(outDir);
process.exit(code);
