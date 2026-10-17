// Suite adapters: one interface over every suite so every system runs the same protocol.
import fs from 'node:fs';
import fsp from 'node:fs/promises';
import path from 'node:path';
import { LOCAL_REFS, HIDDEN, SRC } from './paths.mjs';
import { formatSource } from './formatters.mjs';
import { materializeWorkspace, snapshotSha } from './workspace.mjs';
import { offlineEnv } from './toolchains.mjs';
import { readJson, sha256, walk, writeJson, writeText, ensureDir } from './util.mjs';
import { balancedLimit, filterTasks } from './adapter.mjs';
import * as PG from '../suites/polyglot/suite.mjs';
import * as RL from '../suites/realistic/suite.mjs';
import { livecodebench } from '../suites/livecodebench/suite.mjs';
import { bigcodebench } from '../suites/bigcodebench/suite.mjs';
import { rustevo } from '../suites/rustevo/suite.mjs';
import { webbench } from '../suites/webbench/suite.mjs';
import { swebench } from '../suites/swebench/suite.mjs';
import { terminalbench } from '../suites/terminalbench/suite.mjs';
import { skillsel } from '../suites/skillsel/suite.mjs';
import { knowledge } from '../suites/knowledge/suite.mjs';

export { balancedLimit };

const EB_DIR = path.join(HIDDEN, 'editbench');

async function manifestTasks(dir) {
  const m = await readJson(path.join(dir, 'tasks.json'));
  return m.tasks;
}

export const editbench = {
  name: 'editbench',
  tries: 1,
  defaultTimeoutMs: 30 * 60000,
  async listTasks(opts = {}) {
    const list = filterTasks(await manifestTasks(EB_DIR), opts);
    const picked = balancedLimit(list, opts.limit);
    return Promise.all(picked.map((t) => readJson(path.join(EB_DIR, t.id, 'task.json'))));
  },
  prompt: (task) => task.prompt,
  agentPath: () => [],
  async makeWorkspace(task, dest) {
    await materializeWorkspace(path.join(EB_DIR, task.id, 'template'), dest);
    return { snapshot: await snapshotSha(dest) };
  },
  // The answer (original file) is not a test file; it never exists outside the hidden store, and
  // the agent restoring it is the goal. Reads of the hidden store are caught by the sandbox.
  hiddenFiles: () => [],
  async score(task, workspace, outDir) {
    await ensureDir(outDir);
    const started = Date.now();
    const original = await fsp.readFile(path.join(EB_DIR, task.id, 'original', task.path), 'utf8');
    let final = null;
    try { final = await fsp.readFile(path.join(workspace, task.path), 'utf8'); } catch { /* missing */ }
    const result = { path: task.path, file_present: final !== null, exact: false, formatted: false, final_sha256: null };
    if (final !== null) {
      await writeText(path.join(outDir, 'final', task.path), final);
      result.final_sha256 = sha256(final);
      result.exact = final === original;
      const fo = await formatSource(task.path, original, { rustEdition: task.rust_edition ?? '2021' });
      const ff = await formatSource(task.path, final, { rustEdition: task.rust_edition ?? '2021' });
      result.formatter_ok = fo.ok && ff.ok;
      result.formatted = result.exact || (fo.ok && ff.ok && fo.text === ff.text);
      if (ff.ok) result.formatted_final_sha256 = sha256(ff.text);
    }
    // Other files the agent touched (diagnostic only).
    const files = (await walk(workspace, { skipDirs: ['.git'] })).map((f) => f.rel);
    result.other_files = files.filter((f) => f !== task.path);
    result.passed = result.formatted;
    result.ms = Date.now() - started;
    await writeJson(path.join(outDir, 'result.json'), result);
    return { ...result, feedback: null };
  },
  followup: () => null,
};

export const polyglot = {
  name: 'polyglot',
  tries: PG.POLYGLOT.tries,
  defaultTimeoutMs: 60 * 60000,
  async listTasks(opts = {}) {
    const list = filterTasks(await manifestTasks(path.join(HIDDEN, 'polyglot')), opts);
    return Promise.all(balancedLimit(list, opts.limit).map((t) => PG.loadPolyglotTask(t.id)));
  },
  prompt: (task) => task.prompt,
  agentPath: () => PG.agentPath(),
  agentEnvExtra: () => PG.agentEnvExtra(),
  async makeWorkspace(task, dest) {
    // Offline for the model: a `.cortex-offline` marker makes the tools sandbox deny outbound
    // network (the exercises and their tests are public; fetching them is not solving them), and
    // the folders it lists can be read neither by commands nor by the read tool (the hidden tests,
    // the exercise sources with their reference solutions; a model was seen searching / for them).
    await materializeWorkspace(path.join(PG.taskDir(task.id), 'template'), dest, { links: PG.workspaceLinks(task), exclude: ['/.cortex-offline'] });
    await fsp.writeFile(path.join(dest, '.cortex-offline'), [HIDDEN, SRC, ...LOCAL_REFS].map((dir) => `${dir}\n`).join(''));
    return { snapshot: await snapshotSha(dest) };
  },
  hiddenFiles: (task) => [...PG.hiddenFiles(task), ...PG.secretFiles(task)],
  async score(task, workspace, outDir, tag, scratch) {
    const res = await PG.polyglotScore(task, workspace, path.join(scratch, 'score'), outDir, tag);
    // aider: on a test timeout the follow-up text is "Tests timed out!".
    return { ...res, feedback: res.timed_out ? 'Tests timed out!' : res.feedback };
  },
  followup: (task, score) => PG.followupPrompt(task, score.feedback ?? ''),
  aiderFiles: (task) => ({ fnames: task.solution_files, ignore: [...task.test_files, ...task.example_files, 'Cargo.toml', 'CMakeLists.txt'] }),
};

export const realistic = {
  name: 'realistic',
  tries: 1,
  defaultTimeoutMs: 90 * 60000,
  async listTasks(opts = {}) {
    const list = filterTasks(await manifestTasks(RL.REALISTIC_DIR), opts);
    return Promise.all(balancedLimit(list, opts.limit).map((t) => RL.loadRealisticTask(t.id)));
  },
  prompt: (task) => task.prompt,
  agentPath: () => RL.agentPath(),
  async makeWorkspace(task, dest) {
    await materializeWorkspace(RL.templateDir(task), dest, { exclude: RL.workspaceExcludes(task) });
    const snapshot = await snapshotSha(dest);
    await RL.provisionWorkspace(task, dest);
    return { snapshot };
  },
  hiddenFiles: (task) => RL.hiddenFiles(task),
  async score(task, workspace, outDir, tag, scratch) {
    const res = await RL.realisticScore(task, workspace, path.join(scratch, 'score'), outDir, tag);
    return { ...res, feedback: null };
  },
  followup: () => null,
};

export const SUITES = { editbench, polyglot, realistic, livecodebench, bigcodebench, rustevo, webbench, swebench, terminalbench, skillsel, knowledge };

/** Every suite, in report order. `--suite all` means all of them. */
export const ALL_SUITES = Object.keys(SUITES);

export const SUITE_TITLES = {
  editbench: 'Edit-format benchmark (Stencil methodology)',
  polyglot: 'Aider Polyglot',
  realistic: 'Realistic suite (mined from repository history)',
  ...Object.fromEntries([livecodebench, bigcodebench, rustevo, webbench, swebench, terminalbench, skillsel, knowledge].map((s) => [s.name, s.title])),
};

/** Environment for agents (frame/plugins or baseline processes): offline tools plus suite PATH. */
export function agentEnv(suite) {
  const env = { ...offlineEnv(suite.agentPath().filter((p) => fs.existsSync(p))), ...(suite.agentEnvExtra?.() ?? {}) };
  // A suite unsets a default with null (e.g. RustEvo: the workspace's rust-toolchain.toml decides).
  for (const [k, v] of Object.entries(env)) if (v === null) delete env[k];
  return env;
}
