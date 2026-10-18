// Aider Polyglot (Python, JavaScript, Rust) materialized with aider's own file rules
// (benchmark/benchmark.py: run_test_real) and prompts (benchmark/prompts.py), except that test
// files, example solutions, .meta and .docs are removed from the agent workspace and kept hidden.
import fs from 'node:fs';
import fsp from 'node:fs/promises';
import path from 'node:path';
import { HIDDEN, SRC, tmpRoot } from '../../lib/paths.mjs';
import { ensureDir, exists, log, pool, rmrf, run, runOk, sha256, writeJson, writeText } from '../../lib/util.mjs';
import { cargoCmd, ensurePolyglotJs, ensurePolyglotPy, offlineEnv, T } from '../../lib/toolchains.mjs';
import { exampleTargets, POLYGLOT, polyglotScore } from './suite.mjs';
import { ensureAgentGradleHome, ensureBoost, ensureGo, ensureGradleHome, ensureJdk, verifyGradleDist, warmGradle } from './langs.mjs';

export const POLYGLOT_DIR = path.join(HIDDEN, 'polyglot');

// Verbatim from aider benchmark/prompts.py at the pinned aider commit.
export const INSTRUCTIONS_ADDENDUM = `
####

Use the above instructions to modify the supplied files: {file_list}
Don't change the names of existing functions or classes, as they may be referenced from other code like unit tests, etc.
Only use standard libraries, don't suggest installing any packages.
`;

export const TEST_FAILURES = `
####

See the testing errors above.
The tests are correct, don't try and change them.
Fix the code in {file_list} to resolve the errors.
`;

async function ensureClone(dir, url, commit) {
  if (!await exists(dir)) await runOk('git', ['clone', '-q', url, dir], { timeoutMs: 1800000 });
  const has = await run('git', ['cat-file', '-e', `${commit}^{commit}`], { cwd: dir });
  if (has.code !== 0) await runOk('git', ['fetch', '-q', 'origin'], { cwd: dir, timeoutMs: 1800000 });
}

async function copyFiltered(src, dst, skip) {
  for (const e of await fsp.readdir(src, { withFileTypes: true })) {
    const rel = e.name;
    if (skip(rel, e)) continue;
    const s = path.join(src, rel);
    const d = path.join(dst, rel);
    if (e.isDirectory()) {
      await ensureDir(d);
      await copyFiltered(s, d, (r, ent) => skip(`${rel}/${r}`, ent));
    } else {
      await ensureDir(path.dirname(d));
      await fsp.copyFile(s, d);
    }
  }
}

async function readExerciseFile(root, rel) {
  try { return await fsp.readFile(path.join(root, rel), 'utf8'); } catch { return null; }
}

/** Files the language's test runner runs as tests. They are hidden with the declared test files: an
 * exercise may ship more than it declares (Go's `cases_test.go` as an editor file, Rust's extra
 * `tests/*.rs`), and test code in the workspace is the tests' answers. */
const TEST_FILE = {
  go: (rel) => rel.endsWith('_test.go'),
  rust: (rel) => rel.startsWith('tests/') && rel.endsWith('.rs'),
  python: (rel) => /(^|\/)(test_[^/]*|[^/]*_test)\.py$/.test(rel),
  javascript: (rel) => /\.(spec|test)\.[cm]?js$/.test(rel),
  java: (rel) => rel.startsWith('src/test/'),
  cpp: (rel) => /_test\.cpp$/.test(rel),
};

/** Every file of the exercise (relative paths), `.meta/` and `.docs/` left out. */
async function exerciseFiles(root, dir = '') {
  const out = [];
  for (const entry of await fsp.readdir(path.join(root, dir), { withFileTypes: true })) {
    const rel = dir ? `${dir}/${entry.name}` : entry.name;
    if (rel === '.meta' || rel === '.docs') continue;
    if (entry.isDirectory()) out.push(...(await exerciseFiles(root, rel)));
    else out.push(rel);
  }
  return out;
}

async function materializeExercise(lang, slug, exDir, outRoot) {
  const config = JSON.parse(await fsp.readFile(path.join(exDir, '.meta', 'config.json'), 'utf8'));
  const isTest = TEST_FILE[lang] ?? (() => false);
  const declared = config.files?.test ?? [];
  const testFiles = [...declared, ...(await exerciseFiles(exDir)).filter((rel) => isTest(rel) && !declared.includes(rel)).sort()];
  const exampleFiles = config.files?.example ?? [];
  const solutionFiles = new Set(config.files?.solution ?? []);
  // aider's ignore set: CMakeLists.txt, Cargo.toml, .meta/**, .docs/**, tests, examples.
  const ignore = new Set(['CMakeLists.txt', 'Cargo.toml', ...testFiles, ...exampleFiles]);
  const solution = [...solutionFiles].filter((f) => !ignore.has(f) && !f.startsWith('.meta/') && !f.startsWith('.docs/'));
  const fnames = solution.filter((f) => fs.existsSync(path.join(exDir, f)));
  const fileList = fnames.map((f) => path.basename(f)).join(' ');

  let instructions = '';
  instructions += (await readExerciseFile(exDir, '.docs/introduction.md')) ?? '';
  instructions += await fsp.readFile(path.join(exDir, '.docs', 'instructions.md'), 'utf8');
  instructions += (await readExerciseFile(exDir, '.docs/instructions.append.md')) ?? '';
  instructions += INSTRUCTIONS_ADDENDUM.replace('{file_list}', fileList);

  const id = `pg-${lang}-${slug}`;
  const taskDir = path.join(outRoot, id);
  await rmrf(taskDir);
  const hiddenTests = new Set(testFiles);
  const hiddenExamples = new Set(exampleFiles);
  await copyFiltered(exDir, path.join(taskDir, 'template'), (rel) =>
    rel === '.meta' || rel.startsWith('.meta/') || rel === '.docs' || rel.startsWith('.docs/') ||
    hiddenTests.has(rel) || hiddenExamples.has(rel));
  for (const f of testFiles) {
    await ensureDir(path.dirname(path.join(taskDir, 'tests', f)));
    await fsp.copyFile(path.join(exDir, f), path.join(taskDir, 'tests', f));
  }
  for (const f of exampleFiles) {
    await ensureDir(path.dirname(path.join(taskDir, 'example', f)));
    await fsp.copyFile(path.join(exDir, f), path.join(taskDir, 'example', f));
  }
  const task = {
    id, suite: 'polyglot', lang, slug, solution_files: fnames, file_list: fileList,
    test_files: testFiles, example_files: exampleFiles, prompt: instructions,
    test_sha256: Object.fromEntries(testFiles.map((f) => [f, sha256(fs.readFileSync(path.join(exDir, f)))])),
  };
  await writeJson(path.join(taskDir, 'task.json'), task);
  return task;
}

/** Fetch every crate the Rust exercises depend on into the eval cargo home (offline later). */
async function fetchRustDeps(tasks) {
  const env = { ...process.env, ...offlineEnv(), CARGO_NET_OFFLINE: 'false' };
  for (const t of tasks.filter((x) => x.lang === 'rust')) {
    const dir = path.join(POLYGLOT_DIR, t.id, 'template');
    const toml = await fsp.readFile(path.join(dir, 'Cargo.toml'), 'utf8');
    if (!/\[dependencies\][^[]*\S+\s*=/.test(toml) && !/\[dev-dependencies\][^[]*\S+\s*=/.test(toml)) continue;
    log(`cargo fetch for ${t.id}`);
    await runOk(cargoCmd(), ['generate-lockfile'], { cwd: dir, env, timeoutMs: 600000 });
    await runOk(cargoCmd(), ['fetch'], { cwd: dir, env, timeoutMs: 600000 });
  }
}

/**
 * Validate the scoring pipeline against independent truth: each exercise's upstream example
 * solution must pass its tests, and the untouched stub must fail them.
 */
async function validate(task) {
  const taskDir = path.join(POLYGLOT_DIR, task.id);
  const out = { id: task.id };
  const tmp = path.join(tmpRoot(), 'cceval-validate', task.id);
  for (const mode of ['stub', 'example']) {
    const ws = path.join(tmp, `${mode}-ws`);
    await rmrf(ws);
    await fsp.cp(path.join(taskDir, 'template'), ws, { recursive: true });
    if (mode === 'example') {
      const targets = exampleTargets(task);
      if (!targets.length || targets.some((t) => !t.to)) { out[mode] = 'not-applicable'; continue; }
      for (const t of targets) {
        await fsp.mkdir(path.dirname(path.join(ws, t.to)), { recursive: true });
        await fsp.copyFile(path.join(taskDir, 'example', t.from), path.join(ws, t.to));
      }
    }
    const res = await polyglotScore(task, ws, path.join(tmp, `${mode}-score`), path.join(tmp, `${mode}-log`), `validate-${mode}`);
    out[mode] = res.passed ? 'pass' : 'fail';
    out[`${mode}_ms`] = res.ms;
    // Structural classification of known upstream exceptions (compiler error codes / outcomes).
    if (mode === 'example' && !res.passed && task.lang === 'rust' && /error\[E0(463|432|433)\]/.test(res.output)) {
      out.note = 'upstream example solution uses crates the exercise Cargo.toml does not declare';
    }
    if (mode === 'example' && !res.passed && task.lang !== 'rust') out.example_tail = res.output.slice(-1500);
    if (mode === 'stub' && res.passed) out.note = 'upstream stub already passes its tests';
  }
  await rmrf(tmp);
  return out;
}

/** Toolchains of the C++/Go/Java exercises (see langs.mjs); returns their source records. */
export async function ensurePolyglotLangToolchains(langs) {
  const sources = [];
  if (langs.includes('go')) sources.push(await ensureGo());
  if (langs.includes('java')) { sources.push(await ensureJdk()); sources.push(await ensureGradleHome()); }
  if (langs.includes('cpp')) sources.push(await ensureBoost());
  return sources;
}

export async function preparePolyglot({ force = false, validateAll = true, languages = POLYGLOT.languages } = {}) {
  const manifestPath = path.join(POLYGLOT_DIR, 'tasks.json');
  const prev = !force && await exists(manifestPath) ? JSON.parse(await fsp.readFile(manifestPath, 'utf8')) : null;
  const have = new Set((prev?.tasks ?? []).map((t) => t.lang));
  const want = languages.filter((l) => !have.has(l));
  if (prev && !want.length) {
    log('polyglot: already prepared');
    return prev;
  }
  if (prev) log(`polyglot: adding ${want.join(', ')} to the prepared ${[...have].join(', ')}`);
  const src = path.join(SRC, 'polyglot-benchmark');
  await ensureClone(src, POLYGLOT.url, POLYGLOT.commit);
  await ensureClone(path.join(SRC, 'aider'), POLYGLOT.aiderUrl, POLYGLOT.aiderCommit);
  // The exercise tree is read at the pinned commit through a private worktree copy.
  const tree = path.join(POLYGLOT_DIR, '.tree');
  await rmrf(tree);
  await ensureDir(tree);
  await runOk('/bin/sh', ['-c', `git -C "${src}" archive --format=tar ${POLYGLOT.commit} | tar -x -C "${tree}"`]);
  await ensurePolyglotPy();
  await ensurePolyglotJs();
  const langSources = await ensurePolyglotLangToolchains(want);
  const tasks = [];
  for (const lang of want) {
    const practice = path.join(tree, lang, 'exercises', 'practice');
    const slugs = (await fsp.readdir(practice)).filter((s) => fs.statSync(path.join(practice, s)).isDirectory()).sort();
    for (const slug of slugs) tasks.push(await materializeExercise(lang, slug, path.join(practice, slug), POLYGLOT_DIR));
  }
  await rmrf(tree);
  await fetchRustDeps(tasks);
  if (want.includes('java')) {
    // Online once: the wrapper fetches Gradle 8.7 and every exercise's test dependencies.
    log('polyglot: warming the Gradle cache (online, once)');
    for (const t of tasks.filter((x) => x.lang === 'java')) {
      const tmp = path.join(tmpRoot(), 'cceval-gradle-warm', t.id);
      await rmrf(tmp);
      await fsp.cp(path.join(POLYGLOT_DIR, t.id, 'template'), tmp, { recursive: true });
      for (const f of t.test_files) await fsp.cp(path.join(POLYGLOT_DIR, t.id, 'tests', f), path.join(tmp, f));
      for (const x of exampleTargets(t)) {
        await fsp.mkdir(path.dirname(path.join(tmp, x.to)), { recursive: true });
        await fsp.copyFile(path.join(POLYGLOT_DIR, t.id, 'example', x.from), path.join(tmp, x.to));
      }
      await warmGradle(tmp);
      await rmrf(tmp);
    }
    await verifyGradleDist();
    await ensureAgentGradleHome();
  }
  let validation = [];
  if (validateAll) {
    log(`polyglot: validating ${tasks.length} exercises (example passes, stub fails)`);
    validation = await pool(tasks, 6, validate);
  }
  const allTasks = [...(prev?.tasks ?? []), ...tasks.map((t) => ({ id: t.id, lang: t.lang, slug: t.slug }))];
  const counts = {};
  for (const t of allTasks) counts[t.lang] = (counts[t.lang] ?? 0) + 1;
  const manifest = {
    suite: 'polyglot', source: POLYGLOT.url, commit: POLYGLOT.commit,
    protocol: { source: `${POLYGLOT.aiderUrl} benchmark/benchmark.py`, commit: POLYGLOT.aiderCommit, tries: 2 },
    counts, tasks: allTasks, validation: [...(prev?.validation ?? []), ...validation],
    sources: [...(prev?.sources ?? []), ...langSources],
  };
  await writeJson(manifestPath, manifest);
  return manifest;
}
