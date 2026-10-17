// Skill-selection results from raw run files: re-derive every turn from the attempt journals the
// runner saved (eval/results/skillsel/<system>/<model>/run-<n>/tasks/<id>/attempt-<k>/journal.json).
import fs from 'node:fs';
import path from 'node:path';
import { readJson } from '../../lib/util.mjs';
import { loadBank } from './bank.mjs';
import { shippedDecisionStages } from './catalog.mjs';
import { combineTurns, foldedBySession, scoreTurn } from './score.mjs';

let bankCache = null;
/** The bank, validated against the catalog once per process (throws BankError on any problem). */
export function bank() {
  bankCache ??= loadBank();
  return bankCache;
}

/** A scenario of the current bank, or null (also when the bank does not load: callers fall back
 *  to the expectations saved with the run). */
export function scenarioById(id) {
  try {
    return bank().scenarios.find((s) => s.id === id) ?? null;
  } catch {
    return null;
  }
}

/** The turn record the runner saved for an attempt (`attempt.json` turn), as scoreTurn wants it. */
export function turnInfo(turn) {
  return { from_seq: turn?.from_seq ?? 1, started_ms: turn?.started_ms ?? 0, ended_ms: turn?.ended_ms ?? null, run_id: turn?.run_id ?? null, timed_out: !!turn?.timed_out };
}

/**
 * Re-derive one attempt (= one turn) from raw files: the attempt journals of this and every
 * earlier turn, the runner's turn records, and the token counts measured when it was scored.
 * The current bank's expectations are used; a scenario no longer in the bank falls back to the
 * expectations saved beside the score (`score/scenario.json`).
 */
export async function rescoreFiles(attemptDir, { scenario = null } = {}) {
  const taskDir = path.dirname(attemptDir);
  const k = Number(path.basename(attemptDir).match(/^attempt-(\d+)$/)?.[1]);
  if (!k) return { ok: false, error: `not an attempt directory: ${attemptDir}` };
  const rec = await readJson(path.join(taskDir, 'result.json'), {});
  const saved = await readJson(path.join(attemptDir, 'score', 'scenario.json'), null);
  const sc = scenario ?? scenarioById(rec.task_id ?? path.basename(taskDir)) ?? saved;
  if (!sc) return { ok: false, error: `scenario ${path.basename(taskDir)} is neither in the bank nor saved with the run` };
  const turns = [];
  let foldedBefore = null;
  let prevPassed = true;
  let res = null;
  for (let j = 1; j <= k; j++) {
    const adir = path.join(taskDir, `attempt-${j}`);
    const att = await readJson(path.join(adir, 'attempt.json'), null);
    const journals = await readJson(path.join(adir, 'journal.json'), null);
    if (!att || !journals) return { ok: false, error: `attempt-${j}: missing attempt.json or journal.json` };
    turns.push(turnInfo(att.turn));
    const scored = await readJson(path.join(adir, 'score', 'result.json'), {});
    const expect = sc.expect[j - 1] ?? sc.expect.at(-1);
    res = scoreTurn({ journals, mainSessionId: rec.session_id ?? scored.session_id ?? null, turns, turnIndex: j - 1, expect, foldedBefore, counts: scored.token_counts ?? {}, stages: shippedDecisionStages() });
    if (j < k) {
      prevPassed &&= res.passed;
      foldedBefore = foldedBySession(journals);
    }
  }
  const passed = res.passed && prevPassed;
  return {
    ok: true, ...res, turn_passed: res.passed, passed, scenario: sc.id, group: sc.group,
    failure_class: passed ? null : (res.failure_class ?? 'skill_earlier_turn'),
    turn_timed_out: turns.at(-1)?.timed_out ?? false,
  };
}

function attemptDirs(taskDir) {
  const out = [];
  for (let k = 1; fs.existsSync(path.join(taskDir, `attempt-${k}`, 'attempt.json')); k++) out.push(path.join(taskDir, `attempt-${k}`));
  return out;
}

/**
 * One scenario's turns from its task directory: `{ id, group, fixture, error?, turns, combined }`.
 * Each attempt is one turn of the conversation.
 */
export async function loadScenarioResult(taskDir, id = path.basename(taskDir)) {
  const rec = await readJson(path.join(taskDir, 'result.json'), null);
  if (!rec) return { id, missing: true };
  const turns = [];
  let error = rec.error ?? null;
  let group = scenarioById(id)?.group ?? null;
  let fixture = scenarioById(id)?.fixture ?? null;
  for (const adir of attemptDirs(taskDir)) {
    const r = await rescoreFiles(adir);
    if (!r.ok) { error ??= r.error; break; }
    group ??= r.group;
    turns.push(r);
  }
  if (!fixture) fixture = (await readJson(path.join(taskDir, 'attempt-1', 'score', 'scenario.json'), null))?.fixture ?? null;
  const combined = turns.length ? combineTurns(turns) : null;
  // A later turn that never ran (the conversation stopped early) leaves the scenario unpassed.
  const expectedTurns = (scenarioById(id)?.turns ?? []).length;
  if (combined && expectedTurns && turns.length < expectedTurns) combined.passed = false;
  return { id, group, fixture, error, wall_ms: rec.wall_ms ?? null, turns, combined };
}

/** Every scenario of one run directory, re-derived from raw files. */
export async function loadRunResults(runDir) {
  const meta = await readJson(path.join(runDir, 'run.json'), null);
  const ids = meta?.tasks ?? (fs.existsSync(path.join(runDir, 'tasks')) ? fs.readdirSync(path.join(runDir, 'tasks')).sort() : []);
  const scenarios = [];
  for (const id of ids) scenarios.push(await loadScenarioResult(path.join(runDir, 'tasks', id), id));
  return { runDir, run: meta?.run ?? path.basename(runDir).replace(/^run-/, ''), model: meta?.model ?? null, system: meta?.system_key ?? meta?.system ?? null, meta, scenarios };
}

/** Run directories under a path: the path itself if it holds run.json, else its run-* children. */
export function runDirsUnder(p) {
  if (fs.existsSync(path.join(p, 'run.json'))) return [p];
  let names = [];
  try { names = fs.readdirSync(p).filter((n) => /^run-\d+$/.test(n)).sort((a, b) => Number(a.slice(4)) - Number(b.slice(4))); } catch { /* none */ }
  return names.map((n) => path.join(p, n)).filter((d) => fs.existsSync(path.join(d, 'run.json')));
}
