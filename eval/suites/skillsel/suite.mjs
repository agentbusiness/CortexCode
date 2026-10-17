// Skill-selection suite (docs/SKILL-SELECTION-TESTS.md): which skills the model reads (or, once a
// decision chooses them, which ones the host injects), when, and what they cost, through the real
// chat flow on the real frame. Every scenario is a conversation of one or more turns sent to one
// session (the runner's sequential protocol); each turn is scored from the journal snapshot taken
// when it ended. Code outcomes are not graded here.
import fsp from 'node:fs/promises';
import fs from 'node:fs';
import path from 'node:path';
import { balancedLimit, filterTasks } from '../../lib/adapter.mjs';
import { T } from '../../lib/toolchains.mjs';
import { appendJsonl, ensureDir, rmrf, sha256, writeJson, writeText } from '../../lib/util.mjs';
import { materializeWorkspace, snapshotSha } from '../../lib/workspace.mjs';
import { GO_BIN, JAVA_HOME } from '../polyglot/langs.mjs';
import { FIXTURES_DIR } from './bank.mjs';
import { shippedDecisionStages } from './catalog.mjs';
import { bank, loadRunResults, rescoreFiles, scenarioById, turnInfo } from './results.mjs';
import { foldedBySession, scoreTurn, textsToCount } from './score.mjs';
import { renderRunSummary } from './summary.mjs';

/** Copy a fixture (after its base, if it has one) into `stage`. */
export async function stageFixture(name, stage, fixtures = bank().fixtures, dir = FIXTURES_DIR) {
  const chain = [];
  for (let f = name; f; f = fixtures[f]?.base) {
    if (chain.includes(f)) throw new Error(`fixture ${name}: base cycle`);
    chain.unshift(f);
  }
  await rmrf(stage);
  await ensureDir(stage);
  for (const f of chain) {
    const src = path.join(dir, f);
    if (fs.existsSync(src)) await fsp.cp(src, stage, { recursive: true, force: true, verbatimSymlinks: true });
  }
}

/** Model token counts of texts (`model.tokenize`), cached per frame client. */
const tokenCaches = new WeakMap();
async function measureTokens(client, texts) {
  const counts = {};
  if (!client) return { counts, source: 'estimate' };
  if (!tokenCaches.has(client)) tokenCaches.set(client, new Map());
  const cache = tokenCaches.get(client);
  let missing = 0;
  for (const text of texts) {
    const key = sha256(text);
    if (!cache.has(key)) {
      try {
        const r = await client.request('model.tokenize', { text }, { timeoutMs: 60000 });
        if (Array.isArray(r?.tokens)) cache.set(key, r.tokens.length);
      } catch { /* estimated instead, and flagged */ }
    }
    if (cache.has(key)) counts[key] = cache.get(key);
    else missing++;
  }
  return { counts, source: missing ? (missing === texts.length ? 'estimate' : 'partial') : 'model' };
}

export const sequential = {
  initialState: () => ({ turn: 0, history: [] }),
  maxTries: (task) => task.turns.length,
  stepInfo: (state) => ({ k: state?.turn ?? 0 }),
  async advance(task, state, score) {
    const next = state.turn + 1;
    if (next >= task.turns.length) return null;
    return { state: { turn: next, history: [...state.history, { ...score.turn_record, passed: !!score.passed }] }, prompt: task.turns[next] };
  },
};

export const skillsel = {
  name: 'skillsel',
  title: 'Skill selection (which skills are read, when, and at what cost)',
  systems: ['cortex'],
  unsupportedSystems: { aider: 'skillsel measures CortexCode\'s skill reads; baselines have no skills', mini: 'skillsel measures CortexCode\'s skill reads; baselines have no skills' },
  tries: 1,
  defaultRuns: 3,
  defaultTimeoutMs: 10 * 60000,
  // A turn that runs past its budget is still scored on what it read.
  scoreAfterTimeout: true,
  reportNote: 'Pass = the turn met its soft expectations (every must group used, no must_not read, must reads before the first edit where required, no re-read of a body still in the conversation, no reads at all in negative scenarios); a multi-turn scenario passes when every turn does. No pass threshold is set in advance; the metrics in the skill-selection section are the result.',
  async listTasks(opts = {}) {
    const list = filterTasks(bank().scenarios, opts);
    return balancedLimit(list, opts.limit);
  },
  reportTask: (id) => scenarioById(id) ?? { id, group: null, lang: 'none', expect: [] },
  /** Re-exported for compare.mjs and the report. */
  rescoreFiles,
  prompt: (task) => task.turns[0],
  agentPath: () => [GO_BIN, path.join(JAVA_HOME, 'bin')],
  agentEnvExtra: () => ({ JAVA_HOME, GRADLE_USER_HOME: T.gradleAgentHome, GOCACHE: T.goAgentCache, GOTOOLCHAIN: 'local', GOPROXY: 'off', GOTELEMETRY: 'off' }),
  /** Per-task budget: the turn budget times the turns (or --timeout-min for the whole task). */
  taskTimeoutMs: (task, { overrideMs }) => overrideMs ?? task.timeout_ms,
  async makeWorkspace(task, dest) {
    const stage = `${dest}.fixture`;
    await stageFixture(task.fixture, stage);
    await materializeWorkspace(stage, dest);
    await rmrf(stage);
    return { snapshot: await snapshotSha(dest) };
  },
  hiddenFiles: () => [],
  /**
   * Score one turn from the journal snapshot the runner took when it ended (`extra.journals`),
   * measuring skill text sizes with the model's tokenizer while the frame is up.
   */
  async score(task, workspace, outDir, tag, scratch, state, extra = {}) {
    const k = state?.turn ?? 0;
    const history = state?.history ?? [];
    const turns = [...history.map((h) => h.turn), turnInfo(extra.turn)];
    const journals = extra.journals ?? {};
    const { counts, source } = await measureTokens(extra.client ?? null, textsToCount(journals, extra.sessionId));
    const res = scoreTurn({
      journals, mainSessionId: extra.sessionId ?? null, turns, turnIndex: k, expect: task.expect[k],
      foldedBefore: history.at(-1)?.folded ?? null, counts, stages: shippedDecisionStages(),
    });
    const prevPassed = history.every((h) => h.passed);
    const passed = res.passed && prevPassed;
    const out = {
      scenario: task.id, group: task.group, fixture: task.fixture, turn: k, prompt: task.turns[k], session_id: extra.sessionId ?? null,
      ...res, turn_passed: res.passed, passed, failure_class: passed ? null : (res.failure_class ?? 'skill_earlier_turn'),
      token_counts: counts, token_source: source,
    };
    await ensureDir(outDir);
    await writeJson(path.join(outDir, 'result.json'), out);
    await writeJson(path.join(outDir, 'scenario.json'), task);
    if (extra.runDir) {
      // One row per turn, the moment it ends.
      await appendJsonl(path.join(extra.runDir, 'results.jsonl'), {
        ts: new Date().toISOString(), scenario: task.id, group: task.group, turn: k, passed, miss_layer: out.miss_layer,
        route: res.route_path, reads: res.reads.map((r) => `${r.mark}${r.uri}`), selected: res.selected.map((s) => s.uri), injected: res.injected.map((s) => s.uri),
        recall: res.recall, precision: res.precision, wrong: res.wrong.length, wasted: res.wasted.length, duplicates: res.duplicates.total,
        late: res.late, clean: res.clean, folded_skills: res.folded_skills.length, offered: res.catalog.offered,
        skill_tokens: res.tokens.skill_text, prompt_tokens: res.tokens.prompt, cached_tokens: res.tokens.cached, model_calls: res.tokens.model_calls,
        seconds: res.wall_ms === null ? null : Math.round(res.wall_ms / 1000),
      });
    }
    const turnRecord = { turn: turnInfo(extra.turn), folded: foldedBySession(journals) };
    return {
      passed, failure_class: out.failure_class, miss_layer: out.miss_layer, recall: res.recall, precision: res.precision,
      wrong: res.wrong.length, wasted: res.wasted.length, late: res.late, clean: res.clean, feedback: null, turn_record: turnRecord,
    };
  },
  /** Report re-derivation from raw files (reportlib calls it per attempt). */
  rescore: (task, attemptDir) => rescoreFiles(attemptDir, { scenario: scenarioById(task.id) }),
  /** After every run: summary.md beside run.json. */
  async afterRun({ runDir }) {
    const run = await loadRunResults(runDir);
    await writeText(path.join(runDir, 'summary.md'), renderRunSummary(run));
  },
  followup: () => null,
  sequential,
};
