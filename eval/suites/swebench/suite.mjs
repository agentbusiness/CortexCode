// SWE-bench Verified (princeton-nlp/SWE-bench_Verified, 500 instances), graded by the official
// harness (swebench 4.1.0) in the instance's own Docker image.
//
// Per instance:
//   image    the harness's instance image (env + instance layers): the published x86_64 image by
//            default, a native arm64 image where one is available and its gold-patch control passed
//            (suite option images=auto|arm64|x86_64; see images.py, prepare.mjs and the README)
//   agent    CortexCode works on the repository at /testbed through the container plugin:
//            container.attach {image, workdir: /testbed, seed_from_image: true} onto an empty host
//            workspace, so the repository (with its .git at the base commit) is copied out of the
//            image to the host, edited there, and every command runs inside the container
//   prompt   the problem statement in SWE-agent's default instance template (config/default.yaml,
//            formerly anthropic_filemap.yaml), with working_dir = /testbed
//   score    the model patch = `git diff <base_commit>` of the workspace (working tree incl. new
//            files, file modes ignored as the harness does), without the files the instance's
//            test_patch touches and without files that already differed right after seeding and
//            that the agent left untouched; then the harness's run_instance for that instance
//            (evaluate.py) and its report.json: resolved / unresolved
// Baselines: mini-swe-agent with its official SWE-bench config (baselines/mini_swebench_runner.py);
// aider is not an agent harness for SWE-bench (no shell, no environment) and is not run.
import { createHash } from 'node:crypto';
import fsp from 'node:fs/promises';
import os from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { HIDDEN } from '../../lib/paths.mjs';
import { dockerEnv, platformOf, requireContainerPlugin } from '../../lib/docker.mjs';
import { py, T } from '../../lib/toolchains.mjs';
import { ensureDir, exists, readJson, rmrf, run, writeJson, writeText } from '../../lib/util.mjs';

const HERE = path.dirname(fileURLToPath(import.meta.url));
const EVAL_DIR = path.resolve(HERE, '..', '..');

export const SWEBENCH = {
  dataset: 'princeton-nlp/SWE-bench_Verified',
  revision: 'c104f840cc67f8b6eec6f759ebc8b2693d585d4a',
  file: { name: 'data/test-00000-of-00001.parquet', sha256: 'a45b1fe4e2f0c8390b2b2938ac83e92ed5979000856808f3679c07812e9e6dcd' },
  count: 500,
  harness: 'swebench==4.1.0',
  python: '3.12',
  requirements: path.join(HERE, 'requirements.lock'),
  // run_evaluation's --timeout default (seconds per instance test run).
  testTimeoutSec: 1800,
  workdir: '/testbed',
  // The prompt template: SWE-agent config/default.yaml (instance_template), pinned.
  promptSource: {
    url: 'https://raw.githubusercontent.com/SWE-agent/SWE-agent/3ea751c087f32b16e039a2233dd6eefecef325d5/config/default.yaml',
    revision: '3ea751c087f32b16e039a2233dd6eefecef325d5',
    sha256: '96aeb863cbfaa768044527155f8555c9b401c6644e937b5b6b0bba5538b6eee4',
  },
};

export const SB_DIR = path.join(HIDDEN, 'swebench');
export const EVALUATE_PY = path.join(HERE, 'evaluate.py');
export const IMAGES_PY = path.join(HERE, 'images.py');
export const IMAGES_LOCK = path.join(HERE, 'images.lock.json');
export const MINI_RUNNER = path.join(EVAL_DIR, 'baselines', 'mini_swebench_runner.py');

/** SWE-agent config/default.yaml `instance_template`, verbatim (prepare.mjs checks it against the pinned file). */
export const SWE_AGENT_INSTANCE_TEMPLATE = "<uploaded_files>\n{{working_dir}}\n</uploaded_files>\nI've uploaded a python code repository in the directory {{working_dir}}. Consider the following PR description:\n\n<pr_description>\n{{problem_statement}}\n</pr_description>\n\nCan you help me implement the necessary changes to the repository so that the requirements specified in the <pr_description> are met?\nI've already taken care of all changes to any of the test files described in the <pr_description>. This means you DON'T have to modify the testing logic or any of the tests in any way!\nYour task is to make the minimal changes to non-tests files in the {{working_dir}} directory to ensure the <pr_description> is satisfied.\nFollow these steps to resolve the issue:\n1. As a first step, it might be a good idea to find and read code relevant to the <pr_description>\n2. Create a script to reproduce the error and execute it with `python <filename.py>` using the bash tool, to confirm the error\n3. Edit the sourcecode of the repo to resolve the issue\n4. Rerun your reproduce script and confirm that the error is fixed!\n5. Think about edgecases and make sure your fix handles them as well\nYour thinking should be thorough and so it's fine if it's very long.";

export function swebenchPrompt(instance, workdir = SWEBENCH.workdir) {
  // Jinja substitution of the two variables, as SWE-agent renders the template.
  return SWE_AGENT_INSTANCE_TEMPLATE.split('{{working_dir}}').join(workdir).split('{{problem_statement}}').join(instance.problem_statement);
}

// ---------------------------------------------------------------------------------------------
// Images

/** Where each image source comes from: [namespace, arch] as the harness's make_test_spec takes them. */
export const IMAGE_SOURCES = {
  'hub-x86_64': { namespace: 'swebench', arch: 'x86_64' },
  'hub-arm64': { namespace: 'swebench', arch: 'arm64' },
  'local-arm64': { namespace: null, arch: 'arm64' },
};

/** Docker-compatible instance name of the published images (TestSpec.instance_image_key). */
export function dockerName(instanceId) {
  return instanceId.toLowerCase().replace(/__/g, '_1776_');
}

/** The harness's instance image key for an instance and source (swebench 4.1.0 TestSpec). */
export function imageRef(instanceId, source) {
  const { namespace, arch } = IMAGE_SOURCES[source] ?? {};
  if (!arch) throw new Error(`unknown image source ${source}`);
  const key = `sweb.eval.${arch}.${instanceId.toLowerCase()}:latest`;
  return namespace ? `${namespace}/${key}`.replace(/__/g, '_1776_') : key;
}

/**
 * Image source for one instance under a policy (suite option `images`):
 *   x86_64  the published x86_64 image (what every leaderboard run uses; emulated on Apple Silicon)
 *   arm64   a native arm64 image: the published one when it exists, else one the harness builds
 *   auto    (default) the arm64 image prepare validated for this instance (its gold patch resolves
 *           under the harness in that image), else the published x86_64 image
 */
export function chooseImageSource(instanceId, policy, { hubArm64 = new Set(), validated = {} } = {}) {
  const p = policy ?? 'auto';
  if (p === 'x86_64') return 'hub-x86_64';
  if (p === 'arm64') return hubArm64.has(dockerName(instanceId)) ? 'hub-arm64' : 'local-arm64';
  if (p !== 'auto') throw new Error(`swebench: unknown images policy ${p} (auto|arm64|x86_64)`);
  return validated[instanceId]?.selected ?? 'hub-x86_64';
}

let imageIndexCache = null;
export async function imageIndex() {
  if (imageIndexCache) return imageIndexCache;
  const lock = await readJson(IMAGES_LOCK, { arm64: {}, x86_64: {} });
  const validated = await readJson(path.join(SB_DIR, 'images.json'), {});
  imageIndexCache = { lock, hubArm64: new Set(Object.keys(lock.arm64 ?? {})), validated };
  return imageIndexCache;
}

/** Make the image available (pull or harness build) through images.py; throws when it cannot. */
export async function ensureImage(task, source, { logRoot = path.join(SB_DIR, 'image-logs') } = {}) {
  const res = await run(py(T.swebench), ['-I', IMAGES_PY, '--instance', instanceFile(task), '--source', source, '--log-root', logRoot], {
    env: { ...process.env, ...(await dockerEnv()) }, timeoutMs: 4 * 3600 * 1000,
  });
  const out = lastJson(res.stdout);
  if (!out?.ok) throw new Error(`swebench image ${source} for ${task.instance_id} unavailable: ${out?.error ?? res.output.slice(-2000)}`);
  const { lock } = await imageIndex();
  const pinned = IMAGE_SOURCES[source].namespace ? lock[IMAGE_SOURCES[source].arch]?.[dockerName(task.instance_id)] ?? null : null;
  out.pinned_digest = pinned;
  out.digest_match = pinned ? (out.repo_digests ?? []).some((d) => d.endsWith(`@${pinned}`)) : null;
  return out;
}

// ---------------------------------------------------------------------------------------------
// Patches

/** Undo git's C-style path quoting ("a/f\303\266o" -> a/föo). */
export function unquoteGitPath(s) {
  if (!s.startsWith('"')) return s;
  const bytes = [];
  for (let i = 1; i < s.length - 1; i++) {
    const ch = s[i];
    if (ch !== '\\') { bytes.push(...Buffer.from(ch, 'utf8')); continue; }
    const nx = s[++i];
    if (/[0-7]/.test(nx)) { bytes.push(parseInt(s.slice(i, i + 3), 8)); i += 2; continue; }
    bytes.push({ n: 10, t: 9, r: 13, a: 7, b: 8, f: 12, v: 11 }[nx] ?? nx.charCodeAt(0));
  }
  return Buffer.from(bytes).toString('utf8');
}

function stripPrefix(p) {
  const q = unquoteGitPath(p.replace(/\t.*$/, ''));
  return q === '/dev/null' ? null : q.replace(/^[ab]\//, '');
}

/** Paths named by a `diff --git a/X b/Y` header (quoted or not). */
function headerPaths(line) {
  const rest = line.slice('diff --git '.length);
  if (rest.startsWith('"')) {
    const m = rest.match(/^("(?:[^"\\]|\\.)*")\s+(.+)$/);
    return m ? [stripPrefix(m[1]), stripPrefix(m[2])] : [];
  }
  // Unquoted: a/P b/P with the same P unless renamed; split at the middle when symmetric.
  const half = (rest.length - 1) / 2;
  if (Number.isInteger(half) && rest[half] === ' ' && rest.slice(2, half) === rest.slice(half + 3)) return [rest.slice(2, half)];
  const m = rest.match(/^a\/(.+?) b\/(.+)$/);
  return m ? [m[1], m[2]] : [];
}

/** Split a git patch into per-file sections, each with every path it names. */
export function splitPatch(diff) {
  const sections = [];
  let cur = null;
  let inBody = false;
  for (const line of diff.split(/(?<=\n)/)) {
    if (line.startsWith('diff --git ')) {
      cur = { text: '', paths: new Set(headerPaths(line.replace(/\n$/, ''))) };
      sections.push(cur);
      inBody = false;
    } else if (cur) {
      const l = line.replace(/\n$/, '');
      // Only the extended header names paths; hunk lines ("--- x" is a removed "-- x") do not.
      if (l.startsWith('@@') || l.startsWith('GIT binary patch') || l.startsWith('Binary files ')) inBody = true;
      let p = null;
      if (!inBody && (l.startsWith('--- ') || l.startsWith('+++ '))) p = stripPrefix(l.slice(4));
      else if (!inBody && /^(rename|copy) (from|to) /.test(l)) p = unquoteGitPath(l.replace(/^(rename|copy) (from|to) /, ''));
      if (p) cur.paths.add(p);
    } else {
      // Text before the first file header (a mail header or description) has no paths.
      cur = { text: '', paths: new Set(), preamble: true };
      sections.push(cur);
    }
    cur.text += line;
  }
  return sections;
}

/** Every file a patch touches (the test files of an instance come from its test_patch). */
export function patchFiles(patch) {
  const out = new Set();
  for (const s of splitPatch(patch ?? '')) for (const p of s.paths) out.add(p);
  return [...out].sort();
}

/** Drop the sections of `diff` that touch any of `exclude`; returns {patch, excluded}. */
export function filterPatch(diff, exclude) {
  const ex = new Set(exclude);
  const kept = [];
  const excluded = [];
  for (const s of splitPatch(diff)) {
    if (s.preamble) continue;
    if ([...s.paths].some((p) => ex.has(p))) excluded.push(...s.paths);
    else kept.push(s.text);
  }
  return { patch: kept.join(''), excluded: [...new Set(excluded)].sort() };
}

/**
 * `git diff <base>` of the workspace's working tree (tracked changes and new files, .gitignore
 * respected), through a private index so the agent's own index is untouched. File modes are
 * ignored (core.fileMode=false) as the harness's own diffs do: the repository came out of the
 * image, where /testbed is chmod -R 777.
 */
export async function workspacePatch(workspace, base) {
  const idx = path.join(os.tmpdir(), `cceval-sbidx-${process.pid}-${Date.now()}-${Math.random().toString(16).slice(2)}`);
  const env = { ...process.env, GIT_INDEX_FILE: idx };
  const g = (args) => run('git', ['-c', 'core.fileMode=false', '-c', 'safe.directory=*', '-c', 'core.quotePath=true', ...args], { cwd: workspace, env, timeoutMs: 600000 });
  try {
    const rt = await g(['read-tree', base]);
    if (rt.code !== 0) throw new Error(`git read-tree ${base} failed in ${workspace}: ${rt.output.slice(-1000)}`);
    const add = await g(['add', '-A', '.']);
    if (add.code !== 0) throw new Error(`git add failed in ${workspace}: ${add.output.slice(-1000)}`);
    const res = await g(['diff', '--cached', '--no-color', '--no-ext-diff', '--binary', base]);
    if (res.code !== 0) throw new Error(`git diff failed in ${workspace}: ${res.output.slice(-1000)}`);
    return res.stdout;
  } finally {
    await rmrf(idx);
  }
}

/** Content hashes of the files that differ from `base` right after seeding ({path: sha256|null}). */
export async function seedSnapshot(workspace, base) {
  const out = {};
  for (const rel of patchFiles(await workspacePatch(workspace, base))) out[rel] = await fileHash(path.join(workspace, rel));
  return out;
}

/** Paths of a seed snapshot whose content is still exactly what the seed left (the agent did not touch them). */
export async function unchangedSince(workspace, seed) {
  const out = [];
  for (const [rel, hash] of Object.entries(seed)) if ((await fileHash(path.join(workspace, rel))) === hash) out.push(rel);
  return out.sort();
}

async function fileHash(abs) {
  try { return createHash('sha256').update(await fsp.readFile(abs)).digest('hex'); } catch { return null; }
}

// ---------------------------------------------------------------------------------------------
// Grading results

export function lastJson(output) {
  const lines = (output ?? '').trim().split('\n');
  for (let i = lines.length - 1; i >= 0; i--) {
    if (lines[i].startsWith('{')) { try { return JSON.parse(lines[i]); } catch { /* keep looking */ } }
  }
  return null;
}

// swebench.harness.constants (4.1.0)
const APPLY_PATCH_FAIL = '>>>>> Patch Apply Failed';
const APPLY_PATCH_PASS = '>>>>> Applied Patch';

/**
 * Verdict of one harness run from its raw outputs: evaluate.py's summary line, the instance's
 * report.json (written only when the evaluation completed) and run_instance.log.
 * `resolved` is the harness's own verdict (report[instance_id].resolved).
 */
export function parseSwebenchEval({ instanceId, summary = null, report = null, log = null }) {
  const out = { instance_id: instanceId, resolved: false, completed: false, status: null, failure_class: null, patch_applied: null, tests: null, error: summary?.error ?? null };
  if (summary?.status === 'empty_patch') {
    return { ...out, status: 'empty_patch', failure_class: 'empty_patch', patch_applied: false };
  }
  const entry = report?.[instanceId];
  if (entry) {
    out.completed = true;
    out.resolved = !!entry.resolved;
    out.patch_applied = !!entry.patch_successfully_applied;
    const ts = entry.tests_status;
    if (ts) {
      const count = (k) => ({ success: ts[k]?.success?.length ?? 0, failure: ts[k]?.failure?.length ?? 0 });
      out.tests = { FAIL_TO_PASS: count('FAIL_TO_PASS'), PASS_TO_PASS: count('PASS_TO_PASS') };
    }
    out.status = out.resolved ? 'resolved' : 'unresolved';
    out.failure_class = out.resolved ? null : (out.patch_applied ? 'tests_failed' : 'patch_apply_failed');
    return out;
  }
  const text = log ?? '';
  if (text.includes(APPLY_PATCH_FAIL)) {
    return { ...out, status: 'patch_apply_failed', failure_class: 'patch_apply_failed', patch_applied: false };
  }
  if (/Test timed out after \d+ seconds/.test(text)) {
    return { ...out, status: 'test_timeout', failure_class: 'test_timeout', patch_applied: text.includes(APPLY_PATCH_PASS) };
  }
  return { ...out, status: 'eval_error', failure_class: 'eval_error', patch_applied: text.includes(APPLY_PATCH_PASS) || null };
}

async function readText(file) {
  try { return await fsp.readFile(file, 'utf8'); } catch { return null; }
}

// ---------------------------------------------------------------------------------------------
// Task store

export function instanceFile(task) {
  return path.join(SB_DIR, task.id, 'instance.json');
}

/** Round robin over groups (repositories), so --limit samples every repository. */
export function balancedByGroup(list, limit) {
  if (!limit || limit >= list.length) return list;
  const groups = new Map();
  for (const t of list) {
    if (!groups.has(t.group)) groups.set(t.group, []);
    groups.get(t.group).push(t);
  }
  const lists = [...groups.values()];
  const out = [];
  for (let i = 0; out.length < limit; i++) {
    let any = false;
    for (const l of lists) if (i < l.length && out.length < limit) { out.push(l[i]); any = true; }
    if (!any) break;
  }
  return out;
}

/** List tasks of a prepared store (`dir` = eval/cache/hidden/swebench or a fixture). */
export async function listSwebenchTasks(dir, opts = {}) {
  const m = await readJson(path.join(dir, 'tasks.json'));
  let list = m.tasks.filter((t) => !t.excluded);
  if (opts.ids?.length) list = list.filter((t) => opts.ids.includes(t.id));
  if (opts.group) {
    const groups = String(opts.group).split(',');
    list = list.filter((t) => groups.includes(t.group));
  }
  return Promise.all(balancedByGroup(list, opts.limit).map((t) => readJson(path.join(dir, t.id, 'task.json'))));
}

// ---------------------------------------------------------------------------------------------
// Runner adapter

const STATE = new Map();

function modelPatchFile(workspace) {
  // Written by the mini-swe-agent runner (its submission); the agent workspace is next to it.
  return path.join(path.dirname(workspace), 'model.patch');
}

export async function swebenchScore(task, workspace, outDir, tag) {
  const started = Date.now();
  await ensureDir(outDir);
  const st = STATE.get(workspace) ?? {};
  let patch;
  let excluded = [];
  let origin;
  let seedUnchanged = [];
  if (await exists(path.join(workspace, '.git'))) {
    const full = await workspacePatch(workspace, task.base_commit);
    seedUnchanged = await unchangedSince(workspace, st.seed ?? {});
    ({ patch, excluded } = filterPatch(full, task.test_files));
    if (seedUnchanged.length) patch = filterPatch(patch, seedUnchanged).patch;
    origin = 'workspace diff (test_patch files excluded)';
  } else {
    patch = (await readText(modelPatchFile(workspace))) ?? '';
    origin = 'agent submission';
  }
  const patchPath = path.join(outDir, 'model.patch');
  await writeText(patchPath, patch);
  const source = st.source ?? chooseImageSource(task.instance_id, 'auto', await imageIndex());
  const { namespace, arch } = IMAGE_SOURCES[source];
  const runId = `cceval-${tag}-${Date.now().toString(36)}`.replace(/[^A-Za-z0-9_.-]/g, '_');
  const command = [py(T.swebench), '-I', EVALUATE_PY, '--instance', instanceFile(task), '--patch', patchPath, '--model-name', 'cceval',
    '--run-id', runId, '--namespace', namespace ?? 'none', '--arch', arch, '--timeout', String(SWEBENCH.testTimeoutSec), '--log-root', path.join(outDir, 'harness')];
  const res = await run(command[0], command.slice(1), { env: { ...process.env, ...(await dockerEnv()) }, timeoutMs: (SWEBENCH.testTimeoutSec + 3600) * 1000 });
  await writeText(path.join(outDir, 'output.txt'), res.output);
  const summary = lastJson(res.stdout);
  const logDir = summary?.log_dir ?? null;
  const report = logDir ? await readJson(path.join(logDir, 'report.json'), null) : null;
  const log = logDir ? await readText(path.join(logDir, 'run_instance.log')) : null;
  const verdict = parseSwebenchEval({ instanceId: task.instance_id, summary, report, log });
  const result = {
    command, exit_code: res.code, timed_out: res.timedOut || verdict.status === 'test_timeout', ms: Date.now() - started,
    passed: verdict.resolved && !res.timedOut, ...verdict,
    patch_origin: origin, patch_bytes: Buffer.byteLength(patch), excluded_test_files: excluded, excluded_seed_files: seedUnchanged,
    image_source: source, image: imageRef(task.instance_id, source), run_id: runId, log_dir: logDir ? path.relative(outDir, logDir) : null,
    started_ms: started, ended_ms: Date.now(),
  };
  await writeJson(path.join(outDir, 'result.json'), result);
  return { ...result, output: res.output };
}

/** Re-derive an attempt's verdict from the raw harness files kept in the results (report.mjs). */
export async function swebenchRescore(task, attemptDir) {
  const scoreDir = path.join(attemptDir, 'score');
  const res = await readJson(path.join(scoreDir, 'result.json'), null);
  const output = await readText(path.join(scoreDir, 'output.txt'));
  if (!res || output === null) return { ok: false, error: 'missing score result/output' };
  const summary = lastJson(output);
  const logDir = res.log_dir ? path.join(scoreDir, res.log_dir) : null;
  const report = logDir ? await readJson(path.join(logDir, 'report.json'), null) : null;
  const log = logDir ? await readText(path.join(logDir, 'run_instance.log')) : null;
  const v = parseSwebenchEval({ instanceId: task.instance_id, summary, report, log });
  return { ok: true, passed: v.resolved, timed_out: v.status === 'test_timeout', failure_class: v.failure_class, status: v.status };
}

export const swebench = {
  name: 'swebench',
  title: 'SWE-bench Verified (500 instances, official harness)',
  tries: 1,
  defaultTimeoutMs: 60 * 60000,
  // aider has no shell and no environment, so it is not an agent harness for SWE-bench.
  systems: ['cortex', 'mini'],
  unsupportedSystems: { aider: 'aider is not an agent harness for SWE-bench (it cannot run commands in the instance environment); SWE-bench baselines run mini-swe-agent' },
  listTasks: (opts = {}) => listSwebenchTasks(SB_DIR, opts),
  prompt: (task) => task.prompt,
  agentPath: () => [],
  agentEnvExtra: () => ({}),
  async makeWorkspace(task, dest) {
    // Empty: container.attach seeds it from the image's /testbed (repository + .git at base_commit).
    await rmrf(dest);
    await ensureDir(dest);
    return { snapshot: task.base_commit };
  },
  hiddenFiles: () => [],

  /** Image, then container.attach; the deadline of the agent starts after this. */
  async attach({ client, task, workspace, opts }) {
    await requireContainerPlugin(client);
    const index = await imageIndex();
    const source = chooseImageSource(task.instance_id, opts?.images, index);
    const image = await ensureImage(task, source);
    const params = {
      root: workspace, image: image.image, workdir: SWEBENCH.workdir, seed_from_image: true,
      platform: platformOf(image.arch), network: true,
      // The image activates its conda env from /root/.bashrc; mini-swe-agent's SWE-bench config sets
      // the same so that non-interactive shells get it.
      env: [['BASH_ENV', '/root/.bashrc']],
    };
    const info = await client.request('container.attach', params, { timeoutMs: 60 * 60000 });
    STATE.set(workspace, { source, image, info });
    if (info?.state && !['running', 'starting'].includes(info.state)) throw new Error(`container.attach: ${info.state}: ${info.problem ?? ''}`);
    const head = await run('git', ['-c', 'safe.directory=*', 'rev-parse', 'HEAD'], { cwd: workspace });
    if (head.stdout.trim() !== task.base_commit) {
      throw new Error(`seeded workspace HEAD ${head.stdout.trim() || '(none)'} != base_commit ${task.base_commit} (${head.output.slice(-300)})`);
    }
    // Whatever already differs from the base commit before the agent starts (files the image left
    // untracked, or a copy the host filesystem could not represent exactly) is not the agent's work.
    const seed = await seedSnapshot(workspace, task.base_commit);
    STATE.set(workspace, { source, image, info, seed });
    return { prompt: task.prompt, record: { image_source: source, image, attach: params, container: info, seed_differences: Object.keys(seed) } };
  },

  async release({ client, workspace }) {
    const st = STATE.get(workspace);
    STATE.delete(workspace);
    if (st?.info && client) return client.request('container.detach', { root: workspace }, { timeoutMs: 300000 });
    return null;
  },

  /** The attempt's diff record: the full workspace diff (cortex) or the agent's submission (mini). */
  async diff(task, st) {
    if (await exists(path.join(st.workspace, '.git'))) return workspacePatch(st.workspace, task.base_commit);
    return (await readText(modelPatchFile(st.workspace))) ?? '';
  },

  score: (task, workspace, outDir, tag) => swebenchScore(task, workspace, outDir, tag),
  rescore: swebenchRescore,
  followup: () => null,

  /** mini-swe-agent's SWE-bench runner on the same image the harness grades. */
  async baselineCommand({ system, task, workspace, scratch, taskDir, endpoint, opts }) {
    if (system !== 'mini') throw new Error(swebench.unsupportedSystems[system] ?? `swebench has no ${system} baseline`);
    const index = await imageIndex();
    const source = chooseImageSource(task.instance_id, opts?.images, index);
    const image = await ensureImage(task, source);
    STATE.set(workspace, { source, image });
    await writeJson(path.join(taskDir, 'image.json'), { image_source: source, image });
    return {
      python: py(T.mini),
      args: [MINI_RUNNER, '--instance', instanceFile(task), '--image', image.image, '--platform', platformOf(image.arch),
        '--model', `openai/${endpoint.model}`, '--api-base', endpoint.base_url,
        '--trajectory', path.join(taskDir, 'trajectory.json'), '--patch-out', path.join(scratch, 'model.patch')],
      env: await dockerEnv(),
    };
  },

  reportNote: 'Graded by the official SWE-bench harness (swebench 4.1.0 run_instance, report.json `resolved`) in the instance\'s own image; the image (published x86_64, published arm64, or harness-built arm64) is recorded per attempt. CortexCode\'s model patch is the workspace diff against the base commit without the files the test_patch touches; mini-swe-agent\'s is its own submission (its official SWE-bench flow). aider is not run on SWE-bench.',
};

