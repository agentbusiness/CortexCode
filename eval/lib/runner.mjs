// Run loop: systems (CortexCode through the frame, aider, mini-swe-agent) x suite protocol,
// with resume, a per-task wall-clock budget, one task at a time (--parallel N, at most the model's slots), raw logs,
// and the integrity checks after every attempt.
import { markVerdict } from './verdict.mjs';
import fs from 'node:fs';
import fsp from 'node:fs/promises';
import path from 'node:path';
import { spawn } from 'node:child_process';
import { CortexSystem, summarizeJournals, trajectoryUsage } from './driver.mjs';
import { LOCAL_REFS, HIDDEN, RESULTS, SRC, tmpRoot } from './paths.mjs';
import { agentEnv, SUITES } from './suites.mjs';
import { LANG_READ_ONLY, py, T } from './toolchains.mjs';
import { checkHiddenAbsent, hiddenIndex, workspaceDiff } from './workspace.mjs';
import { ensureDir, killGroup, log, pool, readJson, rmrf, sha256, writeJson, writeText } from './util.mjs';

const EVAL_DIR = path.resolve(path.dirname(new URL(import.meta.url).pathname), '..');

export const SYSTEMS = ['cortex', 'aider', 'mini'];

/** Systems a suite runs (`--system all`, and the default): container suites name their own baselines. */
export function suiteSystems(suite) {
  return suite.systems ?? SYSTEMS;
}
/** Re-runs of a task whose timing a competitor invalidated before its timing is reported invalid. */

export function runKey({ suite, system, model, editFormat, run }) {
  const sys = system === 'cortex' && editFormat ? `cortex-${editFormat}` : system;
  return path.join(suite, sys, model, `run-${run}`);
}

/** Shared caches agents and scoring runs may read but never modify (cargo-home stays writable: cargo
 *  needs its lock file there even offline). */
export function readOnlyCaches() {
  return [T.polyglotJs, T.polyglotPy, T.pyTools, T.nodeTools, T.aider, T.mini, T.bats, T.nodeModules, T.rustTargets, T.uvCache, T.pnpmStore,
    ...LANG_READ_ONLY.filter((p) => fs.existsSync(p))];
}

/** Paths an agent must never read: hidden tasks, upstream clones, results, owner's original repo. */
export function hiddenPaths(extra = []) {
  return [HIDDEN, SRC, RESULTS, ...LOCAL_REFS, ...extra];
}

function safeTag(s) {
  return s.replace(/[^A-Za-z0-9_.-]/g, '_').slice(0, 60);
}

/** Literal hidden-path references in tool call arguments (string match on paths, informational). */
function journalPathReferences(journals, hidden) {
  const needles = new Set();
  // Full relative paths only (bare file names such as lib.rs would match unrelated files).
  for (const h of hidden) if (h.rel.includes('/') || h.rel.includes('_test') || h.rel.includes('.spec.') || h.rel.includes('.test.')) needles.add(h.rel);
  needles.add(HIDDEN);
  needles.add(RESULTS);
  const refs = [];
  for (const { entries } of Object.values(journals)) {
    for (const e of entries ?? []) {
      if (e.kind !== 'tool_call') continue;
      const text = JSON.stringify(e.data?.args ?? {});
      for (const n of needles) if (n && text.includes(n)) refs.push({ seq: e.seq, session_id: e.session_id, needle: n });
    }
  }
  return refs;
}

/** Failure class from structural signals, in priority order. */
function failureClass({ passed, timedOut, error, integrity, diffEmpty, score, terminal }) {
  if (error) return 'harness_error';
  if (integrity && !integrity.ok) return 'integrity_violation';
  if (passed) return null;
  if (timedOut) return 'timeout';
  if (terminal && terminal.state === 'failed') return 'run_failed';
  // Container suites name the failure from their harness's own verdict (empty_patch, patch_apply_failed, ...).
  if (score?.failure_class) return score.failure_class;
  if (diffEmpty) return 'no_change';
  if (score?.file_present === false) return 'file_missing';
  if (score?.timed_out) return 'test_timeout';
  return score && 'exact' in score ? 'edit_mismatch' : 'tests_failed';
}

class Ctx {
  constructor(o) { Object.assign(this, o); }
  taskDir(id) { return path.join(this.runDir, 'tasks', id); }
  /** Agent-visible scratch (workspace, prompt file). */
  scratch(id) { return path.join(this.scratchRoot, id); }
  /** Scoring copies live under a root the agent sandbox denies (they hold the restored tests). */
  scoreScratch(id) { return path.join(this.scoreRoot, id); }
}

async function prepareAttemptState(ctx, task) {
  const suite = ctx.suite;
  const scratch = ctx.scratch(task.id);
  await rmrf(scratch);
  const workspace = path.join(scratch, 'ws');
  const { snapshot } = await suite.makeWorkspace(task, workspace);
  const hidden = await hiddenIndex(suite.hiddenFiles(task));
  return { scratch, scoreScratch: ctx.scoreScratch(task.id), workspace, snapshot, hidden };
}

async function afterAttempt(ctx, task, st, attemptDir, tag, state = null, extra = {}) {
  const integrity = await checkHiddenAbsent(st.workspace, st.hidden);
  await writeJson(path.join(attemptDir, 'hidden_check.json'), integrity);
  // A suite whose workspace is not a plain snapshot repository records its own diff (SWE-bench: the
  // diff against the base commit, or a baseline's submitted patch).
  const diff = ctx.suite.diff ? await ctx.suite.diff(task, st) : await workspaceDiff(st.workspace, st.snapshot);
  await writeText(path.join(attemptDir, 'diff.patch'), diff);
  // `extra` (CortexCode only): the attempt's journals, turn record and frame client, for suites that
  // score the conversation itself (skillsel) rather than the workspace.
  const score = await ctx.suite.score(task, st.workspace, path.join(attemptDir, 'score'), tag, st.scoreScratch, state, { runDir: ctx.runDir, attemptDir, ...extra });
  return { integrity, diff, score };
}

/** Attempts a task may take: the suite's tries, or the multi-step budget of a sequential suite. */
function maxTries(suite, task) {
  return suite.sequential ? suite.sequential.maxTries(task) : suite.tries;
}

/**
 * Wall-clock budget of one task. Suites whose official harness sets per-task timeouts
 * (Terminal-Bench) size it per task; --timeout-min overrides it everywhere. `whole` = the budget
 * covers the whole baseline run (environment build and grading included).
 */
export function taskBudgetMs(ctx, task, { whole = false } = {}) {
  // CortexCode has no time limit unless one is asked for (--timeout-min): a task ends when the
  // agent ends it or the owner stops it in the window (owner, 2026-10-15).
  if (String(ctx.systemKey).startsWith('cortex') && ctx.timeoutOverrideMs == null) return Infinity;
  if (ctx.suite.taskTimeoutMs) return ctx.suite.taskTimeoutMs(task, { overrideMs: ctx.timeoutOverrideMs ?? null, whole });
  return ctx.timeoutMs;
}

/** An attempt passes when the score passes and integrity holds; a timeout fails it unless the
 *  suite's protocol grades the state the agent left (Terminal-Bench: tests run after the timeout). */
function attemptPassed(ctx, score, timedOut, integrity) {
  return !!score.passed && (!timedOut || !!ctx.suite.scoreAfterTimeout) && integrity.ok;
}

async function finishTask(ctx, task, taskDir, rec) {
  rec.ended_ms = Date.now();
  rec.wall_ms = rec.ended_ms - rec.started_ms;
  await writeJson(path.join(taskDir, 'result.json'), rec);
  if (!ctx.keepWorkspaces) await rmrf(ctx.scratch(task.id));
  await rmrf(ctx.scoreScratch(task.id));
  return rec;
}

/** One task through CortexCode. */
async function runCortexTask(ctx, task) {
  const taskDir = ctx.taskDir(task.id);
  await rmrf(taskDir);
  await ensureDir(taskDir);
  const rec = { task_id: task.id, suite: ctx.suite.name, system: ctx.systemKey, model: ctx.model, lang: task.lang, run: ctx.run, started_ms: Date.now(), attempts: [], timeout_ms: ctx.timeoutMs };
  let st;
  try {
    st = await prepareAttemptState(ctx, task);
    // Container suites give the workspace a container first (container.attach seeds it from the
    // image); the agent's budget starts once its environment exists.
    const attached = ctx.suite.attach
      ? await ctx.suite.attach({ client: ctx.system.client, task, workspace: st.workspace, scratch: st.scratch, taskDir, opts: ctx.suiteOpts, timeoutOverrideMs: ctx.timeoutOverrideMs })
      : null;
    if (attached) rec.environment = attached.record ?? null;
    let opened = await ctx.system.openTask(st.workspace, task.id);
    rec.project_id = opened.project.id;
    rec.session_id = opened.session.id;
    rec.session_ids = [opened.session.id];
    // A suite may set the project up through the product's own APIs before the first turn (the
    // knowledge suite seeds items a scenario starts from).
    if (ctx.suite.afterOpen) await ctx.suite.afterOpen({ client: ctx.system.client, task, opened, workspace: st.workspace });
    const deadline = (attached ? Date.now() : rec.started_ms) + taskBudgetMs(ctx, task);
    // The suite's harness may end the agent phase itself (Terminal-Bench: Harbor's agent timeout).
    const shouldStop = () => (ctx.yieldState.requested ? true : (attached?.shouldEnd?.() ? 'timeout' : false));
    let prompt = attached?.prompt ?? ctx.suite.prompt(task);
    const seq = ctx.suite.sequential;
    let state = seq ? seq.initialState(task) : null;
    const tries = maxTries(ctx.suite, task);
    for (let k = 1; k <= tries; k++) {
      const attemptDir = path.join(taskDir, `attempt-${k}`);
      await writeText(path.join(attemptDir, 'prompt.md'), typeof prompt === 'string' ? prompt : `${prompt.text}\n\n[attached ${prompt.name ?? 'test-output.txt'}]\n${prompt.pasted ?? ''}`);
      const turn = await ctx.system.turn(opened, prompt, deadline, shouldStop);
      // The journal is kept even when the attempt was stopped: a stopped run is reviewed too.
      const journals = await ctx.system.collect(opened.project.id).catch(() => null);
      if (journals) await writeJson(path.join(attemptDir, 'journal.json'), journals);
      if (turn.aborted) { rec.aborted = true; break; }
      if (!journals) throw new Error(`the journals of ${task.id} could not be read after the turn`);
      const { integrity, diff, score } = await afterAttempt(ctx, task, st, attemptDir, `${ctx.tag}-score`, state, {
        journals, turn, sessionId: opened.session.id, projectId: opened.project.id, client: ctx.system.client,
      });
      const refs = journalPathReferences(journals, st.hidden);
      const attempt = {
        attempt: k, turn, passed: attemptPassed(ctx, score, turn.timed_out, integrity),
        score_passed: !!score.passed, timed_out: turn.timed_out, integrity_ok: integrity.ok,
        diff_bytes: diff.length, path_references: refs.length, ...(seq ? { step: seq.stepInfo(state) } : {}),
      };
      await writeJson(path.join(attemptDir, 'attempt.json'), attempt);
      rec.attempts.push(attempt);
      // The verdict shows in the app: a note in the session and the title's ✓ / ✗ (the hidden
      // tests decide; nothing in the app knows otherwise).
      await markVerdict(ctx.system.client, {
        sessionId: opened.session.id, taskId: task.id, attempt: k, passed: attempt.passed, output: score.output ?? score.feedback,
        secs: turn.ended_ms && turn.started_ms ? (turn.ended_ms - turn.started_ms) / 1000 : null, final: attempt.passed || k === tries,
      });
      rec.last = { score, integrity, terminal: turn.terminal, diffEmpty: diff.length === 0 };
      if (seq) {
        // Sequential suites decide the next prompt themselves (next step, retry, or stop).
        if (turn.timed_out || !integrity.ok || k === tries) break;
        const nx = await seq.advance(task, state, { ...score, passed: attempt.passed }, st.workspace);
        if (!nx) break;
        state = nx.state;
        prompt = nx.prompt;
        // A scenario may continue in a new conversation of the same project (what a later session
        // knows is the knowledge suite's measure); the task record keeps every session.
        if (nx.new_session && ctx.system.newSession) {
          const session = await ctx.system.newSession(opened.project, `${task.id} ${k + 1}`);
          opened = { project: opened.project, session };
          rec.session_ids.push(session.id);
        }
        continue;
      }
      if (attempt.passed || turn.timed_out || !integrity.ok || k === tries) break;
      const next = ctx.suite.followup(task, score);
      if (!next) break;
      prompt = next;
    }
    const journals = await ctx.system.collect(opened.project.id);
    await writeJson(path.join(taskDir, 'journal.json'), journals);
    rec.usage = summarizeJournals(journals);
  } catch (error) {
    rec.error = String(error?.stack ?? error);
    log(`task ${task.id} harness error: ${error}`);
  } finally {
    await releaseEnvironment(ctx, task, st, rec, ctx.system.client);
  }
  return finishCommon(ctx, task, taskDir, rec);
}

/** Give back a container suite's environment (container.detach, harness processes) before the workspace goes. */
async function releaseEnvironment(ctx, task, st, rec, client) {
  if (!st || !ctx.suite.release) return;
  try {
    rec.release = (await ctx.suite.release({ client, task, workspace: st.workspace })) ?? null;
  } catch (error) {
    rec.release = { error: String(error?.message ?? error) };
    log(`task ${task.id}: releasing its environment failed: ${error}`);
  }
}

async function finishCommon(ctx, task, taskDir, rec) {
  if (rec.aborted) {
    // Stopped mid-task: the attempt is discarded (kept for the record) and re-run.
    const keep = path.join(ctx.runDir, 'yielded', task.id, String(Date.now()));
    await ensureDir(path.dirname(keep));
    await fsp.rename(taskDir, keep).catch(() => {});
    if (!ctx.keepWorkspaces) await rmrf(ctx.scratch(task.id));
    await rmrf(ctx.scoreScratch(task.id));
    return rec;
  }
  const last = rec.last ?? {};
  const final = rec.attempts.at(-1);
  rec.passed = !!final?.passed && !rec.error;
  rec.pass_1 = !!rec.attempts[0]?.passed && !rec.error;
  rec.pass_2 = rec.passed;
  rec.failure_class = failureClass({
    passed: rec.passed, timedOut: !!final?.timed_out, error: rec.error, integrity: last.integrity,
    diffEmpty: last.diffEmpty, score: last.score, terminal: last.terminal,
  });
  delete rec.last;
  return finishTask(ctx, task, taskDir, rec);
}

/**
 * One task through a baseline (aider / mini-swe-agent) runner process. The harness scores each
 * try and answers on the control channel, so the try protocol is identical to CortexCode's.
 */
async function runBaselineTask(ctx, task) {
  const taskDir = ctx.taskDir(task.id);
  await rmrf(taskDir);
  await ensureDir(taskDir);
  const rec = { task_id: task.id, suite: ctx.suite.name, system: ctx.systemKey, model: ctx.model, lang: task.lang, run: ctx.run, started_ms: Date.now(), attempts: [], timeout_ms: ctx.timeoutMs };
  let st;
  let child;
  try {
    st = await prepareAttemptState(ctx, task);
    const msgFile = path.join(st.scratch, 'message.md');
    const prompt = ctx.suite.prompt(task);
    await writeText(msgFile, prompt);
    await writeText(path.join(taskDir, 'attempt-1', 'prompt.md'), prompt);
    const tries = maxTries(ctx.suite, task);
    const seq = ctx.suite.sequential;
    let state = seq ? seq.initialState(task) : null;
    let args;
    // Container suites run a baseline through the benchmark's own runner for it (mini-swe-agent's
    // SWE-bench flow, Harbor's Terminus-2), under the same control channel.
    const spec = ctx.suite.baselineCommand
      ? await ctx.suite.baselineCommand({
        system: ctx.system.kind, task, workspace: st.workspace, scratch: st.scratch, taskDir, msgFile, tries,
        endpoint: ctx.endpoint, metadataFile: ctx.system.metadataFile, opts: ctx.suiteOpts, timeoutOverrideMs: ctx.timeoutOverrideMs,
      })
      : null;
    if (spec) {
      args = spec.args;
    } else if (ctx.system.kind === 'aider') {
      const files = ctx.suite.aiderFiles?.(task);
      args = [path.join(EVAL_DIR, 'baselines', 'aider_runner.py'), '--workspace', st.workspace,
        '--model', `openai/${ctx.endpoint.model}`, '--edit-format', ctx.system.aiderEditFormat,
        '--mode', files ? 'polyglot' : 'message', '--message-file', msgFile, '--tries', String(tries), ...(seq ? ['--sequential'] : []),
        '--history', path.join(taskDir, 'aider.chat.history.md'), '--model-metadata', ctx.system.metadataFile,
        ...(files ? ['--fnames', ...files.fnames, '--ignore', ...files.ignore] : [])];
    } else {
      args = [path.join(EVAL_DIR, 'baselines', 'mini_runner.py'), '--workspace', st.workspace,
        '--model', `openai/${ctx.endpoint.model}`, '--api-base', ctx.endpoint.base_url,
        '--message-file', msgFile, '--trajectory', path.join(taskDir, 'trajectory.json'),
        '--tries', String(tries), '--file-list', task.file_list ?? ''];
    }
    const python = spec?.python ?? py(ctx.system.kind === 'aider' ? T.aider : T.mini);
    const env = {
      ...process.env, ...ctx.agentEnv, ...(spec?.env ?? {}),
      OPENAI_API_BASE: ctx.endpoint.base_url, OPENAI_API_KEY: 'local',
      LITELLM_LOCAL_MODEL_COST_MAP: 'True', AIDER_ANALYTICS: 'false', AIDER_CHECK_UPDATE: 'false',
      MSWEA_COST_TRACKING: 'ignore_errors', MSWEA_CONFIGURED: 'true', HF_HUB_OFFLINE: '1', PYTHONUNBUFFERED: '1',
      // Isolate the baselines from the user's own tool configuration (keys, defaults).
      MSWEA_GLOBAL_CONFIG_DIR: path.join(st.scratch, 'mini-config'), MSWEA_SILENT_STARTUP: '1',
      HOME: path.join(st.scratch, 'home'),
    };
    await ensureDir(env.HOME);
    await ensureDir(env.MSWEA_GLOBAL_CONFIG_DIR);
    const logFd = fs.openSync(path.join(taskDir, 'runner.log'), 'a');
    child = spawn(python, args, {
      cwd: st.workspace, env, detached: true, stdio: ['pipe', logFd, logFd, 'pipe'],
    });
    fs.closeSync(logFd);
    const exited = new Promise((resolve) => child.on('exit', (code, signal) => resolve({ code, signal })));
    const lines = lineReader(child.stdio[3]);
    const deadline = rec.started_ms + taskBudgetMs(ctx, task, { whole: true });
    let k = 0;
    let finished = null;
    let timedOut = false;
    while (true) {
      const remaining = deadline - Date.now();
      const msg = await Promise.race([lines.next(), exited.then(() => null), sleepMs(Math.max(0, remaining)).then(() => 'timeout'), ctx.yieldState.promise.then(() => 'yield')]);
      if (msg === 'yield') { rec.aborted = true; break; }
      if (msg === 'timeout') { timedOut = true; break; }
      if (msg === null) break;
      if (msg.event === 'attempt_done') {
        k = msg.attempt;
        const attemptDir = path.join(taskDir, `attempt-${k}`);
        const { integrity, diff, score } = await afterAttempt(ctx, task, st, attemptDir, `${ctx.tag}-score`, state);
        const attempt = { attempt: k, runner: msg, passed: !!score.passed && integrity.ok, score_passed: !!score.passed, timed_out: false, integrity_ok: integrity.ok, diff_bytes: diff.length, ...(seq ? { step: seq.stepInfo(state) } : {}) };
        await writeJson(path.join(attemptDir, 'attempt.json'), attempt);
        rec.attempts.push(attempt);
        rec.last = { score, integrity, diffEmpty: diff.length === 0 };
        if (seq) {
          // Sequential protocol: the reply carries the next prompt verbatim, or null to stop.
          const nx = k < tries && integrity.ok ? await seq.advance(task, state, { ...score, passed: attempt.passed }, st.workspace) : null;
          if (nx) {
            state = nx.state;
            await writeText(path.join(taskDir, `attempt-${k + 1}`, 'prompt.md'), nx.prompt);
          }
          child.stdin.write(JSON.stringify({ passed: attempt.passed, feedback: '', prompt: nx ? nx.prompt : null }) + '\n');
        } else if (k < tries && !attempt.passed) {
          // The runner appends aider's test_failures text itself; the record keeps the full prompt.
          await writeText(path.join(taskDir, `attempt-${k + 1}`, 'prompt.md'), ctx.suite.followup(task, score) ?? '');
          child.stdin.write(JSON.stringify({ passed: false, feedback: score.feedback ?? '' }) + '\n');
        } else {
          child.stdin.write(JSON.stringify({ passed: attempt.passed, feedback: '' }) + '\n');
        }
      } else if (msg.event === 'finished') {
        finished = msg;
      }
    }
    if (rec.aborted) {
      killGroup(child.pid, 'SIGTERM');
      await Promise.race([exited, sleepMs(10000)]);
      killGroup(child.pid, 'SIGKILL');
      return finishCommon(ctx, task, taskDir, rec);
    }
    if (timedOut) {
      killGroup(child.pid, 'SIGTERM');
      await Promise.race([exited, sleepMs(10000)]);
      killGroup(child.pid, 'SIGKILL');
      // Score the workspace as the budget left it, for the record; the task counts as a timeout.
      const attemptDir = path.join(taskDir, `attempt-${k + 1}`);
      const { integrity, diff, score } = await afterAttempt(ctx, task, st, attemptDir, `${ctx.tag}-score`, state);
      const attempt = { attempt: k + 1, passed: false, score_passed: !!score.passed, timed_out: true, integrity_ok: integrity.ok, diff_bytes: diff.length, ...(seq ? { step: seq.stepInfo(state) } : {}) };
      await writeJson(path.join(attemptDir, 'attempt.json'), attempt);
      rec.attempts.push(attempt);
      rec.last = { score, integrity, diffEmpty: diff.length === 0 };
    }
    const ex = await Promise.race([exited, sleepMs(5000).then(() => null)]);
    rec.runner = { finished, exit: ex };
    if (!rec.attempts.length) rec.error = `runner exited without completing an attempt (${JSON.stringify(ex)}); see runner.log`;
    rec.usage = ctx.system.kind === 'mini'
      ? { ...trajectoryUsage(await readJson(path.join(taskDir, 'trajectory.json'), null)), cached_tokens: NaN, prefill_ms: NaN, decode_ms: NaN }
      : baselineUsage(finished, rec.attempts);
  } catch (error) {
    rec.error = String(error?.stack ?? error);
    log(`task ${task.id} harness error: ${error}`);
  } finally {
    if (child) killGroup(child.pid, 'SIGKILL');
    await releaseEnvironment(ctx, task, st, rec, null);
  }
  return finishCommon(ctx, task, taskDir, rec);
}

function baselineUsage(finished, attempts) {
  const last = finished ?? attempts.at(-1)?.runner ?? {};
  return { prompt_tokens: Number(last.prompt_tokens ?? NaN), completion_tokens: Number(last.completion_tokens ?? NaN), cached_tokens: NaN, prefill_ms: NaN, decode_ms: NaN };
}

function sleepMs(ms) {
  return new Promise((r) => setTimeout(r, ms));
}

function lineReader(stream) {
  let buf = '';
  const queue = [];
  const waiters = [];
  let ended = false;
  stream.setEncoding('utf8');
  stream.on('data', (d) => {
    buf += d;
    let i;
    while ((i = buf.indexOf('\n')) >= 0) {
      const line = buf.slice(0, i);
      buf = buf.slice(i + 1);
      if (!line.trim()) continue;
      let msg;
      try { msg = JSON.parse(line); } catch { continue; }
      if (waiters.length) waiters.shift()(msg); else queue.push(msg);
    }
  });
  stream.on('end', () => { ended = true; while (waiters.length) waiters.shift()(null); });
  return {
    next() {
      if (queue.length) return Promise.resolve(queue.shift());
      if (ended) return Promise.resolve(null);
      return new Promise((r) => waiters.push(r));
    },
  };
}

function makeYieldState() {
  const st = { requested: false, at: null };
  st.promise = new Promise((resolve) => { st.fire = (why) => { if (!st.requested) { st.requested = true; st.at = Date.now(); st.why = why; resolve(); } }; });
  return st;
}

/**
 * Run one suite for one system/model (and edit format), `runs` times, resuming completed tasks.
 *
 * It starts at once: nothing waits for the machine.
 */
export async function runSuite(o) {
  const suite = SUITES[o.suite];
  if (!suite) throw new Error(`unknown suite ${o.suite}`);
  if (!suiteSystems(suite).includes(o.system)) {
    throw new Error(suite.unsupportedSystems?.[o.system] ?? `${o.suite} runs the systems ${suiteSystems(suite).join(', ')}, not ${o.system}`);
  }
  const suiteOpts = o.suiteOpts ?? {};
  const tasks = await suite.listTasks({ limit: o.limit, ids: o.ids, lang: o.lang, group: o.group, since: o.since, until: o.until, suiteOpts });
  const resultsRoot = o.resultsDir ?? RESULTS;
  const timeoutMs = o.timeoutMs ?? (o.system === 'cortex' ? null : suite.defaultTimeoutMs);
  const env = agentEnv(suite);
  const sysKey = o.system === 'cortex' && o.editFormat ? `cortex-${o.editFormat}` : o.system;
  const tag = safeTag(`${o.suite}-${sysKey}-${o.tagSuffix ?? Date.now().toString(36)}`);
  const frameDir = path.join(tmpRoot(), `cceval-${tag}`);
  const deny = hiddenPaths([path.join(frameDir, 'score')]);
  const summaries = [];
  for (let r = 1; r <= (o.runs ?? 1); r++) {
    const runDir = path.join(resultsRoot, runKey({ suite: o.suite, system: o.system, model: o.model, editFormat: o.editFormat, run: r }));
    await ensureDir(runDir);
    const prev = await readJson(path.join(runDir, 'run.json'), null);
    if (prev && JSON.stringify(prev.tasks) !== JSON.stringify(tasks.map((t) => t.id))) {
      throw new Error(`${runDir} holds a run over a different task set; use a different --results-dir`);
    }
    const segments = prev?.segments ?? [];
    for (;;) {
      const todo = [];
      for (const t of tasks) {
        const done = await readJson(path.join(runDir, 'tasks', t.id, 'result.json'), null);
        // Finished tasks are kept.
        if (done && done.failure_class !== 'harness_error') continue;
        todo.push(t);
      }
      if (!todo.length) break;
      await rmrf(frameDir);
      await ensureDir(frameDir);
      const system = new CortexSystem({
        runDir: frameDir, model: o.model, editFormat: o.system === 'cortex' ? o.editFormat ?? null : null,
        tag, denyRead: deny, denyWrite: readOnlyCaches(), env, categories: o.system === 'cortex' ? null : ['model'],
      });
      const yieldState = makeYieldState();
      try {
        log(`starting frame for ${o.suite}/${sysKey} (${o.model}) run ${r}`);
        await system.start();
        const slots = system.slots();
        // One task at a time unless asked: the engine interleaves conversations rather than
        // batching them, so parallel tasks each decode slower (measured: ~45 tok/s alone, ~6-7 with
        // four) and run into their wall-clock budgets.
        const parallel = Math.max(1, Math.min(o.parallel ?? 1, slots));
        const endpoint = o.system === 'cortex' ? null : await system.endpoint();
        let profile = null;
        let metadataFile = null;
        if (o.system !== 'cortex') {
          metadataFile = path.join(frameDir, 'model-metadata.json');
          const ctxTokens = Number(system.modelInfo?.ctx_total) || Number(system.modelInfo?.ctx_native) || 32768;
          const perSlot = Math.floor(ctxTokens / parallel);
          await writeJson(metadataFile, { [`openai/${endpoint.model}`]: { max_input_tokens: perSlot, max_output_tokens: perSlot, max_tokens: perSlot, input_cost_per_token: 0, output_cost_per_token: 0, litellm_provider: 'openai', mode: 'chat' } });
        }
        const segment = {
          started: new Date().toISOString(), frame_pid: system.frame.proc.pid, parallel, slots, todo: todo.length,
        };
        segments.push(segment);
        const runMeta = {
          suite: o.suite, system: o.system, system_key: sysKey, model: o.model, edit_format: o.editFormat ?? null, run: r,
          tasks: tasks.map((t) => t.id), task_count: tasks.length, timeout_ms: timeoutMs, parallel, slots,
          model_info: system.modelInfo, plugins: system.frame.pluginInfo, endpoint,
          tool_schemas: o.system === 'cortex' ? await system.toolSchemas(o.editFormat) : null,
          aider_edit_format: o.system === 'aider' ? o.aiderEditFormat ?? 'diff' : null,
          sandbox_tag: tag, started: prev?.started ?? new Date().toISOString(), limit: o.limit ?? null,
          suite_opts: suiteOpts, timeout_override_ms: o.timeoutMs ?? null,
          segments, harness: { node: process.version },
        };
        await writeJson(path.join(runDir, 'run.json'), runMeta);
        await fsp.copyFile(system.frame.configPath, path.join(runDir, 'frame.toml')).catch(() => {});
        const ctx = new Ctx({
          suite, system: o.system === 'cortex' ? system : { kind: o.system, aiderEditFormat: o.aiderEditFormat ?? 'diff', metadataFile },
          systemKey: sysKey, model: o.model, run: r, runDir, tag, timeoutMs, parallel, endpoint, profile,
          agentEnv: env, scratchRoot: path.join(frameDir, `ws-${r}`), scoreRoot: path.join(frameDir, 'score', `run-${r}`),
          keepWorkspaces: !!o.keepWorkspaces, yieldState, suiteOpts, timeoutOverrideMs: o.timeoutMs ?? null,
        });
        log(`${o.suite}/${sysKey}/${o.model} run ${r}: ${tasks.length - todo.length} done, ${todo.length} to run, ${parallel} parallel`);
        await pool(todo, parallel, async (t) => {
          if (yieldState.requested) return null;
          const rec = o.system === 'cortex' ? await runCortexTask(ctx, t) : await runBaselineTask(ctx, t);
          if (rec.aborted) { log(`${t.id}: discarded (the run was stopped)`); return null; }
          log(`${t.id}: ${rec.passed ? 'PASS' : 'fail'}${rec.failure_class ? ` (${rec.failure_class})` : ''} ${Math.round(rec.wall_ms / 1000)}s`);
          return rec;
        });
        segment.ended = new Date().toISOString();
        if (yieldState.requested) segment.yielded = { at: new Date(yieldState.at).toISOString(), why: yieldState.why ?? null };
        await writeJson(path.join(runDir, 'run.json'), runMeta);
        // Raw frame and plugin logs of this segment (frame.log, <category>.log).
        const logDir = path.join(runDir, `logs-${Date.now()}`);
        await ensureDir(logDir);
        await fsp.copyFile(system.frame.logPath, path.join(logDir, 'frame.log')).catch(() => {});
        await fsp.cp(path.join(system.frame.home, 'logs'), path.join(logDir, 'plugins'), { recursive: true }).catch(() => {});
      } finally {
        await system.stop();
      }
      if (yieldState.requested) {
        log(`stopped during ${o.suite}/${sysKey} run ${r}; resuming`);
        continue;
      }
      break;
    }
    const results = await Promise.all(tasks.map((t) => readJson(path.join(runDir, 'tasks', t.id, 'result.json'), null)));
    const complete = results.every((x) => x && x.failure_class !== 'harness_error');
    await writeJson(path.join(runDir, 'status.json'), { complete, finished: new Date().toISOString(), passed: results.filter((x) => x?.passed).length, total: tasks.length, harness_errors: results.filter((x) => x?.failure_class === 'harness_error').length });
    // A suite's own per-run summary (skillsel: summary.md), from the files just written.
    if (suite.afterRun) {
      try { await suite.afterRun({ runDir, tasks }); } catch (error) { log(`${o.suite} run ${r}: summary failed: ${error?.stack ?? error}`); }
    }
    summaries.push({ runDir, complete, passed: results.filter((x) => x?.passed).length, total: tasks.length });
  }
  return summaries;
}

export { sha256 };
