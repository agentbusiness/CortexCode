// Prepare RustEvo²: the pinned dataset (GitHub SYSUSELab/RustEvo), the Rust toolchains its std tasks
// target (1.71.0 .. 1.84.0, minimal profile, installed by rustup into eval/cache/toolchains/rustup,
// never into the user's ~/.rustup), the task store, and the controls:
//   positive: RustEvo's reference solution (`code`) must compile and pass the test program;
//   negative: the workspace stub (signature + `unimplemented!()`) must fail.
// Tasks are joined to their API record by position (task_idx = index + 1 in APIDocs.json; checked:
// every API name occurs in its task's reference code). The 208 third-party-crate tasks are measured
// too (reference through the same official compile step) and excluded: RustEvo's evaluator has no
// Cargo manifest or crate versions, so `rustc --test` cannot resolve their `use <crate>` imports.
import fs from 'node:fs';
import fsp from 'node:fs/promises';
import path from 'node:path';
import { ensureGitCommit } from '../../lib/fetch.mjs';
import { SRC, tmpRoot } from '../../lib/paths.mjs';
import { ensureManagedVenv, T } from '../../lib/toolchains.mjs';
import { ensureDir, exists, log, pool, rmrf, run, runOk, sha256, writeJson, writeText } from '../../lib/util.mjs';
import { materializeWorkspace } from '../../lib/workspace.mjs';
import { RUSTEVO, RUSTEVO_DIR, RUSTEVO_PY, rustcFor, rustevoPrompt, rustevoScore, stubSource } from './suite.mjs';

async function ensureToolchains() {
  const missing = RUSTEVO.toolchains.filter((v) => !fs.existsSync(rustcFor(v)));
  if (missing.length) {
    log(`rustevo: installing Rust ${missing.join(', ')} (minimal profile) into ${T.rustup}`);
    await ensureDir(T.rustup);
    const env = { ...process.env, RUSTUP_HOME: T.rustup, CARGO_HOME: path.join(T.rustup, '..', 'rustup-cargo-home') };
    delete env.RUSTUP_TOOLCHAIN;
    await runOk('rustup', ['toolchain', 'install', ...missing, '--profile', 'minimal', '--no-self-update'], { env, timeoutMs: 3 * 3600 * 1000 });
  }
  const versions = [];
  for (const v of RUSTEVO.toolchains) {
    const res = await runOk(rustcFor(v), ['--version']);
    versions.push({ version: v, rustc: res.stdout.trim() });
  }
  return versions;
}


export async function validate(task, ref) {
  const out = { id: task.id };
  const tmp = path.join(tmpRoot(), 'cceval-rustevo-validate', task.id);
  await rmrf(tmp);
  // The reference runs twice: a positive control must pass deterministically (some test programs
  // race threads against sleeps).
  for (const mode of ['stub', 'reference', 'reference_repeat']) {
    const ws = path.join(tmp, `${mode}-ws`);
    await materializeWorkspace(path.join(RUSTEVO_DIR, task.id, 'template'), ws);
    const override = mode.startsWith('reference') ? (dir) => fsp.writeFile(path.join(dir, 'src', 'lib.rs'), ref) : undefined;
    const res = await rustevoScore(task, ws, path.join(tmp, `${mode}-score`), path.join(tmp, `${mode}-log`), `rustevoval-${mode}`, { solutionOverride: override });
    const v = lastJson(res.output);
    out[mode] = res.passed ? 'pass' : 'fail';
    out[`${mode}_ms`] = res.ms;
    out[`${mode}_detail`] = v ? { error: (v.error ?? '').slice(0, 800), compile_exit: v.compile_exit ?? null, test_exit: v.test_exit ?? null, official_pass: v.official_pass ?? null } : { tail: res.output.slice(-800) };
  }
  await rmrf(tmp);
  return out;
}

/** Label for a failing reference, from structural features of the recorded compiler/test output. */
function referenceFailure(d = {}) {
  const e = d.error ?? '';
  if (/INCORRECT CODE/.test(e)) return 'dataset defect: the test program contains the literal placeholder "INCORRECT CODE" (does not compile)';
  if (/error\[E0252\]|error\[E0428\]/.test(e)) return 'dataset defect: the reference and the test program both define the same name at the crate root (E0252/E0428)';
  if (/could not find `windows` in `os`/.test(e)) return 'Windows-only API (std::os::windows): cannot compile on macOS';
  if (d.compile_exit === 0 && d.test_exit !== 0) return 'reference compiles but its own test program fails';
  return 'reference fails RustEvo\'s compile-and-test step (see validation detail)';
}

export function lastJson(output) {
  const lines = output.trim().split('\n');
  for (let i = lines.length - 1; i >= 0; i--) {
    if (lines[i].startsWith('{')) { try { return JSON.parse(lines[i]); } catch { /* keep looking */ } }
  }
  return null;
}

export async function prepareRustevo({ force = false, validateAll = true } = {}) {
  const manifestPath = path.join(RUSTEVO_DIR, 'tasks.json');
  if (!force && await exists(manifestPath)) {
    log('rustevo: already prepared');
    return JSON.parse(await fsp.readFile(manifestPath, 'utf8'));
  }
  const repo = path.join(SRC, 'rustevo');
  const src = await ensureGitCommit(repo, RUSTEVO.repo, RUSTEVO.commit);
  const tasksText = (await runOk('git', ['show', `${RUSTEVO.commit}:${RUSTEVO.tasksFile}`], { cwd: repo })).stdout;
  const apiText = (await runOk('git', ['show', `${RUSTEVO.commit}:${RUSTEVO.apiFile}`], { cwd: repo })).stdout;
  const sources = [
    { ...src, path: undefined, files: [{ path: RUSTEVO.tasksFile, sha256: sha256(tasksText) }, { path: RUSTEVO.apiFile, sha256: sha256(apiText) }] },
  ];
  const toolchains = await ensureToolchains();
  sources.push({ name: 'Rust toolchains (rustup, minimal profile)', url: 'https://static.rust-lang.org', revision: toolchains.map((t) => t.rustc).join('; '), sha256: null, bytes: null });
  await ensureManagedVenv(RUSTEVO_PY, RUSTEVO.python, []);

  const raw = JSON.parse(tasksText).sort((a, b) => a.task_idx - b.task_idx);
  const apis = JSON.parse(apiText);
  if (raw.length !== apis.length) throw new Error(`RustEvo: ${raw.length} tasks vs ${apis.length} API records`);
  await rmrf(RUSTEVO_DIR);
  await ensureDir(RUSTEVO_DIR);
  const tasks = [];
  const refs = new Map();
  for (let i = 0; i < raw.length; i++) {
    const t = raw[i];
    const api = apis[i];
    if (t.task_idx !== i + 1) throw new Error(`RustEvo: task_idx ${t.task_idx} at position ${i}`);
    const base = String(api.name).split('::').at(-1);
    if (!t.code.includes(base)) throw new Error(`RustEvo task ${t.task_idx}: API ${api.name} not in its reference code (join by position failed)`);
    const id = `rustevo-${String(t.task_idx).padStart(3, '0')}`;
    const crate = api.crate ?? null;
    const version = crate ? '1.84.0' : api.to_version; // eval_models_rq1.py: crate tasks use 1.84.0
    const dir = path.join(RUSTEVO_DIR, id);
    await writeText(path.join(dir, 'template', 'Cargo.toml'), `[package]\nname = "rustevo_task"\nversion = "0.1.0"\nedition = "2015"\n\n[lib]\npath = "src/lib.rs"\n\n[dependencies]\n`);
    await writeText(path.join(dir, 'template', 'rust-toolchain.toml'), `[toolchain]\nchannel = "${version}"\nprofile = "minimal"\n`);
    await writeText(path.join(dir, 'template', 'src', 'lib.rs'), stubSource(t.function_signature));
    await writeText(path.join(dir, 'template', '.gitignore'), '/target\nCargo.lock\n');
    await writeJson(path.join(dir, 'tests', 'task-test.json'), { task_idx: t.task_idx, test_program: t.test_program, function_signature: t.function_signature, api_name: api.name });
    await writeText(path.join(dir, 'reference', 'lib.rs'), t.code);
    const task = {
      id, suite: 'rustevo', lang: 'rust', group: crate ? `crate:${crate}` : api.change_type, rust_version: version,
      task_idx: t.task_idx, api_name: api.name, api_module: api.module, change_type: api.change_type, crate,
      from_version: api.from_version, to_version: api.to_version, function_signature: t.function_signature,
      solution_files: ['src/lib.rs'], file_list: 'lib.rs', prompt: rustevoPrompt(t, api),
    };
    await writeJson(path.join(dir, 'task.json'), task);
    tasks.push(task);
    refs.set(id, t.code);
  }
  let validation = [];
  if (validateAll) {
    log(`rustevo: validating ${tasks.length} tasks (reference passes, stub fails)`);
    validation = await pool(tasks, 8, (t) => validate(t, refs.get(t.id)));
  }
  const byId = new Map(validation.map((v) => [v.id, v]));
  const entries = tasks.map((t) => {
    const v = byId.get(t.id);
    let excluded = null;
    if (t.crate) excluded = `third-party crate task (${t.crate}): RustEvo's evaluator compiles with bare rustc --test and no dependencies; reference ${v?.reference ?? 'not run'}`;
    else if (v && v.reference !== 'pass') excluded = referenceFailure(v.reference_detail);
    else if (v && v.reference_repeat !== 'pass') excluded = 'reference passes only some runs (nondeterministic test program)';
    else if (v && v.stub !== 'fail') excluded = 'workspace stub already passes the tests';
    return { id: t.id, lang: t.lang, group: t.group, rust_version: t.rust_version, ...(excluded ? { excluded } : {}) };
  });
  const counts = {};
  for (const e of entries.filter((x) => !x.excluded)) counts[e.group] = (counts[e.group] ?? 0) + 1;
  const manifest = {
    suite: 'rustevo', sources, counts, toolchains,
    grading: { source: `${RUSTEVO.repo} Evaluate/eval_models_rq1.py (create_test_file, run_rust_test with rustup run <to_version> rustc --test)`, commit: RUSTEVO.commit, metric: 'pass@1 (one attempt; compile + tests); official_pass additionally applies RustEvo\'s name checks' },
    controls: { positive: 'RustEvo reference solution (`code`), twice', negative: 'workspace stub (signature + unimplemented!())' },
    tasks: entries, validation,
  };
  await writeJson(manifestPath, manifest);
  return manifest;
}
