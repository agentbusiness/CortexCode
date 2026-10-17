// Terminal-Bench 2.0 (terminal-bench@2.0 in the Harbor registry: 89 tasks of
// laude-institute/terminal-bench-2), run by Harbor, its official harness (harbor 0.24.0).
//
// A task is a Docker environment (environment/Dockerfile and a prebuilt image), an instruction, and
// tests (tests/test.sh) that Harbor runs in the same container after the agent and that write the
// reward. Each harness task is one Harbor trial (`harbor trial start -p <task>`), so Harbor itself
// starts the environment, enforces the task's agent and verifier timeouts, runs the verifier and
// writes result.json; this suite reads the reward from it.
//
// CortexCode plugs in as a custom Harbor agent plus the environment it needs
// (harbor/cceval_tb.py): Harbor's environment start() asks this harness to `container.attach` the
// task image (workdir = the image's WORKDIR, seed_from_image), so the container is the one
// CortexCode's container plugin owns: the workdir's files are on the host (CortexCode edits them
// there) and every command, and later every test, runs inside that container, where all state
// outside the workdir lives. The agent's run() hands the instruction over; this harness sends it
// through chatflow.turn (lib/runner.mjs) and answers when the turn is over or Harbor's agent timeout
// cancelled it. The channel is JSON lines on fds 3 (requests from Harbor's process) and 4 (replies).
//
// Baseline: Harbor's reference agent Terminus-2 on the same endpoint (baselines/tb_terminus_runner.py),
// in Harbor's own docker environment.
import fs from 'node:fs';
import fsp from 'node:fs/promises';
import path from 'node:path';
import { spawn } from 'node:child_process';
import { fileURLToPath } from 'node:url';
import { HIDDEN } from '../../lib/paths.mjs';
import { dockerEnv, requireContainerPlugin } from '../../lib/docker.mjs';
import { parseToml } from '../../lib/toml.mjs';
import { py, T } from '../../lib/toolchains.mjs';
import { ensureDir, killGroup, readJson, rmrf, writeJson, writeText } from '../../lib/util.mjs';

const HERE = path.dirname(fileURLToPath(import.meta.url));
const EVAL_DIR = path.resolve(HERE, '..', '..');

export const TB = {
  dataset: 'terminal-bench@2.0',
  repo: 'https://github.com/laude-institute/terminal-bench-2.git',
  commit: '69671fbaac6d67a7ef0dfec016cc38a64ef7a77c',
  count: 89,
  // The Harbor registry entry that defines terminal-bench@2.0 (task names, repository, commit).
  registry: {
    url: 'https://raw.githubusercontent.com/laude-institute/harbor/f527f8e447038fd242d08ecfa8aaa8029100da97/registry.json',
    revision: 'f527f8e447038fd242d08ecfa8aaa8029100da97',
    sha256: 'da1446bce05eabbd72a25eb9eef5a2f5db94645ce88c28e2497581433b3d2e60',
  },
  harness: 'harbor==0.24.0',
  python: '3.12',
  requirements: path.join(HERE, 'requirements.lock'),
  agent: 'cceval_tb:CortexAgent',
  environment: 'cceval_tb:CortexContainerEnvironment',
  // The harness's own deadline only backs up Harbor's timeouts (Harbor cancels the agent first).
  backstopMs: 15 * 60000,
};

export const TB_DIR = path.join(HIDDEN, 'terminalbench');
export const HARBOR_MODULE_DIR = path.join(HERE, 'harbor');
export const TERMINUS_RUNNER = path.join(EVAL_DIR, 'baselines', 'tb_terminus_runner.py');

export function harborBin() {
  return path.join(T.harbor, 'bin', 'harbor');
}

// ---------------------------------------------------------------------------------------------
// Task metadata

/** Harbor's strip_canary: leading canary comment lines and the blank lines after them. */
export function stripCanary(text) {
  const lines = text.split('\n');
  let i = 0;
  const canary = /^(<!--.*canary.*-->|#.*canary.*)$/i;
  while (i < lines.length && canary.test(lines[i].trim())) i++;
  while (i < lines.length && lines[i].trim() === '') i++;
  return lines.slice(i).join('\n');
}

/** WORKDIR in effect at the end of a Dockerfile's last stage (relative WORKDIRs resolved), or null. */
export function dockerfileWorkdir(text) {
  let wd = null;
  for (const raw of (text ?? '').split('\n')) {
    const line = raw.trim();
    if (/^FROM\s/i.test(line)) { wd = null; continue; }
    const m = line.match(/^WORKDIR\s+(.+)$/i);
    if (!m) continue;
    let v = m[1].trim();
    if (v.startsWith('[')) { try { v = JSON.parse(v)[0]; } catch { /* keep text */ } }
    v = v.replace(/^["']|["']$/g, '');
    wd = v.startsWith('/') ? path.posix.normalize(v) : path.posix.join(wd ?? '/', v);
  }
  return wd;
}

/** A harness task record from a Harbor task directory (task.toml, instruction.md, environment/). */
export async function tbTaskFromDir(dir, name) {
  const cfg = parseToml(await fsp.readFile(path.join(dir, 'task.toml'), 'utf8'));
  const instruction = stripCanary(await fsp.readFile(path.join(dir, 'instruction.md'), 'utf8'));
  let dockerfile = null;
  try { dockerfile = await fsp.readFile(path.join(dir, 'environment', 'Dockerfile'), 'utf8'); } catch { /* prebuilt only */ }
  const env = cfg.environment ?? {};
  const hasCompose = ['docker-compose.yaml', 'docker-compose.yml'].some((f) => fs.existsSync(path.join(dir, 'environment', f)));
  return {
    id: name, suite: 'terminalbench', lang: 'terminal', group: cfg.metadata?.category ?? 'uncategorized', name,
    category: cfg.metadata?.category ?? null, difficulty: cfg.metadata?.difficulty ?? null, tags: cfg.metadata?.tags ?? [],
    agent_timeout_sec: cfg.agent?.timeout_sec ?? null, verifier_timeout_sec: cfg.verifier?.timeout_sec ?? null,
    build_timeout_sec: env.build_timeout_sec ?? null, docker_image: env.docker_image ?? null,
    cpus: env.cpus ?? null, memory: env.memory ?? null, storage: env.storage ?? null,
    workdir: env.workdir ?? dockerfileWorkdir(dockerfile), has_dockerfile: dockerfile !== null, has_compose: hasCompose,
    prompt: instruction, file_list: '',
  };
}

export function harborTaskDir(task, dir = TB_DIR) {
  return path.join(dir, task.id, 'task');
}

/** List tasks of a prepared store (`dir` = eval/cache/hidden/terminalbench or a fixture). */
export async function listTerminalbenchTasks(dir, opts = {}) {
  const m = await readJson(path.join(dir, 'tasks.json'));
  let list = m.tasks.filter((t) => !t.excluded);
  if (opts.ids?.length) list = list.filter((t) => opts.ids.includes(t.id));
  if (opts.group) {
    const groups = String(opts.group).split(',');
    list = list.filter((t) => groups.includes(t.group));
  }
  if (opts.limit) list = list.slice(0, opts.limit);
  return Promise.all(list.map((t) => readJson(path.join(dir, t.id, 'task.json'))));
}

// ---------------------------------------------------------------------------------------------
// Trial results

function ms(t) {
  if (!t?.started_at || !t?.finished_at) return null;
  return Date.parse(t.finished_at) - Date.parse(t.started_at);
}

/**
 * Verdict of one Harbor trial from its result.json (TrialResult). The reward is Harbor's
 * verifier_result.rewards.reward (from /logs/verifier/reward.txt); a trial without one counts as
 * reward 0, as Harbor's mean metric counts it. TB 2.0 rewards are 0 or 1.
 */
export function parseTrialResult(result) {
  if (!result) return { completed: false, reward: null, passed: false, status: 'no_result', failure_class: 'no_result' };
  const rewards = result.verifier_result?.rewards ?? null;
  const values = rewards ? Object.values(rewards) : [];
  const reward = rewards ? (rewards.reward ?? (values.length === 1 ? values[0] : null)) : null;
  const exc = result.exception_info ?? null;
  const agentTimedOut = exc?.exception_type === 'AgentTimeoutError';
  const passed = reward !== null && Number(reward) >= 1;
  let failure = null;
  if (!passed) {
    if (reward === null) failure = exc && !agentTimedOut ? 'trial_error' : 'no_reward';
    else failure = agentTimedOut ? 'timeout' : 'tests_failed';
  }
  return {
    completed: reward !== null, reward, rewards, passed,
    status: passed ? 'passed' : reward === null ? 'no_reward' : 'failed', failure_class: failure,
    agent_timed_out: agentTimedOut, exception_type: exc?.exception_type ?? null,
    exception_message: exc?.exception_message ? String(exc.exception_message).slice(0, 2000) : null,
    timing_ms: { environment_setup: ms(result.environment_setup), agent_setup: ms(result.agent_setup), agent: ms(result.agent_execution), verifier: ms(result.verifier) },
    agent: result.agent_info ?? null, trial_name: result.trial_name ?? null, task_checksum: result.task_checksum ?? null,
  };
}

async function readText(file) {
  try { return await fsp.readFile(file, 'utf8'); } catch { return null; }
}

// ---------------------------------------------------------------------------------------------
// The Harbor trial process behind one CortexCode attempt

function lineReader(stream, onLine) {
  let buf = '';
  stream.setEncoding('utf8');
  stream.on('data', (d) => {
    buf += d;
    let i;
    while ((i = buf.indexOf('\n')) >= 0) {
      const line = buf.slice(0, i).trim();
      buf = buf.slice(i + 1);
      if (!line) continue;
      let msg;
      try { msg = JSON.parse(line); } catch { continue; }
      onLine(msg);
    }
  });
}

export class TrialProcess {
  constructor({ client, task, workspace, trialsDir, trialName, logFile, forceBuild = false, agentTimeoutSec = null }) {
    Object.assign(this, { client, task, workspace, trialsDir, trialName, logFile, forceBuild, agentTimeoutSec });
    this.trialDir = path.join(trialsDir, trialName);
    this.container = null;
    this.attachParams = null;
    this.runMsg = null;
    this.cancelMsg = null;
    this.detached = false;
    this.exit = null;
    this.events = [];
  }

  command() {
    return [harborBin(), 'trial', 'start', '-p', harborTaskDir(this.task), '--agent', TB.agent, '--env', TB.environment,
      '--trials-dir', this.trialsDir, '--trial-name', this.trialName, '--no-delete',
      ...(this.forceBuild ? ['--force-build'] : []), ...(this.agentTimeoutSec ? ['--agent-timeout', String(this.agentTimeoutSec)] : [])];
  }

  async start() {
    await ensureDir(this.trialsDir);
    const env = {
      ...process.env, ...(await dockerEnv()), PYTHONPATH: HARBOR_MODULE_DIR, CCEVAL_TB_CHANNEL: 'fd3-fd4',
      HARBOR_TELEMETRY: '0', PYTHONUNBUFFERED: '1', PYTHONDONTWRITEBYTECODE: '1',
    };
    const cmd = this.command();
    await ensureDir(path.dirname(this.logFile));
    fs.appendFileSync(this.logFile, `$ ${cmd.join(' ')}\n`);
    const logFd = fs.openSync(this.logFile, 'a');
    this.proc = spawn(cmd[0], cmd.slice(1), { cwd: this.trialsDir, env, detached: true, stdio: ['ignore', logFd, logFd, 'pipe', 'pipe'] });
    fs.closeSync(logFd);
    this.exited = new Promise((resolve) => this.proc.on('exit', (code, signal) => { this.exit = { code, signal }; resolve(this.exit); }));
    this.proc.stdio[4].on('error', () => {});
    let runResolve;
    this.runRequested = new Promise((r) => { runResolve = r; });
    lineReader(this.proc.stdio[3], (msg) => {
      this.events.push({ ts: Date.now(), event: msg.event, id: msg.id });
      if (msg.event === 'run') { this.runMsg = msg; runResolve(msg); return; }
      if (msg.event === 'cancel') { this.cancelMsg = msg; return; }
      this.handle(msg).catch((error) => this.reply(msg.id, { ok: false, error: String(error?.message ?? error) }));
    });
  }

  async handle(msg) {
    if (msg.event === 'attach') {
      this.attachParams = {
        root: this.workspace, image: msg.image, workdir: msg.workdir, seed_from_image: true, platform: msg.platform,
        env: msg.env ?? [], network: true,
      };
      this.container = await this.client.request('container.attach', this.attachParams, { timeoutMs: 60 * 60000 });
      if (this.container?.state && !['running', 'starting'].includes(this.container.state)) {
        throw new Error(`container.attach: ${this.container.state}: ${this.container.problem ?? ''}`);
      }
      this.reply(msg.id, { ok: true, container: this.container });
    } else if (msg.event === 'detach') {
      await this.client.request('container.detach', { root: this.workspace }, { timeoutMs: 300000 });
      this.detached = true;
      this.reply(msg.id, { ok: true });
    } else {
      this.reply(msg.id, { ok: false, error: `unknown request ${msg.event}` });
    }
  }

  reply(id, data) {
    try { this.proc.stdio[4].write(JSON.stringify({ id, ...data }) + '\n'); } catch { /* harbor exited */ }
  }

  /** Wait until Harbor's environment is up and the agent asked to run; throws if Harbor ends first. */
  async waitForRun() {
    const first = await Promise.race([this.runRequested, this.exited.then(() => null)]);
    if (first) return first;
    const result = await readJson(path.join(this.trialDir, 'result.json'), null);
    const tail = ((await readText(this.logFile)) ?? '').slice(-3000);
    throw new Error(`harbor trial ended before the agent ran (${JSON.stringify(this.exit)}): ${result?.exception_info?.exception_type ?? ''} ${result?.exception_info?.exception_message ?? ''}\n${tail}`);
  }

  /** The agent phase is over on this side: answer Harbor (cancel acknowledged, or run done). */
  finishAgent() {
    if (this.cancelMsg) this.reply(this.cancelMsg.id, { ok: true });
    else if (this.runMsg) this.reply(this.runMsg.id, { ok: true });
  }

  async waitExit(timeoutMs) {
    let timer;
    const res = await Promise.race([this.exited, new Promise((r) => { timer = setTimeout(() => r(null), timeoutMs); })]);
    clearTimeout(timer);
    return res;
  }

  async kill() {
    if (!this.proc || this.exit) return;
    killGroup(this.proc.pid, 'SIGTERM');
    if (!(await this.waitExit(30000))) killGroup(this.proc.pid, 'SIGKILL');
  }
}

// ---------------------------------------------------------------------------------------------
// Runner adapter

const TRIALS = new Map();

function agentTimeoutSec(task, overrideMs) {
  return overrideMs ? Math.round(overrideMs / 1000) : null;
}

export async function terminalbenchScore(task, workspace, outDir) {
  const started = Date.now();
  await ensureDir(outDir);
  const st = TRIALS.get(workspace);
  if (!st) throw new Error(`no Harbor trial recorded for ${workspace}`);
  if (st.process) {
    st.process.finishAgent();
    // Harbor now runs the verifier (task verifier timeout) and stops the environment.
    const limit = ((task.verifier_timeout_sec ?? 3600) + 1800) * 1000;
    if (!(await st.process.waitExit(limit))) await st.process.kill();
  }
  const result = await readJson(path.join(st.trialDir, 'result.json'), null);
  const verdict = parseTrialResult(result);
  const stdout = await readText(path.join(st.trialDir, 'verifier', 'test-stdout.txt'));
  const output = `${stdout ?? '(no verifier output)'}\n--- harbor log tail ---\n${((await readText(st.logFile)) ?? '').slice(-6000)}`;
  await writeText(path.join(outDir, 'output.txt'), output);
  const res = {
    command: st.command, exit_code: verdict.passed ? 0 : 1, timed_out: false, ms: Date.now() - started,
    ...verdict, harbor_exit: st.process?.exit ?? null, trial_dir: path.relative(outDir, st.trialDir),
    attach: st.process?.attachParams ?? null, container: st.process?.container ?? null,
    channel_events: st.process?.events ?? null, agent_timeout_override_sec: st.agentTimeoutSec ?? null,
    started_ms: started, ended_ms: Date.now(),
  };
  await writeJson(path.join(outDir, 'result.json'), res);
  return { ...res, output };
}

/** Re-derive an attempt's verdict from the Harbor trial's result.json kept in the results. */
export async function terminalbenchRescore(task, attemptDir) {
  const scoreDir = path.join(attemptDir, 'score');
  const res = await readJson(path.join(scoreDir, 'result.json'), null);
  if (!res?.trial_dir) return { ok: false, error: 'missing score result' };
  const result = await readJson(path.join(scoreDir, res.trial_dir, 'result.json'), null);
  const v = parseTrialResult(result);
  return { ok: true, passed: v.passed, reward: v.reward, failure_class: v.failure_class, timed_out: false };
}

export const terminalbench = {
  name: 'terminalbench',
  title: 'Terminal-Bench 2.0 (89 tasks, Harbor)',
  tries: 1,
  defaultTimeoutMs: 60 * 60000,
  systems: ['cortex', 'terminus'],
  unsupportedSystems: {
    aider: 'aider is not a terminal agent; Terminal-Bench baselines run Harbor\'s reference agent Terminus-2 (--system terminus)',
    mini: 'Terminal-Bench baselines run Harbor\'s reference agent Terminus-2 (--system terminus)',
  },
  // Harbor runs the verifier even after its agent timeout and the reward counts (TB 2.0 protocol).
  scoreAfterTimeout: true,
  listTasks: (opts = {}) => listTerminalbenchTasks(TB_DIR, opts),
  prompt: (task) => task.prompt,
  agentPath: () => [],
  agentEnvExtra: () => ({}),
  async makeWorkspace(task, dest) {
    // Empty: container.attach copies the image's workdir here (seed_from_image).
    await rmrf(dest);
    await ensureDir(dest);
    return { snapshot: null };
  },
  hiddenFiles: () => [],

  /**
   * Per-task budget of the harness's own deadline: Harbor's agent timeout (task.toml, or the
   * --timeout-min override, which is passed to Harbor as --agent-timeout) plus a backstop, since
   * Harbor cancels the agent itself; for baselines the whole trial (build + agent + verifier).
   */
  taskTimeoutMs(task, { overrideMs = null, whole = false } = {}) {
    const agent = overrideMs ?? (task.agent_timeout_sec ?? 3600) * 1000;
    const rest = whole ? ((task.build_timeout_sec ?? 600) + (task.verifier_timeout_sec ?? 3600)) * 1000 : 0;
    return agent + rest + TB.backstopMs;
  },

  /** Start the Harbor trial; returns once Harbor's environment is attached and the agent is asked to run. */
  async attach({ client, task, workspace, taskDir, opts, timeoutOverrideMs }) {
    await requireContainerPlugin(client);
    const tp = new TrialProcess({
      client, task, workspace, trialsDir: path.join(taskDir, 'harbor'), trialName: `${task.id}-cortex`.slice(0, 60),
      logFile: path.join(taskDir, 'harbor.log'), forceBuild: opts?.build === 'native', agentTimeoutSec: agentTimeoutSec(task, timeoutOverrideMs),
    });
    TRIALS.set(workspace, { process: tp, trialDir: tp.trialDir, logFile: tp.logFile, command: tp.command(), agentTimeoutSec: tp.agentTimeoutSec });
    await tp.start();
    const runMsg = await tp.waitForRun();
    return {
      prompt: runMsg.instruction,
      record: { harbor_command: tp.command(), trial_dir: path.relative(taskDir, tp.trialDir), attach: tp.attachParams, container: tp.container, prompt_matches_task: runMsg.instruction === task.prompt },
      shouldEnd: () => !!tp.cancelMsg || !!tp.exit,
    };
  },

  async release({ client, workspace }) {
    const st = TRIALS.get(workspace);
    TRIALS.delete(workspace);
    const tp = st?.process;
    if (!tp) return null;
    await tp.kill();
    if (tp.container && !tp.detached && client) {
      await client.request('container.detach', { root: workspace }, { timeoutMs: 300000 });
      return { detached_by: 'harness' };
    }
    return { detached_by: tp.detached ? 'harbor' : null };
  },

  diff: async () => '',
  score: (task, workspace, outDir) => terminalbenchScore(task, workspace, outDir),
  rescore: terminalbenchRescore,
  followup: () => null,

  /** Terminus-2 in Harbor's own docker environment, on the local endpoint. */
  async baselineCommand({ system, task, workspace, taskDir, endpoint, metadataFile, opts, timeoutOverrideMs }) {
    if (system !== 'terminus') throw new Error(terminalbench.unsupportedSystems[system] ?? `terminalbench has no ${system} baseline`);
    const trialsDir = path.join(taskDir, 'harbor');
    const trialName = `${task.id}-terminus`.slice(0, 60);
    const logFile = path.join(taskDir, 'harbor.log');
    const meta = await readJson(metadataFile, {});
    const info = meta[`openai/${endpoint.model}`] ?? {};
    const modelInfo = { max_input_tokens: info.max_input_tokens, max_output_tokens: info.max_output_tokens, max_tokens: info.max_tokens, input_cost_per_token: 0, output_cost_per_token: 0 };
    const sec = agentTimeoutSec(task, timeoutOverrideMs);
    const args = [TERMINUS_RUNNER, '--harbor', harborBin(), '--task-dir', harborTaskDir(task), '--trials-dir', trialsDir, '--trial-name', trialName,
      '--model', `openai/${endpoint.model}`, '--api-base', endpoint.base_url, '--model-info', JSON.stringify(modelInfo), '--log', logFile,
      ...(opts?.build === 'native' ? ['--force-build'] : []), ...(sec ? ['--agent-timeout', String(sec)] : [])];
    TRIALS.set(workspace, { process: null, trialDir: path.join(trialsDir, trialName), logFile, command: args, agentTimeoutSec: sec });
    return { python: py(T.harbor), args, env: { ...(await dockerEnv()), HARBOR_TELEMETRY: '0', OPENAI_API_KEY: 'local' } };
  },

  reportNote: 'Run by Harbor (harbor 0.24.0 `trial start`, the task\'s own agent and verifier timeouts; the verifier runs after an agent timeout and its reward counts, as in TB 2.0). CortexCode works in the task container its container plugin attached (Harbor environment cceval_tb:CortexContainerEnvironment: image = the task\'s prebuilt image, workdir bind-mounted from the host, everything else in the container); Terminus-2 runs in Harbor\'s own docker environment. Pass = reward 1.',
};
