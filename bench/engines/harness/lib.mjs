// Shared harness pieces: engine process control, memory sampling,
// streaming OpenAI-compatible client, token counting with the model's own tokenizer.
import { spawn, execFileSync } from 'node:child_process';
import http from 'node:http';
import fs from 'node:fs';
import path from 'node:path';
import os from 'node:os';
import readline from 'node:readline';
import { fileURLToPath } from 'node:url';

export const HERE = path.dirname(fileURLToPath(import.meta.url));
export const ENGINES_DIR = path.resolve(HERE, '..');
export const REPO = path.resolve(ENGINES_DIR, '../..');
export const MODELS = path.join(os.homedir(), '.cortexcode/models');
// Qwen3.8 tokenizer. The corpus was counted with mlx-community/Qwen3.8-27B-4bit's tokenizer.json
// (sha256 06b95093...); that copy was removed by the owner's disk cleanup, and the Mirai-M copy
// reproduces the corpus token counts exactly (176/176 sampled prompts, checked 2026-10-15).
export const TOKENIZER = path.join(MODELS, 'trymirai-Qwen3.8-27B-M/8b878f57094a616c0ea4c214fe774af0cbfb4353/tokenizer.json');
export const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
export const now = () => performance.timeOrigin + performance.now(); // ms, monotonic-ish epoch

// ---------------------------------------------------------------- memory sampling
function vmUsedBytes() {
  const t = execFileSync('vm_stat', { encoding: 'utf8' });
  const page = Number(t.match(/page size of (\d+)/)[1]);
  const g = (k) => Number((t.match(new RegExp(k + ':\\s+(\\d+)')) || [0, 0])[1]);
  // used = active + wired + compressor occupancy (structural accounting of vm_stat fields)
  return (g('Pages active') + g('Pages wired down') + g('Pages occupied by compressor')) * page;
}
function treePids(root) {
  const out = execFileSync('ps', ['-axo', 'pid=,ppid='], { encoding: 'utf8' });
  const rows = out.trim().split('\n').map((l) => l.trim().split(/\s+/).map(Number));
  const set = new Set([root]);
  let grew = true;
  while (grew) { grew = false; for (const [p, pp] of rows) if (!set.has(p) && set.has(pp)) { set.add(p); grew = true; } }
  return [...set];
}
export function footprint(pid) {
  try {
    const t = execFileSync('footprint', ['-p', String(pid), '--noCategories', '-f', 'bytes'], { encoding: 'utf8', stdio: ['ignore', 'pipe', 'ignore'] });
    const cur = t.match(/phys_footprint:\s+(\d+)/);
    const peak = t.match(/phys_footprint_peak:\s+(\d+)/);
    return { phys_footprint: cur ? Number(cur[1]) : null, phys_footprint_peak: peak ? Number(peak[1]) : null };
  } catch { return { phys_footprint: null, phys_footprint_peak: null }; }
}
/** System-wide free memory share as macOS reports it (kern.memorystatus_level, the percentage
 *  `memory_pressure` prints as "System-wide memory free percentage"). */
export function availableShare() {
  return Number(execFileSync('sysctl', ['-n', 'kern.memorystatus_level'], { encoding: 'utf8' }).trim()) / 100;
}
// Memory guard (owner rule after Crane's runaway growth): engine footprint above 40 GB or system
// available memory below 15% ends the run.
export const GUARD = { maxFootprint: 40 * 2 ** 30, minAvailable: 0.15 };
export class MemSampler {
  constructor(pid, file, onExceed) {
    this.onExceed = onExceed; this.tripped = null;
    this.pid = pid; this.file = file; this.base = vmUsedBytes();
    this.peakRss = 0; this.peakSys = 0; this.peakFoot = 0; this.n = 0;
    fs.appendFileSync(file, JSON.stringify({ t: now(), kind: 'baseline', sys_used: this.base }) + '\n');
    this.timer = setInterval(() => this.sample(), 1000);
  }
  sample() {
    try {
      const pids = treePids(this.pid);
      const out = execFileSync('ps', ['-o', 'rss=', '-p', pids.join(',')], { encoding: 'utf8' });
      const rss = out.trim().split('\n').reduce((a, l) => a + Number(l.trim() || 0) * 1024, 0);
      const sys = vmUsedBytes() - this.base;
      const rec = { t: now(), rss, sys_delta: sys, available_share: Math.round(availableShare() * 1000) / 1000 };
      this.n++;
      Object.assign(rec, footprint(this.pid));
      if (!this.tripped && ((rec.phys_footprint ?? 0) > GUARD.maxFootprint || rec.available_share < GUARD.minAvailable)) {
        this.tripped = { t: rec.t, phys_footprint: rec.phys_footprint, available_share: rec.available_share, rss };
        fs.appendFileSync(this.file, JSON.stringify({ ...rec, kind: 'guard-tripped' }) + '\n');
        this.onExceed?.(this.tripped);
      }
      this.peakRss = Math.max(this.peakRss, rss); this.peakSys = Math.max(this.peakSys, sys);
      if (rec.phys_footprint_peak) this.peakFoot = Math.max(this.peakFoot, rec.phys_footprint_peak);
      fs.appendFileSync(this.file, JSON.stringify(rec) + '\n');
    } catch {}
  }
  stop() {
    clearInterval(this.timer);
    const f = footprint(this.pid);
    if (f.phys_footprint_peak) this.peakFoot = Math.max(this.peakFoot, f.phys_footprint_peak);
    fs.appendFileSync(this.file, JSON.stringify({ t: now(), kind: 'final', ...f }) + '\n');
    return { peak_rss: this.peakRss, peak_sys_delta: this.peakSys, phys_footprint_peak: this.peakFoot || null };
  }
}

// ---------------------------------------------------------------- engine process
export function startEngine({ cmd, args, env, cwd, logFile }) {
  const out = fs.openSync(logFile, 'a');
  // own process group, so the whole engine (and any helper it spawns) can be stopped together
  const p = spawn(cmd, args, { cwd, env: { ...process.env, ...env }, stdio: ['ignore', out, out], detached: true });
  return p;
}
export function killGroup(p, sig) { try { process.kill(-p.pid, sig); } catch { try { p.kill(sig); } catch {} } }
export async function stopEngine(p) {
  if (!p || p.exitCode !== null) return;
  killGroup(p, 'SIGTERM');
  for (let i = 0; i < 100 && p.exitCode === null && p.signalCode === null; i++) await sleep(100);
  if (p.exitCode === null && p.signalCode === null) killGroup(p, 'SIGKILL');
  await sleep(500);
}
export async function waitReady(base, p, timeoutMs, headers = {}) {
  const t0 = now();
  while (now() - t0 < timeoutMs) {
    if (p.exitCode !== null || p.signalCode !== null) throw new Error(`engine exited (${p.exitCode ?? p.signalCode}) before ready`);
    try {
      const r = await fetch(base + '/v1/models', { headers, signal: AbortSignal.timeout(2000) });
      if (r.ok) return now();
    } catch {}
    await sleep(250);
  }
  throw new Error('engine not ready in time');
}

// ---------------------------------------------------------------- streaming chat client
/**
 * One streamed chat completion. Records every SSE chunk with its arrival time.
 * Returns {t_send, t_first, t_last, chunks:[[t, content, reasoning, toolArgsDelta]], content, reasoning, tool_calls, usage, finish_reason, raw_final, error}
 */
export async function streamChat(base, body, { headers = {}, timeoutMs = 3_600_000 } = {}) {
  // node:http rather than fetch: undici's 300 s headers timeout would abort engines that send
  // their response headers only after a long prefill.
  const rec = { t_send: now(), t_first: null, t_last: null, chunks: [], content: '', reasoning: '', tool_calls: [], usage: null, finish_reason: null, extra: null, error: null };
  const u = new URL(base + '/v1/chat/completions');
  const payload = Buffer.from(JSON.stringify({ ...body, stream: true }));
  await new Promise((resolve) => {
    const req = http.request({ hostname: u.hostname, port: u.port, path: u.pathname, method: 'POST', headers: { 'content-type': 'application/json', 'content-length': payload.length, ...headers } }, (res) => {
      rec.headers = Object.fromEntries(Object.entries(res.headers).filter(([k]) => k.startsWith('x-')));
      if (res.statusCode < 200 || res.statusCode >= 300) {
        let t = '';
        res.on('data', (d) => (t += d));
        res.on('end', () => { rec.error = `HTTP ${res.statusCode}: ${t.slice(0, 2000)}`; resolve(); });
        return;
      }
      res.setEncoding('utf8');
      let buf = '';
      res.on('data', (part) => {
        const t = now();
        buf += part;
        let idx;
        while ((idx = buf.search(/\r?\n\r?\n/)) >= 0) {
          const event = buf.slice(0, idx);
          buf = buf.slice(idx).replace(/^\r?\n\r?\n/, '');
          for (const line of event.split(/\r?\n/)) {
            if (!line.startsWith('data:')) continue;
            const data = line.slice(5).trim();
            if (data === '[DONE]') continue;
            let j;
            try { j = JSON.parse(data); } catch { continue; }
            if (j.error) { rec.error = JSON.stringify(j.error).slice(0, 2000); continue; }
            if (j.usage) rec.usage = j.usage;
            if (j.timings) rec.extra = { ...(rec.extra || {}), timings: j.timings };
            const ch = j.choices?.[0];
            if (!ch) continue;
            if (ch.finish_reason) rec.finish_reason = ch.finish_reason;
            const d = ch.delta || {};
            const c = typeof d.content === 'string' ? d.content : '';
            const rc = typeof d.reasoning_content === 'string' ? d.reasoning_content : (typeof d.reasoning === 'string' ? d.reasoning : '');
            let ta = '';
            for (const tc of d.tool_calls || []) {
              const i = tc.index ?? 0;
              rec.tool_calls[i] ??= { name: '', arguments: '' };
              if (tc.function?.name) { rec.tool_calls[i].name += tc.function.name; ta += tc.function.name; }
              if (tc.function?.arguments) { rec.tool_calls[i].arguments += tc.function.arguments; ta += tc.function.arguments; }
            }
            if (c || rc || ta) {
              if (rec.t_first === null) rec.t_first = t;
              rec.t_last = t;
              rec.content += c; rec.reasoning += rc;
              rec.chunks.push([Math.round((t - rec.t_send) * 1000) / 1000, c, rc, ta]);
            }
          }
        }
      });
      res.on('end', () => { rec.t_end = now(); resolve(); });
      res.on('error', (e) => { rec.error = String(e?.stack || e).slice(0, 2000); resolve(); });
    });
    req.setTimeout(timeoutMs, () => { rec.error = 'client timeout'; req.destroy(); });
    req.on('error', (e) => { rec.error = String(e?.stack || e).slice(0, 2000); resolve(); });
    req.end(payload);
  });
  return rec;
}

/** Qwen3.8 tool-call text exactly as the chat template renders it (used to count tool-call tokens uniformly). */
export function toolCallText(tc) {
  let args = {};
  try { args = typeof tc.arguments === 'string' ? JSON.parse(tc.arguments || '{}') : tc.arguments; } catch { return `<tool_call>\n<function=${tc.name}>\n${tc.arguments}\n</function>\n</tool_call>`; }
  let s = `<tool_call>\n<function=${tc.name}>\n`;
  for (const [k, v] of Object.entries(args || {})) s += `<parameter=${k}>\n${typeof v === 'string' ? v : JSON.stringify(v)}\n</parameter>\n`;
  return s + '</function>\n</tool_call>';
}
export function generatedText(rec) {
  const calls = rec.tool_calls.filter(Boolean).map(toolCallText).join('\n');
  return (rec.reasoning ? rec.reasoning + '\n' : '') + rec.content + (calls ? (rec.content ? '\n\n' : '') + calls : '');
}

// ---------------------------------------------------------------- token counting
export class TokCounter {
  constructor() {
    this.p = spawn('uv', ['run', '-q', '--with', 'tokenizers', 'python', '-I', path.join(HERE, 'tokcount.py'), TOKENIZER], { stdio: ['pipe', 'pipe', 'inherit'] });
    this.rl = readline.createInterface({ input: this.p.stdout });
    this.pending = new Map(); this.id = 0;
    this.rl.on('line', (l) => { const j = JSON.parse(l); const r = this.pending.get(j.id); this.pending.delete(j.id); r?.(j); });
  }
  async count(texts, withIds = false) {
    const id = ++this.id;
    const p = new Promise((r) => this.pending.set(id, r));
    this.p.stdin.write(JSON.stringify({ id, texts, with_ids: withIds }) + '\n');
    return p;
  }
  close() { this.p.stdin.end(); }
}

export function readJsonl(file) {
  return fs.readFileSync(file, 'utf8').split('\n').filter(Boolean).map((l) => JSON.parse(l));
}
export function sha256File(file) {
  return execFileSync('shasum', ['-a', '256', file], { encoding: 'utf8' }).split(/\s+/)[0];
}

// ---------------------------------------------------------------- flattened ChatML messages
// Engines whose server renders a minimal ChatML (no tool support) receive messages that are
// already in the Qwen3.8 template's textual form: the tool block goes into the system message and
// assistant tool calls become their XML text. Rendered by such a server, these produce the exact
// token sequence the official template produces (checked per request against the engine's
// reported prompt token count where it reports one).
const TOOLS_PREAMBLE = '# Tools\n\nYou have access to the following functions:\n\n<tools>';
const TOOLS_POST = '\n</tools>\n\nIf you choose to call a function ONLY reply in the following format with NO suffix:\n\n<tool_call>\n<function=example_function_name>\n<parameter=example_parameter_1>\nvalue_1\n</parameter>\n<parameter=example_parameter_2>\nThis is the value for the second parameter\nthat can span\nmultiple lines\n</parameter>\n</function>\n</tool_call>\n\n<IMPORTANT>\nReminder:\n- Function calls MUST follow the specified format: an inner <function=...></function> block must be nested within <tool_call></tool_call> XML tags\n- Required parameters MUST be specified\n- You may provide optional reasoning for your function call in natural language BEFORE the function call, but NOT after\n- If there is no function call available, answer the question like normal with your current knowledge and do not tell the user about function calls\n</IMPORTANT>';
// Python json.dumps default separators (", " and ": "), as the template's tojson filter emits.
export function pyJson(v) {
  if (v === null) return 'null';
  if (Array.isArray(v)) return '[' + v.map(pyJson).join(', ') + ']';
  if (typeof v === 'object') return '{' + Object.entries(v).map(([k, x]) => JSON.stringify(k) + ': ' + pyJson(x)).join(', ') + '}';
  return JSON.stringify(v);
}
export function flattenMessages(messages, tools) {
  const out = [];
  let sys = null;
  if (tools?.length) {
    sys = TOOLS_PREAMBLE + tools.map((t) => '\n' + pyJson(t)).join('') + TOOLS_POST;
    if (messages[0]?.role === 'system' && messages[0].content.trim()) sys += '\n\n' + messages[0].content.trim();
    out.push({ role: 'system', content: sys });
  } else if (messages[0]?.role === 'system') out.push({ role: 'system', content: messages[0].content.trim() });
  for (const m of messages) {
    if (m.role === 'system') continue;
    if (m.role === 'assistant') {
      const content = (m.content || '').trim();
      let s = content;
      (m.tool_calls || []).forEach((tc, i) => {
        const f0 = tc.function || tc;
        const f = { ...f0, arguments: typeof f0.arguments === 'string' ? JSON.parse(f0.arguments || '{}') : f0.arguments };
        let x = (i === 0 ? (content ? '\n\n' : '') : '\n') + `<tool_call>\n<function=${f.name}>\n`;
        for (const [k, v] of Object.entries(f.arguments || {})) x += `<parameter=${k}>\n${typeof v === 'string' ? v : pyJson(v)}\n</parameter>\n`;
        s += x + '</function>\n</tool_call>';
      });
      out.push({ role: 'assistant', content: s });
    } else out.push({ role: m.role, content: typeof m.content === 'string' ? m.content.trim() : m.content });
  }
  return out;
}

/** Parse Qwen3.8 XML tool calls out of raw text (structural parser). */
export function parseToolCallsXml(text) {
  const calls = [];
  const re = /<tool_call>\s*<function=([^>\n]+)>([\s\S]*?)<\/function>\s*<\/tool_call>/g;
  let m;
  while ((m = re.exec(text))) {
    const args = {};
    const pre = /<parameter=([^>\n]+)>\n?([\s\S]*?)\n?<\/parameter>/g;
    let p;
    while ((p = pre.exec(m[2]))) args[p[1].trim()] = p[2];
    calls.push({ name: m[1].trim(), arguments: args });
  }
  return calls;
}

/** OpenAI wire form: assistant tool-call arguments as JSON strings; tool results carry their call id. */
export function toWire(messages) {
  let pending = [];
  let n = 0;
  return messages.map((m) => {
    if (m.tool_calls) {
      const tcs = m.tool_calls.map((tc) => ({ id: tc.id || `call_${n++}`, type: 'function', function: { name: tc.function.name, arguments: typeof tc.function.arguments === 'string' ? tc.function.arguments : JSON.stringify(tc.function.arguments) } }));
      pending = tcs.map((t) => t.id);
      return { ...m, tool_calls: tcs };
    }
    if (m.role === 'tool' && !m.tool_call_id) return { ...m, tool_call_id: pending.shift() || `call_${n++}` };
    return m;
  });
}

