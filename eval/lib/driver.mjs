// The CortexCode system under test: the real frame with the real plugins, driven headless over the
// frame's WebSocket API exactly as the UI and `cx` drive it.
//
// Edit format selection: tools.edit_format reaches the tools plugin through ExecContext.edit_format,
// which the chat flow fills. The contract has no per-session field for it (contract request filed:
// an optional `edit_format` on SessionCreateParams/TurnParams). Until then the harness runs one frame
// per edit format and sets the format through every channel that exists without contract changes:
//   - `[plugin.config] edit_format = "<fmt>"` for the tools and chatflow plugins (HelloAck.config),
//   - the `CORTEX_EDIT_FORMAT` environment variable on the frame (inherited by plugins).
// Whether the format actually took effect is measured, not assumed: the edit tool calls recorded in
// the journal are classified by argument shape (see editShape) and reported per run.
import fs from 'node:fs';
import fsp from 'node:fs/promises';
import os from 'node:os';
import path from 'node:path';
import { appHome, buildFrameConfig, folderDefault, Frame, FrameClient } from './frame.mjs';
import { TOOLCHAINS } from './paths.mjs';
import { ensureDir, sleep } from './util.mjs';
import { requireCleanSlate, withdrawBenchmarkLessons } from './cleanslate.mjs';

export const MODELS_DIR = path.join(os.homedir(), '.cortexcode', 'models');
const CALIBRATION_CACHE_DIR = path.join(TOOLCHAINS, 'model-state');

export const EDIT_FORMATS = ['hashline', 'replace', 'whole'];
// A turn's last status: `stopped` is the chat flow's stop when the work repeats itself (a guard);
// `blocked` is a turn that ended waiting for the user, who never answers in a benchmark: the attempt
// is graded as it stands and the run goes on.
const TERMINAL = new Set(['done', 'failed', 'cancelled', 'stopped', 'blocked']);

/** The open app's frame (`run/frame.json` whose process is alive), or null. */
export function runningApp() {
  try {
    const info = JSON.parse(fs.readFileSync(path.join(appHome(), 'run', 'frame.json'), 'utf8'));
    process.kill(info.pid, 0);
    return info;
  } catch {
    return null;
  }
}

let stopHooked = false;
/** Once per process: on SIGTERM/SIGINT cancel the active project's runs in the app, then exit. */
function installStopHook(system) {
  if (stopHooked) return;
  stopHooked = true;
  const stop = (signal) => {
    const active = system.active;
    const done = () => process.exit(signal === 'SIGINT' ? 130 : 143);
    if (!system.client) return done();
    const cancel = active ? system.cancelAll(active.projectId) : Promise.resolve();
    Promise.race([cancel, sleep(15000)]).then(done, done);
  };
  process.once('SIGTERM', () => stop('SIGTERM'));
  process.once('SIGINT', () => stop('SIGINT'));
}

export class CortexSystem {
  /**
   * @param {object} o
   * @param {string} o.runDir
   * @param {string} o.model           model plugin variant id
   * @param {string|null} o.editFormat
   * @param {string} o.tag             sandbox tag
   * @param {string[]} o.denyRead      hidden paths
   * @param {object} o.env             agent environment (PATH, offline settings)
   * @param {string[]} [o.categories]  restrict plugins (baselines need only the model)
   */
  constructor(o) {
    Object.assign(this, o);
  }

  async start() {
    // While the app is open, its frame is driven (its window shows every task as a conversation;
    // the app's own home and config are used, and no second model is loaded). A run that overrides
    // the product (model variant, edit format, a plugin subset) needs its own frame.
    // CORTEX_EVAL_ATTACH=0: always a hidden frame; =1: the app's, or an error when it is not open.
    const mode = process.env.CORTEX_EVAL_ATTACH;
    const overrides = (this.model && this.model !== folderDefault('plugins/model-uzu', 'variant')) || this.editFormat || this.categories;
    if (mode === '1' || (mode !== '0' && !overrides && runningApp())) return this.attach();
    const home = path.join(this.runDir, 'home');
    // The frame home is private; its models/ entry points at the shared model store (read-only
    // hardlinks), which is where a model plugin looks by default ($CORTEX_HOME/models).
    await ensureDir(home);
    if (fs.existsSync(MODELS_DIR) && !fs.existsSync(path.join(home, 'models'))) await fsp.symlink(MODELS_DIR, path.join(home, 'models'));
    // Only what differs from the product's own configuration is overridden.
    const pluginConfig = this.model && this.model !== folderDefault('plugins/model-uzu', 'variant') ? { model: { variant: this.model } } : {};
    if (this.editFormat) {
      pluginConfig.tools = { ...pluginConfig.tools, edit_format: this.editFormat };
      pluginConfig.chatflow = { edit_format: this.editFormat };
    }
    const config = buildFrameConfig({ home, pluginConfig, categories: this.categories ?? null });
    const env = { ...this.env };
    if (this.editFormat) env.CORTEX_EDIT_FORMAT = this.editFormat;
    // The model plugin measures slots/speculation once per model file and machine and stores the
    // result in its data dir; carry that measurement across eval frames instead of re-measuring.
    // The frame names a plugin's data dir after its packed executable (frame/src/units.rs).
    const modelPlugin = config.installed.find((p) => p.category === 'model');
    const modelId = modelPlugin?.dataName ?? 'model';
    this.calibrationCache = path.join(CALIBRATION_CACHE_DIR, `${modelId}.calibration.json`);
    this.calibrationFile = path.join(home, 'plugins-data', modelId, 'calibration.json');
    if (fs.existsSync(this.calibrationCache)) {
      await ensureDir(path.dirname(this.calibrationFile));
      await fsp.copyFile(this.calibrationCache, this.calibrationFile);
    }
    // CortexCode runs as it does normally (owner, 2026-10-15).
    this.frame = new Frame({ runDir: this.runDir, config, sandboxProfile: null, env });
    try {
      await this.frame.start();
    } catch (error) {
      await this.frame.stop().catch(() => {});
      throw error;
    }
    await this.saveCalibration();
    this.client = this.frame.client;
    this.modelInfo = await this.client.request('model.info', {}).catch((e) => ({ error: String(e) }));
    return this;
  }

  async stop() {
    await this.frame?.stop();
    await this.saveCalibration();
  }

  /** Connect to the running app's frame (`<CORTEX_HOME>/run/frame.json`); stopping only disconnects. */
  async attach() {
    const home = appHome();
    const info = runningApp();
    if (!info) throw new Error(`CORTEX_EVAL_ATTACH=1 but no app frame is running (${path.join(home, 'run', 'frame.json')})`);
    const client = await new FrameClient(`ws://127.0.0.1:${info.port}/ws?token=${encodeURIComponent(info.token)}`).connect();
    const listed = await client.request('frame.plugins.list', {});
    this.client = client;
    // Offline through the workspace, never through the app's plugin set: every workspace carries
    // the tools sandbox's offline marker, so its commands get no network and its research:// search
    // and fetch answer "offline". The exercises and their tests are public: a model that fetched
    // them would not be solving them (seen 2026-10-16: a clone of the exercise repository). The
    // harness used to disable the research plugin and re-enable it at exit; a run that died left
    // the owner's app without research (2026-10-17, after a kernel panic).
    // Runs of an earlier benchmark that the app resumed after a restart are this harness's litter:
    // cancelled before anything new starts, so nothing of a previous run shares the model.
    await this.cancelStale(client);
    this.frame = {
      proc: { pid: info.pid }, home, pluginInfo: listed.plugins ?? listed,
      configPath: path.join(path.dirname(new URL(import.meta.url).pathname), '..', '..', 'cortex.dev.toml'),
      logPath: path.join(home, 'logs', 'frame.log'),
      stop: async () => {
        try { client.close(); } catch { /* closed */ }
      },
    };
    this.modelInfo = await client.request('model.info', {}).catch((e) => ({ error: String(e) }));
    return this;
  }

  async saveCalibration() {
    try {
      if (this.calibrationFile && fs.existsSync(this.calibrationFile)) {
        await ensureDir(path.dirname(this.calibrationCache));
        await fsp.copyFile(this.calibrationFile, this.calibrationCache);
      }
    } catch { /* best effort: a missing cache only costs a re-measurement */ }
  }

  slots() {
    const n = Number(this.modelInfo?.slots);
    return Number.isFinite(n) && n > 0 ? n : 1;
  }

  async endpoint() {
    return this.client.request('model.endpoint', {});
  }

  async toolSchemas(editFormat) {
    return this.client.request('tools.schemas', { edit_format: editFormat ?? null }).catch((e) => ({ error: String(e) }));
  }

  async openTask(workspace, title) {
    // Every task starts from a clean slate: the shipped skills, no learned state (cleanslate.mjs);
    // what earlier benchmark tasks taught is withdrawn first.
    await withdrawBenchmarkLessons((method, params) => this.client.request(method, params));
    this.slate = await requireCleanSlate((method, params) => this.client.request(method, params), { codeDir: path.join(this.frame.home, 'code') });
    const project = await this.client.request('frame.project.open', { root: workspace });
    const session = await this.client.request('frame.session.create', { project_id: project.id, kind: 'chat', title });
    return { project, session };
  }

  /** Another chat conversation of an open task's project (a sequential suite moving to a new session). */
  async newSession(project, title) {
    return this.client.request('frame.session.create', { project_id: project.id, kind: 'chat', title });
  }

  /** `{text, pasted, name?}` -> the turn's text and a pasted-text attachment made through `chatflow.attachment.put`. */
  async pastedMessage(project, session, message) {
    const pasted = String(message.pasted ?? '');
    if (!pasted) return { text: message.text, attachments: [] };
    const name = message.name ?? 'test-output.txt';
    const put = await this.client.request('chatflow.attachment.put', {
      data: Buffer.from(pasted, 'utf8').toString('base64'), name, mime: 'text/plain', project_id: project.id, session_id: session.id,
    });
    const attachment = { kind: 'file', path: put.path, name: put.name ?? name, mime: put.mime ?? 'text/plain', bytes: put.size ?? pasted.length, stored: put.stored ?? true, pasted_text: true, lines: pasted.split('\n').length };
    if (put.sha256) attachment.sha256 = put.sha256;
    return { text: message.text, attachments: [attachment] };
  }

  /** Cancel running sessions of earlier benchmark workspaces (`cceval-*`) the app still holds. */
  async cancelStale(client) {
    const listed = await client.request('frame.project.list', {}).catch(() => null);
    const projects = (listed?.projects ?? listed ?? []).filter((p) => /\/cceval-/.test(String(p.root)));
    for (const project of projects) {
      const sessions = await client.request('frame.session.list', { project_id: project.id }).catch(() => []);
      for (const s of (Array.isArray(sessions) ? sessions : (sessions?.sessions ?? []))) {
        if (s.status === 'running' || s.status === 'waiting') await client.request('chatflow.cancel', { session_id: s.id }, { timeoutMs: 30000 }).catch(() => {});
      }
    }
  }

  async readJournal(sessionId, from = 1) {
    const r = await this.client.request('frame.journal.read', { session_id: sessionId, from });
    return r?.entries ?? [];
  }

  async sessions(projectId) {
    const r = await this.client.request('frame.session.list', { project_id: projectId });
    return Array.isArray(r) ? r : (r?.sessions ?? []);
  }

  /**
   * Send one user message (chatflow.turn, mode auto) and follow the journal until the run ends and
   * every agent session spawned in the project is quiescent, or until `deadlineMs` (wall clock),
   * when the runs are cancelled and the attempt is recorded as timed out.
   */
  async turn({ project, session }, message, deadlineMs, shouldStop = () => false) {
    const startedMs = Date.now();
    // Stopped from outside (SIGTERM/SIGINT, the runner killed): the turn in flight is cancelled in
    // the app before this process ends, so nothing keeps running on the GPU for nobody.
    this.active = { projectId: project.id };
    installStopHook(this);
    const before = await this.readJournal(session.id);
    const fromSeq = (before.at(-1)?.seq ?? 0) + 1;
    // A message is a string, or `{text, pasted}`: pasted output (a test run) goes the way a
    // person's long paste goes, as a pasted-text attachment the model reads, with the text as the
    // message. The chat shows the compact card instead of the whole log.
    const { text, attachments } = typeof message === 'string' ? { text: message, attachments: [] } : await this.pastedMessage(project, session, message);
    const turn = await this.client.request('chatflow.turn', { session_id: session.id, text, mode: 'auto', ...(attachments.length ? { attachments } : {}) }, { timeoutMs: 120000 });
    let lastSeq = fromSeq - 1;
    let terminal = null;
    let lastChange = Date.now();
    let timedOut = false;
    let aborted = false;
    while (true) {
      if (Date.now() > deadlineMs) { timedOut = true; break; }
      // shouldStop() === 'timeout': the benchmark's own harness ended the agent phase (its timeout).
      const stop = shouldStop();
      if (stop === 'timeout') { timedOut = true; break; }
      if (stop) { aborted = true; break; }
      const fresh = await this.readJournal(session.id, lastSeq + 1).catch(() => []);
      if (fresh.length) { lastSeq = fresh.at(-1).seq; lastChange = Date.now(); }
      for (const e of fresh) {
        if (e.kind === 'status' && TERMINAL.has(e.data?.state) && (!e.data?.run_id || e.data.run_id === turn.run_id)) terminal = e;
        // Nobody is here to answer an approval (a sandbox lift for an install or the network): it is
        // denied at once, as a headless run denies every prompt, and the model works with the result.
        // An approved lift would also drop the offline rules that hide the tests.
        if (e.kind === 'approval' && e.data?.state === 'pending') {
          await this.client.request('chatflow.approve', { session_id: session.id, seq: e.seq, approve: false }).catch(() => {});
        }
      }
      if (terminal) {
        const state = await this.client.request('chatflow.state', { session_id: session.id }).catch(() => null);
        if (!state || (!state.running && !state.pending_followups)) {
          if (await this.agentsQuiescent(project.id, session.id)) break;
        }
      }
      await sleep(terminal ? 2000 : 1000);
    }
    if (timedOut || aborted) await this.cancelAll(project.id);
    return { run_id: turn.run_id, queued_as_steer: !!turn.queued_as_steer, started_ms: startedMs, ended_ms: Date.now(), timed_out: timedOut, aborted, terminal: terminal?.data ?? null, from_seq: fromSeq, idle_ms: Date.now() - lastChange };
  }

  async agentsQuiescent(projectId, mainSessionId) {
    const all = await this.sessions(projectId).catch(() => []);
    return all.filter((s) => s.id !== mainSessionId).every((s) => s.status !== 'running');
  }

  async cancelAll(projectId) {
    const all = await this.sessions(projectId).catch(() => []);
    for (const s of all) {
      if (s.status === 'running' || s.status === 'waiting') {
        await this.client.request('chatflow.cancel', { session_id: s.id }, { timeoutMs: 30000 }).catch(() => {});
      }
    }
    const deadline = Date.now() + 60000;
    while (Date.now() < deadline) {
      const now = await this.sessions(projectId).catch(() => []);
      if (now.every((s) => s.status !== 'running')) return;
      await sleep(1000);
    }
  }

  /** Full materialized journals of every session in the project (raw log for the report). */
  async collect(projectId) {
    const out = {};
    for (const s of await this.sessions(projectId).catch(() => [])) {
      out[s.id] = { session: s, entries: await this.readJournal(s.id).catch((e) => [{ error: String(e) }]) };
    }
    return out;
  }
}

/**
 * Structural summary of a project's journals: token usage, model timing, tool calls.
 * Shared by the run loop and the report so numbers come from one definition.
 */
export function summarizeJournals(journals) {
  const s = {
    prompt_tokens: 0, cached_tokens: 0, completion_tokens: 0, prefill_ms: 0, decode_ms: 0, queue_ms: 0,
    model_calls: 0, tool_calls: 0, host_calls: 0, tool_errors: 0, tools: {}, edit_shapes: {}, sessions: 0, errors: [],
  };
  for (const { entries } of Object.values(journals)) {
    s.sessions++;
    for (const e of entries ?? []) {
      if (e.kind === 'usage') {
        const d = e.data ?? {};
        s.model_calls++;
        s.prompt_tokens += num(d.prompt_tokens);
        s.cached_tokens += num(d.cached_tokens);
        s.completion_tokens += num(d.completion_tokens);
        s.prefill_ms += num(d.prefill_ms);
        s.decode_ms += num(d.decode_ms);
        s.queue_ms += num(d.queue_ms);
      } else if (e.kind === 'tool_call' && e.data?.host) {
        // Host-driven reads (graph, symbols, repo map) the flow runs for the model.
        s.host_calls++;
      } else if (e.kind === 'tool_call') {
        s.tool_calls++;
        const tool = String(e.data?.tool ?? 'unknown');
        s.tools[tool] = (s.tools[tool] ?? 0) + 1;
        const shape = editShape(tool, e.data?.args);
        if (shape) s.edit_shapes[shape] = (s.edit_shapes[shape] ?? 0) + 1;
      } else if (e.kind === 'tool_result') {
        if (e.data?.ok === false && !e.data?.host) s.tool_errors++;
      } else if (e.kind === 'error') {
        s.errors.push(e.data?.code ?? e.data?.message ?? 'error');
      }
    }
  }
  return s;
}

function num(x) {
  const n = Number(x);
  return Number.isFinite(n) ? n : 0;
}

/**
 * Classify an `edit` tool call by the structure of its arguments, following the tools plugin's
 * schemas (plugins/tools/backend/src/schemas.rs): `edits` (anchored line ops) is the hashline format, `old`
 * is exact-string replacement, `content` alone is a whole-file write (available in every format to
 * create files). Arguments arrive as the model's raw JSON text or as an object.
 */
export function editShape(tool, args) {
  if (tool !== 'edit') return null;
  let a = args;
  if (typeof a === 'string') {
    try { a = JSON.parse(a); } catch { return 'unparsed'; }
  }
  if (!a || typeof a !== 'object') return 'unparsed';
  if (Array.isArray(a.edits)) return 'hashline';
  if (typeof a.old === 'string') return 'replace';
  if (typeof a.content === 'string') return 'whole';
  return 'other';
}

/** Shapes that only one format's schema offers (whole-file `content` exists in all of them). */
export const EXCLUSIVE_SHAPE = { hashline: 'hashline', replace: 'replace' };

/** Is the observed edit traffic consistent with the requested format? null = no evidence. */
export function formatConsistent(format, shapes) {
  const seen = Object.keys(shapes ?? {}).filter((k) => shapes[k] > 0);
  if (!seen.length) return null;
  for (const [fmt, shape] of Object.entries(EXCLUSIVE_SHAPE)) {
    if (fmt !== format && seen.includes(shape)) return false;
  }
  return true;
}

/** Token usage from a mini-swe-agent trajectory (raw API responses saved by the agent). */
export function trajectoryUsage(trajectory) {
  const out = { prompt_tokens: 0, completion_tokens: 0, model_calls: 0 };
  for (const m of trajectory?.messages ?? []) {
    const usage = m?.extra?.response?.usage;
    if (!usage) continue;
    out.prompt_tokens += Number(usage.prompt_tokens) || 0;
    out.completion_tokens += Number(usage.completion_tokens) || 0;
    out.model_calls++;
  }
  return out;
}
