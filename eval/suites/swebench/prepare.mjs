// Prepare SWE-bench Verified: the pinned dataset file, the harness venv (swebench 4.1.0 from a hashed
// lock, uv-managed CPython; never system Python or Homebrew), the task store (one instance.json =
// the dataset row, and one task.json per instance), and a check that the embedded prompt template
// is SWE-agent's pinned one.
//
// Images (opt-in, Docker; `--images`): for each instance, make the harness's image available and,
// with `--validate`, grade the instance's gold patch in it with the harness (positive control):
//   --images x86_64   pull the published x86_64 images (swebench/sweb.eval.x86_64.*)
//   --images arm64    pull the published arm64 image where one exists (281 of 500, images.lock.json),
//                     else let the harness build the arm64 image natively
//   --images all      arm64 as above, and the x86_64 image as the fallback
// eval/cache/hidden/swebench/images.json records per instance what was made, its digest and gold
// result, and `selected`: the arm64 source when its gold patch resolved, which the default image
// policy (images=auto) then uses instead of the emulated x86_64 image.
import fsp from 'node:fs/promises';
import path from 'node:path';
import readline from 'node:readline';
import fs from 'node:fs';
import { fetchPinned } from '../../lib/fetch.mjs';
import { dockerEnv } from '../../lib/docker.mjs';
import { SRC, TOOLCHAINS } from '../../lib/paths.mjs';
import { ensureManagedVenv, py, T } from '../../lib/toolchains.mjs';
import { ensureDir, exists, log, pool, readJson, rmrf, run, runOk, sha256, writeJson } from '../../lib/util.mjs';
import {
  dockerName, ensureImage, EVALUATE_PY, imageIndex, instanceFile, lastJson, parseSwebenchEval, patchFiles, SB_DIR,
  SWE_AGENT_INSTANCE_TEMPLATE, SWEBENCH, swebenchPrompt,
} from './suite.mjs';

const HERE = path.dirname(new URL(import.meta.url).pathname);
const DATA = path.join(SRC, 'datasets', 'swe-bench-verified');
const PREP_VENV = path.join(TOOLCHAINS, 'bench-prep');

/** The embedded template must be the pinned file's instance_template, verbatim (YAML `|-` block, 6-space indent). */
export function templateInYaml(yamlText, template = SWE_AGENT_INSTANCE_TEMPLATE) {
  const block = template.split('\n').map((l) => (l ? `      ${l}` : '')).join('\n');
  return yamlText.includes(`    instance_template: |-\n${block}\n`);
}

async function* rows(file) {
  const rl = readline.createInterface({ input: fs.createReadStream(file, { encoding: 'utf8' }), crlfDelay: Infinity });
  for await (const line of rl) if (line.trim()) yield JSON.parse(line);
}

export async function ensureSwebenchVenv() {
  const lockText = await fsp.readFile(SWEBENCH.requirements, 'utf8');
  await ensureManagedVenv(T.swebench, SWEBENCH.python, [['--require-hashes', '-r', SWEBENCH.requirements]], { stampExtra: { lock_sha256: sha256(lockText) } });
  return { lock_sha256: sha256(lockText) };
}

export async function prepareSwebench({ force = false, images = null, validate = false, only = null, workers = 2 } = {}) {
  const manifestPath = path.join(SB_DIR, 'tasks.json');
  const venv = await ensureSwebenchVenv();
  if (force || !await exists(manifestPath)) await buildStore(venv);
  else log('swebench: already prepared');
  if (images) await prepareImages({ policy: images, validate, only, workers });
  return readJson(manifestPath);
}

async function buildStore(venv) {
  const data = await fetchPinned({
    url: `https://huggingface.co/datasets/${SWEBENCH.dataset}/resolve/${SWEBENCH.revision}/${SWEBENCH.file.name}`,
    dest: path.join(DATA, path.basename(SWEBENCH.file.name)), sha256: SWEBENCH.file.sha256,
    name: `${SWEBENCH.dataset} test split`, revision: SWEBENCH.revision,
  });
  const tmpl = await fetchPinned({
    url: SWEBENCH.promptSource.url, dest: path.join(DATA, 'swe-agent-default.yaml'), sha256: SWEBENCH.promptSource.sha256,
    name: 'SWE-agent config/default.yaml (instance_template)', revision: SWEBENCH.promptSource.revision,
  });
  if (!templateInYaml(await fsp.readFile(tmpl.path, 'utf8'))) throw new Error('swebench: the embedded prompt template differs from SWE-agent config/default.yaml');
  await ensureManagedVenv(PREP_VENV, '3.12', [['pyarrow==21.0.0', 'pyyaml==6.0.2', 'packaging==25.0']]);
  await rmrf(SB_DIR);
  await ensureDir(SB_DIR);
  const extracted = path.join(SB_DIR, '.rows.jsonl');
  await runOk(path.join(PREP_VENV, 'bin', 'python'), ['-I', path.join(HERE, 'extract.py'), data.path, extracted]);
  const entries = [];
  for await (const row of rows(extracted)) {
    const id = row.instance_id;
    await writeJson(path.join(SB_DIR, id, 'instance.json'), row);
    const task = {
      id, suite: 'swebench', lang: 'python', group: row.repo, repo: row.repo, instance_id: id, base_commit: row.base_commit,
      version: row.version, difficulty: row.difficulty ?? null, created_at: row.created_at,
      environment_setup_commit: row.environment_setup_commit,
      test_files: patchFiles(row.test_patch), gold_files: patchFiles(row.patch),
      prompt: swebenchPrompt(row), file_list: '',
    };
    await writeJson(path.join(SB_DIR, id, 'task.json'), task);
    entries.push({ id, lang: task.lang, group: task.group });
  }
  await rmrf(extracted);
  if (entries.length !== SWEBENCH.count) throw new Error(`swebench: ${entries.length} instances, expected ${SWEBENCH.count}`);
  const manifest = {
    suite: 'swebench',
    sources: [data, tmpl, { name: 'swebench harness', url: 'https://pypi.org/project/swebench/4.1.0/', revision: SWEBENCH.harness, sha256: null, bytes: null, venv_lock_sha256: venv.lock_sha256 }],
    grading: { harness: SWEBENCH.harness, call: 'make_test_spec(instance, namespace, arch) + run_instance (evaluate.py)', metric: 'resolved (report.json), one attempt', test_timeout_sec: SWEBENCH.testTimeoutSec },
    prompt: { source: SWEBENCH.promptSource.url, working_dir: SWEBENCH.workdir },
    tasks: entries,
  };
  await writeJson(path.join(SB_DIR, 'tasks.json'), manifest);
  log(`swebench: ${entries.length} instances`);
}

async function gold(task, source, { namespace, arch }) {
  const logRoot = path.join(SB_DIR, 'image-logs', 'gold');
  const res = await run(py(T.swebench), ['-I', EVALUATE_PY, '--instance', instanceFile(task), '--gold', '--model-name', 'gold',
    '--run-id', `cceval-gold-${source}`, '--namespace', namespace ?? 'none', '--arch', arch, '--timeout', String(SWEBENCH.testTimeoutSec), '--log-root', logRoot], {
    env: { ...process.env, ...(await dockerEnv()) }, timeoutMs: (SWEBENCH.testTimeoutSec + 3600) * 1000,
  });
  const summary = lastJson(res.stdout);
  const logDir = summary?.log_dir;
  const report = logDir ? await readJson(path.join(logDir, 'report.json'), null) : null;
  const logText = logDir ? await fsp.readFile(path.join(logDir, 'run_instance.log'), 'utf8').catch(() => null) : null;
  const v = parseSwebenchEval({ instanceId: task.instance_id, summary, report, log: logText });
  return { resolved: v.resolved, status: v.status, tests: v.tests, error: summary?.error ?? null };
}

async function prepareImages({ policy, validate, only, workers }) {
  if (!['x86_64', 'arm64', 'all'].includes(policy)) throw new Error(`--images ${policy}: expected x86_64, arm64 or all`);
  const manifest = await readJson(path.join(SB_DIR, 'tasks.json'));
  const ids = manifest.tasks.map((t) => t.id).filter((id) => !only || only.includes(id));
  const { hubArm64 } = await imageIndex();
  const file = path.join(SB_DIR, 'images.json');
  const record = await readJson(file, {});
  const sources = { hub: 'hub-arm64', local: 'local-arm64' };
  let done = 0;
  await pool(ids, workers, async (id) => {
    const task = await readJson(path.join(SB_DIR, id, 'task.json'));
    const entry = record[id] ?? {};
    const candidates = [];
    if (policy !== 'x86_64') candidates.push(hubArm64.has(dockerName(id)) ? sources.hub : sources.local);
    if (policy !== 'arm64') candidates.push('hub-x86_64');
    for (const source of candidates) {
      try {
        const img = await ensureImage(task, source);
        entry[source] = { ok: true, image: img.image, digest_match: img.digest_match, repo_digests: img.repo_digests, architecture: img.architecture };
        if (validate) entry[source].gold = await gold(task, source, { namespace: img.namespace, arch: img.arch });
      } catch (error) {
        entry[source] = { ok: false, error: String(error?.message ?? error).slice(0, 2000) };
      }
    }
    const arm = [sources.hub, sources.local].find((s) => entry[s]?.ok && entry[s]?.gold?.resolved);
    entry.selected = arm ?? null;
    record[id] = entry;
    await writeJson(file, record);
    log(`swebench images ${++done}/${ids.length} ${id}: ${candidates.map((c) => `${c}=${entry[c]?.ok ? (entry[c].gold ? (entry[c].gold.resolved ? 'gold-ok' : 'gold-fail') : 'ok') : 'failed'}`).join(' ')}`);
  });
}
