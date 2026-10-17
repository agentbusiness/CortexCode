// Prepare LiveCodeBench: pinned dataset files, the official grader, task store, and the grader
// controls (positive: a test-oracle program must pass every problem; negative: the untouched
// template must fail every problem).
//
// Window and contamination: release_v6 spans contest dates 2023-05 .. 2025-04. The model under test
// (Qwen3.8-27B) was published 2026-08 and its training cutoff is not published, so no part of
// release_v6 lies after its cutoff. The two newest increments (test5.jsonl: 2024-09-22..2025-01-04,
// test6.jsonl: 2025-01-04..2025-04-06; 342 problems) are used: they are the least exposed window,
// and they keep the download at 0.69 GB instead of 4.49 GB for the older increments
// (test.jsonl..test4.jsonl, problems up to 2024-09), which would only add more-contaminated problems.
// LiveCodeBench ships no reference solutions, so the positive control is a test-oracle program
// (it looks the expected output up by input); it proves the decoding, the grader plumbing and the
// comparison accept correct outputs for every test, not that a problem is solvable.
import fs from 'node:fs';
import fsp from 'node:fs/promises';
import path from 'node:path';
import readline from 'node:readline';
import { fetchPinned, ensureGitCommit } from '../../lib/fetch.mjs';
import { SRC, tmpRoot } from '../../lib/paths.mjs';
import { ensureManagedVenv, T } from '../../lib/toolchains.mjs';
import { ensureDir, exists, log, pool, rmrf, runOk, sha256, writeJson, writeText } from '../../lib/util.mjs';
import { materializeWorkspace } from '../../lib/workspace.mjs';
import { LCB, LCB_DIR, lcbPrompt, lcbScore, templateSource } from './suite.mjs';

const HERE = path.dirname(new URL(import.meta.url).pathname);
const DATA = path.join(SRC, 'datasets', 'lcb');

async function ensureGrader() {
  const repo = path.join(SRC, 'livecodebench');
  const src = await ensureGitCommit(repo, LCB.repo, LCB.commit);
  await ensureManagedVenv(T.lcb, LCB.python, [LCB.venv]);
  // Official grading modules, byte-identical to the pinned commit; the package __init__ files are
  // empty (upstream's evaluation/__init__.py imports unrelated scorers).
  const dest = path.join(T.lcb, 'src');
  await rmrf(dest);
  const files = [];
  for (const rel of LCB.graderFiles) {
    const res = await runOk('git', ['show', `${LCB.commit}:${rel}`], { cwd: repo });
    await writeText(path.join(dest, rel), res.stdout);
    files.push({ path: rel, sha256: sha256(res.stdout) });
  }
  await writeText(path.join(dest, 'lcb_runner', '__init__.py'), '');
  await writeText(path.join(dest, 'lcb_runner', 'evaluation', '__init__.py'), '');
  return { ...src, files };
}

function safeId(s) {
  return s.replace(/[^A-Za-z0-9_-]/g, '_');
}

/**
 * Oracle program for the positive control: it loads the problem's own tests (from a file outside the
 * scoring copy) and answers each call with the expected output, keyed exactly as the grader feeds
 * input (stdin: the raw input string; functional: the JSON-decoded argument list).
 */
function oracleSource(sample, dataFile) {
  const load = `import json, pathlib, sys\n_S = json.loads(pathlib.Path(${JSON.stringify(dataFile)}).read_text())\n`;
  if (sample.fn_name) {
    return load
      + '_D = {json.dumps([json.loads(l) for l in i.split("\\n")]): o for i, o in zip(_S["inputs"], _S["outputs"])}\n'
      + 'class Solution:\n'
      + `    def ${sample.fn_name}(self, *args):\n`
      + '        return json.loads(_D[json.dumps(list(args))])\n';
  }
  return load
    + '_D = dict(zip(_S["inputs"], _S["outputs"]))\n'
    + 'sys.stdout.write(_D[sys.stdin.read()])\n';
}

export async function validate(task, rec) {
  const out = { id: task.id };
  const tmp = path.join(tmpRoot(), 'cceval-lcb-validate', task.id);
  await rmrf(tmp);
  // template: the untouched workspace; oracle: every expected output; wrong: the oracle with the
  // expected outputs rotated by one test (a well-formed program giving wrong answers).
  const distinct = new Set(rec.sample.outputs).size > 1;
  for (const mode of ['template', 'oracle', 'wrong']) {
    if (mode === 'wrong' && !distinct) { out.wrong = 'not-applicable'; continue; }
    const ws = path.join(tmp, `${mode}-ws`);
    await materializeWorkspace(path.join(LCB_DIR, task.id, 'template'), ws);
    let override;
    if (mode !== 'template') {
      const dataFile = path.join(tmp, `${mode}.json`);
      const outputs = mode === 'oracle' ? rec.sample.outputs : [...rec.sample.outputs.slice(1), rec.sample.outputs[0]];
      await writeJson(dataFile, { ...rec.sample, outputs });
      const src = oracleSource(rec.sample, dataFile);
      override = (dir) => fsp.writeFile(path.join(dir, 'solution.py'), src);
    }
    const res = await lcbScore(task, ws, path.join(tmp, `${mode}-score`), path.join(tmp, `${mode}-log`), `lcbval-${mode}`, { solutionOverride: override });
    const verdict = lastJson(res.output);
    out[mode] = res.passed ? 'pass' : 'fail';
    out[`${mode}_ms`] = res.ms;
    out[`${mode}_detail`] = verdict ? { verdict: verdict.verdict, n_passed: verdict.n_passed, n_tests: verdict.n_tests, error: verdict.metadata?.error_message ?? verdict.error ?? null } : { tail: res.output.slice(-400) };
  }
  await rmrf(tmp);
  return out;
}

export function lastJson(output) {
  const lines = output.trim().split('\n');
  for (let i = lines.length - 1; i >= 0; i--) {
    if (lines[i].startsWith('{')) { try { return JSON.parse(lines[i]); } catch { /* keep looking */ } }
  }
  return null;
}

async function* rows(file) {
  const rl = readline.createInterface({ input: fs.createReadStream(file, { encoding: 'utf8' }), crlfDelay: Infinity });
  for await (const line of rl) if (line.trim()) yield JSON.parse(line);
}

export async function prepareLivecodebench({ force = false, validateAll = true } = {}) {
  const manifestPath = path.join(LCB_DIR, 'tasks.json');
  if (!force && await exists(manifestPath)) {
    log('livecodebench: already prepared');
    return JSON.parse(await fsp.readFile(manifestPath, 'utf8'));
  }
  const sources = [];
  for (const f of LCB.files) {
    sources.push(await fetchPinned({
      url: `https://huggingface.co/datasets/${LCB.dataset}/resolve/${LCB.datasetRevision}/${f.name}`,
      dest: path.join(DATA, f.name), sha256: f.sha256, name: `${LCB.dataset}/${f.name} (${f.release})`, revision: LCB.datasetRevision,
    }));
  }
  const grader = await ensureGrader();
  sources.push({ name: 'LiveCodeBench grader (lcb_runner/evaluation)', url: LCB.repo, revision: LCB.commit, sha256: null, bytes: null, files: grader.files });
  await rmrf(LCB_DIR);
  await ensureDir(LCB_DIR);
  const tasks = [];
  const seen = new Set();
  for (const f of LCB.files) {
    const decoded = path.join(LCB_DIR, `.decoded-${f.name}`);
    await runOk(path.join(T.lcb, 'bin', 'python'), ['-I', path.join(HERE, 'decode.py'), path.join(DATA, f.name), decoded], { timeoutMs: 3600000 });
    for await (const rec of rows(decoded)) {
      const id = `lcb-${rec.platform}-${safeId(rec.question_id)}`;
      if (seen.has(id)) throw new Error(`duplicate LiveCodeBench id ${id}`);
      const dir = path.join(LCB_DIR, id);
      await writeText(path.join(dir, 'template', 'solution.py'), templateSource(rec));
      await writeJson(path.join(dir, 'tests', 'tests.json'), rec.sample);
      const task = {
        id, suite: 'livecodebench', lang: 'python', group: rec.difficulty, platform: rec.platform,
        question_id: rec.question_id, question_title: rec.question_title, contest_id: rec.contest_id,
        contest_date: rec.contest_date, date: rec.contest_date.slice(0, 10), release_file: f.name,
        test_type: rec.sample.fn_name ? 'functional' : 'stdin', fn_name: rec.sample.fn_name,
        n_tests: rec.sample.inputs.length, n_public: rec.n_public, n_private: rec.n_private,
        solution_files: ['solution.py'], file_list: 'solution.py', prompt: lcbPrompt(rec),
        tests_sha256: sha256(await fsp.readFile(path.join(dir, 'tests', 'tests.json'))),
      };
      await writeJson(path.join(dir, 'task.json'), task);
      tasks.push(task);
      seen.add(id);
    }
  }
  let validation = [];
  if (validateAll) {
    log(`livecodebench: validating ${tasks.length} problems (oracle passes, template fails)`);
    // Re-read each problem's tests for its oracle (the decoded rows are too large to keep in memory).
    validation = await pool(tasks, 8, async (t) => {
      const sample = JSON.parse(await fsp.readFile(path.join(LCB_DIR, t.id, 'tests', 'tests.json'), 'utf8'));
      return validate(t, { sample });
    });
  }
  for (const f of LCB.files) await rmrf(path.join(LCB_DIR, `.decoded-${f.name}`));
  const byId = new Map(validation.map((v) => [v.id, v]));
  const entries = tasks.map((t) => {
    const v = byId.get(t.id);
    const bad = v && !(v.oracle === 'pass' && v.template === 'fail' && v.wrong !== 'pass');
    return { id: t.id, lang: t.lang, group: t.group, date: t.date, platform: t.platform, ...(bad ? { excluded: `grader control failed: oracle=${v.oracle} template=${v.template} wrong=${v.wrong}` } : {}) };
  });
  const counts = {};
  for (const e of entries.filter((x) => !x.excluded)) counts[e.group] = (counts[e.group] ?? 0) + 1;
  const dates = tasks.map((t) => t.date).sort();
  const manifest = {
    suite: 'livecodebench', sources, counts, window: { from: dates[0], to: dates.at(-1) },
    contamination: 'Qwen3.8-27B (published 2026-08; training cutoff unpublished) postdates every problem; no post-cutoff window exists in release_v6.',
    grading: { source: `${LCB.repo} lcb_runner/evaluation/compute_code_generation_metrics.py check_correctness`, commit: LCB.commit, timeout_per_test_s: LCB.timeoutPerTest, metric: 'pass@1 (one attempt; all public and private tests must pass)' },
    controls: { positive: 'test-oracle program (LiveCodeBench ships no reference solutions)', negative: ['untouched template (starter code or placeholder)', 'oracle with expected outputs rotated by one test (wrong answers, well-formed program)'] },
    tasks: entries, validation,
  };
  await writeJson(manifestPath, manifest);
  return manifest;
}
