// Project-knowledge suite (docs/PROJECT-KNOWLEDGE-PLAN.md P7): does what a project learns reach
// the next session, and at what cost. Every scenario is a conversation of one or more turns on the
// bowling fixture through the real chat flow on the real frame; a turn may start a new conversation
// of the same project (`new_session`). Scoring is structural (score.mjs); the graph is read through
// the memory plugin's own API, and cards the scenario says to keep are answered through the chat
// flow's own API, as a person would.
import fs from 'node:fs';
import fsp from 'node:fs/promises';
import path from 'node:path';
import { balancedLimit, filterTasks } from '../../lib/adapter.mjs';
import { appendJsonl, ensureDir, rmrf, sha256, writeJson, writeText } from '../../lib/util.mjs';
import { materializeWorkspace, snapshotSha } from '../../lib/workspace.mjs';
import { FIXTURES_DIR, loadBank, seedItems } from './bank.mjs';
import { scoreTurn } from './score.mjs';
import { renderRunSummary } from './summary.mjs';

let bankCache = null;
export function bank() {
  bankCache ??= loadBank();
  return bankCache;
}

export function scenarioById(id) {
  try { return bank().scenarios.find((s) => s.id === id) ?? null; } catch { return null; }
}

/** Model token counts of a text (`model.tokenize`), characters / 4 when the model is not reachable. */
async function tokenCounter(client) {
  const cache = new Map();
  return async (text) => {
    const key = sha256(text);
    if (!cache.has(key)) {
      let n = null;
      if (client) {
        try {
          const r = await client.request('model.tokenize', { text }, { timeoutMs: 60000 });
          if (Array.isArray(r?.tokens)) n = r.tokens.length;
        } catch { /* estimated */ }
      }
      cache.set(key, n ?? Math.ceil(text.length / 4));
    }
    return cache.get(key);
  };
}

/** The project's knowledge items, history included (empty when memory does not answer). */
async function knowledgeOf(client, projectId) {
  if (!client || !projectId) return [];
  try {
    const r = await client.request('memory.knowledge.list', { project_id: projectId, include_history: true }, { timeoutMs: 60000 });
    return Array.isArray(r?.items) ? r.items : [];
  } catch {
    return [];
  }
}

export const sequential = {
  initialState: () => ({ turn: 0, history: [], kept: { ids: [], texts: [] } }),
  maxTries: (task) => task.turns.length,
  stepInfo: (state) => ({ k: state?.turn ?? 0 }),
  async advance(task, state, score) {
    const next = state.turn + 1;
    if (next >= task.turns.length) return null;
    const kept = score.kept ?? state.kept;
    return {
      state: { turn: next, history: [...state.history, { ...score.turn_record, passed: !!score.passed }], kept },
      prompt: task.turns[next],
      new_session: !!task.expect[next]?.new_session,
    };
  },
};

export const knowledge = {
  name: 'knowledge',
  title: 'Project knowledge (what a session learns reaches the next, and what it costs)',
  systems: ['cortex'],
  unsupportedSystems: { aider: 'knowledge measures CortexCode\'s project knowledge; baselines have none', mini: 'knowledge measures CortexCode\'s project knowledge; baselines have none' },
  tries: 1,
  defaultRuns: 3,
  defaultTimeoutMs: 15 * 60000,
  scoreAfterTimeout: true,
  reportNote: 'Pass = every structural expectation of every turn held (a card proposed or not, an item kept in one session recalled in the next, a refuted theory avoided, the README section read, the brief within its budget). K3 (a general statement) records a rate, not a target.',
  async listTasks(opts = {}) {
    const list = filterTasks(bank().scenarios, opts);
    return balancedLimit(list, opts.limit);
  },
  reportTask: (id) => scenarioById(id) ?? { id, group: null, lang: 'none', expect: [] },
  prompt: (task) => task.turns[0],
  agentPath: () => [],
  taskTimeoutMs: (task, { overrideMs }) => overrideMs ?? task.timeout_ms,
  async makeWorkspace(task, dest) {
    const stage = `${dest}.fixture`;
    await rmrf(stage);
    await ensureDir(stage);
    const src = path.join(FIXTURES_DIR, task.fixture);
    if (fs.existsSync(src)) await fsp.cp(src, stage, { recursive: true, force: true });
    await materializeWorkspace(stage, dest);
    await rmrf(stage);
    return { snapshot: await snapshotSha(dest) };
  },
  hiddenFiles: () => [],
  /** The scenario's seed, written through the memory plugin before the first turn, once the
   *  project is indexed (a seed that binds to code needs the symbols). */
  async afterOpen({ client, task, opened }) {
    if (!task.seed) return;
    const deadline = Date.now() + 120000;
    while (Date.now() < deadline) {
      const status = await client.request('memory.project.status', { project_id: opened.project.id }, { timeoutMs: 60000 }).catch(() => null);
      if (status?.state === 'ready') break;
      await new Promise((r) => setTimeout(r, 500));
    }
    const items = seedItems(bank().seeds[task.seed]);
    for (const item of items) {
      await client.request('memory.knowledge.upsert', { project_id: opened.project.id, ...item }, { timeoutMs: 60000 });
    }
  },
  /**
   * Score one turn from the journal snapshot, the graph and the workspace; then, when the scenario
   * says so, keep the cards the turn proposed (and accept its decisions) through the chat flow's
   * API, so the next turn can find them.
   */
  async score(task, workspace, outDir, tag, scratch, state, extra = {}) {
    const k = state?.turn ?? 0;
    const expect = task.expect[k] ?? {};
    const client = extra.client ?? null;
    const sessionId = extra.sessionId ?? null;
    const journals = extra.journals ?? {};
    const tokensOf = await tokenCounter(client);
    const brief = scoreTurn({ journals, sessionId, turn: extra.turn, expect: {}, kept: [] });
    const briefTokens = brief.brief_items ? await tokensOf(briefSection(journals, sessionId)) : 0;
    const keptAll = await knowledgeOf(client, extra.projectId);
    const keptBefore = state?.kept ?? { ids: [], texts: [] };
    const res = scoreTurn({ journals, sessionId, turn: extra.turn, expect, kept: keptAll, keptBefore, workspace, tokensOf: () => briefTokens });
    // Keep what the scenario says to keep: every card and decision proposed in this turn.
    const kept = { ids: [...keptBefore.ids], texts: [...keptBefore.texts] };
    const answered = [];
    if (expect.accept && client && sessionId) {
      for (const card of res.cards.filter((c) => c.state === 'proposed')) {
        try {
          const r = await client.request('chatflow.knowledge.answer', { session_id: sessionId, seq: card.seq, accept: true }, { timeoutMs: 60000 });
          answered.push({ seq: card.seq, kind: card.kind, result: r });
        } catch (cause) {
          answered.push({ seq: card.seq, kind: card.kind, error: String(cause) });
        }
      }
      for (const decision of res.decisions.filter((d) => d.state === 'proposed')) {
        try {
          const r = await client.request('chatflow.decision.answer', { session_id: sessionId, seq: decision.seq, accept: true }, { timeoutMs: 60000 });
          answered.push({ seq: decision.seq, kind: 'decision', result: r });
        } catch (cause) {
          answered.push({ seq: decision.seq, kind: 'decision', error: String(cause) });
        }
      }
      // What the graph holds now is what later turns may recall.
      for (const item of await knowledgeOf(client, extra.projectId)) {
        if (!kept.ids.includes(item.id)) { kept.ids.push(item.id); kept.texts.push(String(item.text ?? '')); }
      }
    }
    const prevPassed = (state?.history ?? []).every((h) => h.passed);
    const passed = res.passed && prevPassed;
    const out = {
      scenario: task.id, group: task.group, fixture: task.fixture, turn: k, prompt: task.turns[k], session_id: sessionId, new_session: !!expect.new_session,
      ...res, turn_passed: res.passed, passed, measured: !!expect.measured, answered, kept_ids: kept.ids,
      failure_class: passed ? null : (res.passed ? 'earlier_turn' : (res.checks.find((c) => !c.ok)?.name ?? 'expectation')),
    };
    await ensureDir(outDir);
    await writeJson(path.join(outDir, 'result.json'), out);
    await writeJson(path.join(outDir, 'scenario.json'), task);
    if (extra.runDir) {
      await appendJsonl(path.join(extra.runDir, 'results.jsonl'), {
        ts: new Date().toISOString(), scenario: task.id, group: task.group, turn: k, passed, route: res.route, teaches: res.teaches, settles: res.settles,
        cards: res.cards.length, decisions: res.decisions.length, brief_tokens: res.brief_tokens, brief_items: res.brief_items, note_lines: res.note_lines,
        attached: res.attached, checks: res.checks.map((c) => `${c.ok ? '✓' : '✗'} ${c.name}: ${c.detail}`),
      });
    }
    return { passed, failure_class: out.failure_class, checks: res.checks, kept, feedback: null, turn_record: { turn: extra.turn ?? null } };
  },
  rescore: async (task, attemptDir) => {
    const saved = JSON.parse(fs.readFileSync(path.join(attemptDir, 'score', 'result.json'), 'utf8'));
    return { ok: true, ...saved };
  },
  async afterRun({ runDir }) {
    const rows = fs.existsSync(path.join(runDir, 'results.jsonl')) ? fs.readFileSync(path.join(runDir, 'results.jsonl'), 'utf8').split('\n').filter(Boolean).map((l) => JSON.parse(l)) : [];
    await writeText(path.join(runDir, 'summary.md'), renderRunSummary(rows));
  },
  followup: () => null,
  sequential,
};

function briefSection(journals, sessionId) {
  const entries = (journals?.[sessionId]?.entries ?? []);
  const context = entries.find((e) => e?.kind === 'context');
  const system = typeof context?.data?.system === 'string' ? context.data.system : '';
  const at = system.indexOf('## What the project knows');
  if (at < 0) return '';
  const rest = system.slice(at);
  const end = rest.slice(1).search(/^## /m);
  return end >= 0 ? rest.slice(0, end + 1) : rest;
}
