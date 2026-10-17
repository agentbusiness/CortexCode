// Report computation from raw logs only. Every number is re-derived from files the harness wrote
// during runs: test runner exit codes and output, the agent's final files, journals, integrity
// checks and timestamps. Summary files (result.json) are only cross-checked, never trusted.
import fs from 'node:fs';
import fsp from 'node:fs/promises';
import path from 'node:path';
import { formatSource } from './formatters.mjs';
import { formatConsistent, summarizeJournals, trajectoryUsage } from './driver.mjs';
import { HIDDEN } from './paths.mjs';
import { mcnemar, mean, median, pairOutcomes, sum, wilson } from './stats.mjs';
import { pool, readJson, sha256 } from './util.mjs';
import { COUNTERS, REALISTIC_REPOS } from '../suites/realistic/repos.mjs';
import { ALL_SUITES, SUITES } from './suites.mjs';

async function readText(file) {
  try { return await fsp.readFile(file, 'utf8'); } catch { return null; }
}

function dirs(p) {
  try { return fs.readdirSync(p, { withFileTypes: true }).filter((d) => d.isDirectory()).map((d) => d.name).sort(); } catch { return []; }
}

/** Find run directories: <root>/<suite>/<system>/<model>/run-<n>/run.json */
export function findRuns(root) {
  const runs = [];
  for (const suite of dirs(root).filter((d) => ALL_SUITES.includes(d))) {
    for (const system of dirs(path.join(root, suite))) {
      for (const model of dirs(path.join(root, suite, system))) {
        for (const run of dirs(path.join(root, suite, system, model))) {
          const dir = path.join(root, suite, system, model, run);
          if (fs.existsSync(path.join(dir, 'run.json'))) runs.push({ suite, system, model, run, dir });
        }
      }
    }
  }
  return runs;
}

const formatCache = new Map();
async function formatted(file, text, edition) {
  const key = `${file}\0${edition}\0${sha256(text)}`;
  if (!formatCache.has(key)) formatCache.set(key, formatSource(file, text, { rustEdition: edition }));
  return formatCache.get(key);
}

/** Re-derive one attempt's test/edit outcome from raw files. */
async function rescoreAttempt(suite, task, attemptDir, opts) {
  const scoreDir = path.join(attemptDir, 'score');
  if (suite === 'editbench') {
    const originalPath = path.join(opts.hiddenRoot, 'editbench', task.id, 'original', task.path);
    const original = await readText(originalPath);
    if (original === null) return { ok: false, error: `missing original ${originalPath}` };
    const final = await readText(path.join(scoreDir, 'final', task.path));
    if (final === null) return { ok: true, passed: false, exact: false, formatted: false, file_present: false };
    const exact = final === original;
    let fmt = exact;
    if (!exact && opts.format !== false) {
      const edition = task.rust_edition ?? '2021';
      const [fo, ff] = await Promise.all([formatted(task.path, original, edition), formatted(task.path, final, edition)]);
      fmt = fo.ok && ff.ok && fo.text === ff.text;
    }
    return { ok: true, passed: fmt, exact, formatted: fmt, file_present: true };
  }
  // Container suites re-derive the verdict from their official harness's raw files (SWE-bench
  // report.json / run_instance.log, Harbor result.json).
  if (SUITES[suite]?.rescore) return SUITES[suite].rescore(task, attemptDir);
  const res = await readJson(path.join(scoreDir, 'result.json'), null);
  const output = await readText(path.join(scoreDir, 'output.txt'));
  if (!res || output === null) return { ok: false, error: 'missing score result/output' };
  if (suite === 'polyglot') {
    return { ok: true, passed: res.exit_code === 0 && !res.timed_out, exit_code: res.exit_code, timed_out: res.timed_out };
  }
  if (suite !== 'realistic') {
    // Language-specific suites: the benchmark's own grader ran in the scoring sandbox; its exit code
    // is the verdict (0 = every test passed). The grader's last output line is its JSON verdict.
    const out = { ok: true, passed: res.exit_code === 0 && !res.timed_out, exit_code: res.exit_code, timed_out: res.timed_out };
    const verdict = lastJsonLine(output);
    if (suite === 'rustevo') out.official_pass = !!verdict?.official_pass && out.passed;
    if (suite === 'webbench') out.index = res.index ?? verdict?.index ?? null;
    return out;
  }
  const repo = REALISTIC_REPOS.find((r) => r.id === task.repo);
  const counts = COUNTERS[repo.counter](output);
  const minPassed = task.reference?.passed ?? 1;
  const passed = res.exit_code === 0 && !res.timed_out && counts.found && counts.failed === 0 && counts.passed >= minPassed;
  return { ok: true, passed, exit_code: res.exit_code, timed_out: res.timed_out, counts };
}

function lastJsonLine(output) {
  const lines = (output ?? '').trim().split('\n');
  for (let i = lines.length - 1; i >= 0; i--) {
    if (lines[i].startsWith('{')) { try { return JSON.parse(lines[i]); } catch { /* keep looking */ } }
  }
  return null;
}

/**
 * Web-Bench metrics for one project from its per-step attempts (projects/readme.md):
 * pass_tasks@1 = tasks passed on the first attempt before the first failure; pass_tasks@2 = tasks
 * passed within the attempts; the optional init task is not counted.
 */
export function sequentialCounts(task, attempts) {
  const steps = task.steps ?? [];
  const passedSteps = new Set();
  const firstTry = new Map();
  for (const a of attempts) {
    if (!a.step) continue;
    const s = steps[a.step.k];
    if (!s || s.init) continue;
    if (a.passed) passedSteps.add(a.step.k);
    if (a.step.attempt === 1) firstTry.set(a.step.k, !!a.passed);
  }
  let pass1 = 0;
  for (let k = 0; k < steps.length; k++) {
    if (steps[k].init) continue;
    if (firstTry.get(k) === true) pass1++;
    else break;
  }
  return { tasks: steps.filter((s) => !s.init).length, pass1, pass2: passedSteps.size, errors1: [...firstTry.values()].filter((v) => !v).length };
}

async function loadTask(suite, id, hiddenRoot) {
  // Suites whose tasks are not materialized under the hidden store (skillsel: its bank) name them.
  if (SUITES[suite]?.reportTask) return SUITES[suite].reportTask(id);
  const sub = suite === 'editbench' ? 'editbench' : suite;
  return readJson(path.join(hiddenRoot, sub, id, 'task.json'));
}

function failureClass(t) {
  if (t.error) return 'harness_error';
  if (t.integrity_ok === false) return 'integrity_violation';
  if (t.passed) return null;
  if (t.timed_out) return 'timeout';
  if (t.terminal_state === 'failed') return 'run_failed';
  if (t.score_failure_class) return t.score_failure_class;
  if (t.diff_empty) return 'no_change';
  if (t.file_present === false) return 'file_missing';
  if (t.test_timed_out) return 'test_timeout';
  return t.suite === 'editbench' ? 'edit_mismatch' : 'tests_failed';
}

/** Re-derive every task outcome of a run. */
export async function analyzeRun(run, { hiddenRoot = HIDDEN, format = true } = {}) {
  const meta = await readJson(path.join(run.dir, 'run.json'));
  const tasks = await pool(meta.tasks, 8, async (id) => {
    const tdir = path.join(run.dir, 'tasks', id);
    const result = await readJson(path.join(tdir, 'result.json'), null);
    if (!result) return { id, missing: true };
    const task = await loadTask(meta.suite, id, hiddenRoot);
    const attempts = [];
    for (let k = 1; fs.existsSync(path.join(tdir, `attempt-${k}`)); k++) {
      const adir = path.join(tdir, `attempt-${k}`);
      const att = await readJson(path.join(adir, 'attempt.json'), null);
      if (!att) break;
      const hidden = await readJson(path.join(adir, 'hidden_check.json'), null);
      const diff = await readText(path.join(adir, 'diff.patch'));
      const rescored = await rescoreAttempt(meta.suite, task, adir, { hiddenRoot, format });
      const timedOut = !!(att.timed_out ?? att.turn?.timed_out);
      const integrityOk = hidden ? hidden.ok && (hidden.leaks?.length ?? 0) === 0 : false;
      attempts.push({
        attempt: k, rescored, timed_out: timedOut, integrity_ok: integrityOk, step: att.step ?? null,
        // Terminal-Bench grades the state an agent timeout left (the verifier still runs).
        passed: !!rescored.passed && (!timedOut || !!SUITES[meta.suite]?.scoreAfterTimeout) && integrityOk,
        diff_empty: diff === null ? null : diff.length === 0,
        terminal_state: att.turn?.terminal?.state ?? null,
        recorded_passed: !!att.passed,
        wall_ms: att.turn ? att.turn.ended_ms - att.turn.started_ms : null,
      });
    }
    const last = attempts.at(-1);
    let usage;
    if (meta.system === 'cortex') {
      const journals = await readJson(path.join(tdir, 'journal.json'), null);
      usage = journals ? summarizeJournals(journals) : null;
    } else if (meta.system === 'mini') {
      const traj = await readJson(path.join(tdir, 'trajectory.json'), null);
      usage = traj ? { ...trajectoryUsage(traj), cached_tokens: NaN, prefill_ms: NaN, decode_ms: NaN, tool_calls: NaN, edit_shapes: {} } : null;
    } else {
      const ev = attempts.length ? await readJson(path.join(tdir, `attempt-${attempts.length}`, 'attempt.json'), null) : null;
      const fin = result.runner?.finished ?? ev?.runner ?? null;
      usage = fin ? { prompt_tokens: Number(fin.prompt_tokens), completion_tokens: Number(fin.completion_tokens), cached_tokens: NaN, prefill_ms: NaN, decode_ms: NaN, tool_calls: NaN, edit_shapes: {} } : null;
    }
    const viol = await readJson(path.join(tdir, 'violations.json'), { events: [] });
    const t = {
      id, lang: task.lang, group: task.group ?? null, suite: meta.suite, type: task.mutation?.type ?? null, repo: task.repo ?? null, origin: task.origin ?? null,
      official_pass: meta.suite === 'rustevo' ? !!last?.rescored?.official_pass && !!last?.passed && !result.error : null,
      seq: SUITES[meta.suite]?.sequential && task.steps ? sequentialCounts(task, attempts.map((a) => ({ ...a, passed: a.passed }))) : null,
      attempts, passed: !!last?.passed && !result.error, pass_1: !!attempts[0]?.passed && !result.error,
      exact: !!last?.rescored?.exact && !!last?.passed,
      timed_out: !!last?.timed_out, integrity_ok: attempts.every((a) => a.integrity_ok),
      diff_empty: last?.diff_empty, file_present: last?.rescored?.file_present, test_timed_out: !!last?.rescored?.timed_out,
      score_failure_class: last?.rescored?.failure_class ?? null,
      terminal_state: last?.terminal_state, error: result.error ?? null,
      wall_ms: result.ended_ms - result.started_ms, usage,
      violations: { net: (viol.events ?? []).filter((v) => v.kind === 'net').length, hidden: (viol.events ?? []).filter((v) => v.kind === 'hidden').length },
      leaks: attempts.reduce((s, a) => s + (a.integrity_ok ? 0 : 1), 0),
      rescore_errors: attempts.filter((a) => !a.rescored.ok).map((a) => a.rescored.error),
      recorded_passed: !!result.passed,
    };
    t.failure_class = failureClass(t);
    t.consistent = t.recorded_passed === t.passed;
    return t;
  });
  return { ...run, meta, tasks };
}

function rate(tasks, key = 'passed') {
  const n = tasks.length;
  const k = tasks.filter((t) => t[key]).length;
  return { k, n, ...wilson(k, n) };
}

function groupBy(list, fn) {
  const m = new Map();
  for (const x of list) {
    const k = fn(x);
    if (!m.has(k)) m.set(k, []);
    m.get(k).push(x);
  }
  return m;
}

/** Aggregate analyzed runs into the report model. */
export function aggregate(analyzed) {
  const groups = groupBy(analyzed, (r) => `${r.suite}\0${r.system}\0${r.model}`);
  const series = [];
  for (const [key, runs] of groups) {
    const [suite, system, model] = key.split('\0');
    const all = runs.flatMap((r) => r.tasks.filter((t) => !t.missing).map((t) => ({ ...t, run: r.run })));
    const perRun = runs.map((r) => ({ run: r.run, complete: r.tasks.every((t) => !t.missing), ...rate(r.tasks.filter((t) => !t.missing)), pass1: rate(r.tasks.filter((t) => !t.missing), 'pass_1') }));
    const byLang = {};
    for (const [lang, ts] of groupBy(all, (t) => t.lang)) byLang[lang] = rate(ts);
    const byType = {};
    if (suite === 'editbench') for (const [type, ts] of groupBy(all, (t) => t.type)) byType[type] = rate(ts);
    const byGroup = {};
    for (const [group, ts] of groupBy(all.filter((t) => t.group), (t) => t.group)) byGroup[group] = rate(ts);
    const seqTasks = all.filter((t) => t.seq);
    const seq = seqTasks.length ? {
      projects: seqTasks.length, tasks: sum(seqTasks.map((t) => t.seq.tasks)), pass1: sum(seqTasks.map((t) => t.seq.pass1)),
      pass2: sum(seqTasks.map((t) => t.seq.pass2)), errors1: sum(seqTasks.map((t) => t.seq.errors1)),
      by_project: seqTasks.map((t) => ({ id: t.id, run: t.run, ...t.seq })),
    } : null;
    const failures = {};
    for (const t of all) if (t.failure_class) failures[t.failure_class] = (failures[t.failure_class] ?? 0) + 1;
    const u = (f) => all.map((t) => (t.usage ? Number(t.usage[f]) : NaN));
    const shapes = {};
    for (const t of all) for (const [s, n] of Object.entries(t.usage?.edit_shapes ?? {})) shapes[s] = (shapes[s] ?? 0) + n;
    const fmt = system.startsWith('cortex-') ? system.slice('cortex-'.length) : null;
    series.push({
      suite, system, model, runs: perRun, pooled: rate(all), pooled_pass1: rate(all, 'pass_1'), pooled_exact: rate(all, 'exact'), by_lang: byLang, by_type: byType, failures,
      by_group: byGroup, seq, official: suite === 'rustevo' ? rate(all, 'official_pass') : null,
      tokens: {
        prompt_mean: mean(u('prompt_tokens')), prompt_median: median(u('prompt_tokens')),
        cached_mean: mean(u('cached_tokens')), completion_mean: mean(u('completion_tokens')), completion_median: median(u('completion_tokens')),
        prefill_s_mean: mean(u('prefill_ms')) / 1000, decode_s_mean: mean(u('decode_ms')) / 1000,
        tool_calls_mean: mean(u('tool_calls')),
      },
      wall_s_mean: mean(all.map((t) => t.wall_ms)) / 1000, wall_s_median: median(all.map((t) => t.wall_ms)) / 1000,
      integrity: {
        leaks: sum(all.map((t) => t.leaks)), net_denials: sum(all.map((t) => t.violations.net)), hidden_denials: sum(all.map((t) => t.violations.hidden)),
      },
      edit_shapes: shapes, format_consistent: fmt ? formatConsistent(fmt, shapes) : null,
      inconsistent: all.filter((t) => !t.consistent).map((t) => `${t.run}/${t.id}`),
      rescore_errors: all.flatMap((t) => t.rescore_errors.map((e) => `${t.run}/${t.id}: ${e}`)),
      outcomes: Object.fromEntries(all.map((t) => [`${t.run}/${t.id}`, t.passed])),
      outcomes_by_task_run: all,
    });
  }
  series.sort((a, b) => (a.suite + a.system + a.model).localeCompare(b.suite + b.system + b.model));
  return { series, comparisons: comparisons(series) };
}

/** Paired comparisons on identical (task, run) sets: McNemar exact. */
function comparisons(series) {
  const out = [];
  const bySuiteModel = groupBy(series, (s) => `${s.suite}\0${s.model}`);
  for (const [key, list] of bySuiteModel) {
    const [suite, model] = key.split('\0');
    for (let i = 0; i < list.length; i++) {
      for (let j = i + 1; j < list.length; j++) {
        const a = list[i], b = list[j];
        const p = pairOutcomes(a.outcomes, b.outcomes);
        if (!p.n) continue;
        out.push({ suite, model, a: a.system, b: b.system, ...p, diff: (p.onlyA - p.onlyB) / p.n });
      }
    }
  }
  return out;
}

export { mcnemar, wilson };
