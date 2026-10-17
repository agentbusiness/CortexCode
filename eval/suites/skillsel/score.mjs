// Skill-selection scoring: everything structural, from a project's journals (the runner's
// `journal.json` snapshots), never from words in the prompt or the answer.
//
// Three layers are recorded apart (docs/SKILL-SELECTION-TESTS.md):
//   - reads:      non-host `tool_call` entries with tool `read` and a `skill://` path (the lazy reads
//                 of today's design), with their step, result and size;
//   - selections: a `skills` array chosen by a decision (`route` entries, `detail.skills` or
//                 `skills`): ROUTE's, in the turn's first generation (journals from before that
//                 change carry DECIDE's, scored the same way); in an IMPLEMENT turn, the skills each
//                 task of the implemented `plan` chose (`tasks[].skills`);
//   - injections: skill bodies the host put into the conversation (`note` entries with purpose
//                 `skills`; their URIs from `skills`/`uris`/`uri`, else from the text).
// A skill is *used* when it was read successfully, selected or injected; the expectations are
// judged on the used set (recall) and on everything chosen (precision, wrong and wasted reads).
//
// Pure functions only: the suite adapter measures token counts with the model and passes them in.
import { sha256 } from '../../lib/util.mjs';

export const SCHEME = 'skill://';

/** `skill://<plugin>/<name>` in a canonical form (no `.md`, no slashes or a line range at the
 *  ends); `skill://` alone is the listing. Anything else: null. */
export function normalizeSkillUri(p) {
  if (typeof p !== 'string') return null;
  let s = p.trim();
  if (!s.startsWith(SCHEME)) return null;
  s = s.slice(SCHEME.length).replace(/:[\d,+-]+$/, '').replace(/^\/+|\/+$/g, '').replace(/\.md$/, '');
  return SCHEME + s;
}

/** Glob over URIs: `*` matches any run of characters (including `/`). */
export function globRegex(pattern) {
  return new RegExp(`^${pattern.split('*').map((s) => s.replace(/[.+?^${}()|[\]\\]/g, '\\$&')).join('.*')}$`);
}

const regexCache = new Map();
export function matches(uri, pattern) {
  if (!pattern.includes('*')) return uri === pattern;
  if (!regexCache.has(pattern)) regexCache.set(pattern, globRegex(pattern));
  return regexCache.get(pattern).test(uri);
}

export function matchesAny(uri, patterns) {
  return (patterns ?? []).some((p) => matches(uri, p));
}

/** The `## Skills` section of a system prompt: its text and the URIs it offers. */
export function parseCatalog(system) {
  const text = typeof system === 'string' ? system : '';
  const at = text.search(/^## Skills\b/m);
  if (at < 0) return { uris: [], text: '' };
  const rest = text.slice(at);
  const end = rest.slice(1).search(/^## /m);
  const section = end >= 0 ? rest.slice(0, end + 1).trimEnd() : rest.trimEnd();
  const uris = [];
  for (const m of section.matchAll(/skill:\/\/[^\s`)<>]+/g)) {
    const u = normalizeSkillUri(m[0]);
    if (u && u !== SCHEME && !uris.includes(u)) uris.push(u);
  }
  return { uris, text: section };
}

/**
 * The catalog a session offered: the `## Skills` section of its system prompt (older journals) or,
 * since the catalog moved to ROUTE's stage message, the context entry's `skill_lines`.
 */
export function catalogOf(context) {
  const fromPrompt = parseCatalog(context?.system);
  if (fromPrompt.uris.length) return fromPrompt;
  const lines = Array.isArray(context?.skill_lines) ? context.skill_lines : [];
  const uris = [];
  for (const line of lines) {
    const u = normalizeSkillUri(String(line).split(/\s+/)[0]);
    if (u && u !== SCHEME && !uris.includes(u)) uris.push(u);
  }
  return { uris, text: lines.join('\n') };
}

function parseArgs(args) {
  if (typeof args !== 'string') return args && typeof args === 'object' ? args : {};
  try { const v = JSON.parse(args); return v && typeof v === 'object' ? v : {}; } catch { return {}; }
}

function readTarget(args) {
  const a = parseArgs(args);
  const p = a.path ?? a.target ?? a.uri;
  return typeof p === 'string' ? p : null;
}

function asUriList(v) {
  if (!Array.isArray(v)) return null;
  const out = [];
  for (const x of v) {
    const raw = typeof x === 'string' ? x : (x && typeof x === 'object' ? (x.uri ?? x.skill ?? x.name) : null);
    if (typeof raw !== 'string') continue;
    // A bare `<plugin>/<name>` is accepted as the URI it names.
    const u = normalizeSkillUri(raw.startsWith(SCHEME) ? raw : SCHEME + raw);
    if (u && u !== SCHEME && !out.includes(u)) out.push(u);
  }
  return out;
}

/** Skills chosen by a decision (`route` entry), whichever field carries them. */
export function routeSkills(data) {
  return asUriList(data?.detail?.skills) ?? asUriList(data?.skills) ?? [];
}

/** Skills the tasks of a plan version chose (`tasks[].skills`), in task order, each once. */
export function planTaskSkills(data) {
  const out = [];
  for (const task of Array.isArray(data?.tasks) ? data.tasks : []) {
    for (const u of asUriList(task?.skills) ?? []) if (!out.includes(u)) out.push(u);
  }
  return out;
}

/** Skills a host `skills` note injected: listed fields first, else the URIs in its text. */
export function noteSkills(data) {
  const listed = asUriList(data?.skills) ?? asUriList(data?.uris) ?? (typeof data?.uri === 'string' ? asUriList([data.uri]) : null);
  if (listed) return listed;
  const out = [];
  for (const m of String(data?.text ?? '').matchAll(/skill:\/\/[^\s`)<>]+/g)) {
    const u = normalizeSkillUri(m[0]);
    if (u && u !== SCHEME && !out.includes(u)) out.push(u);
  }
  return out;
}

const str = (x) => (typeof x === 'string' ? x : '');
const numOr0 = (x) => (Number.isFinite(Number(x)) ? Number(x) : 0);

/**
 * Index one journals object (`{ <session_id>: { session, entries } }`) into per-session event
 * lists. `mainSessionId` is the chat session the harness sent the turns to.
 */
export function indexJournals(journals, mainSessionId) {
  const sessions = new Map();
  const ids = Object.keys(journals ?? {});
  const main = mainSessionId && ids.includes(mainSessionId) ? mainSessionId : ids[0] ?? null;
  for (const id of ids) {
    const entries = (journals[id]?.entries ?? []).filter((e) => e && Number.isFinite(e.seq)).sort((a, b) => a.seq - b.seq);
    let context = null;
    let newestResult = 0;
    const results = new Map();
    for (const e of entries) {
      if (e.kind === 'context') context = e;
      if (e.kind === 'tool_result') {
        newestResult = Math.max(newestResult, e.seq);
        const callId = str(e.data?.call_id);
        if (callId && !results.has(callId)) results.set(callId, e);
      }
    }
    const events = [];
    for (const e of entries) {
      const d = e.data ?? {};
      const base = { session: id, seq: e.seq, ts: numOr0(e.ts_ms) };
      if (e.kind === 'tool_call' && !d.host) {
        const tool = str(d.tool) || str(d.raw_name);
        const target = tool === 'read' ? readTarget(d.args ?? d.raw_arguments) : null;
        const uri = normalizeSkillUri(target);
        const res = results.get(str(d.call_id));
        events.push({
          ...base, type: 'call', tool, uri, call_id: str(d.call_id), run_id: str(d.run_id), step: Number.isFinite(d.step) ? d.step : null,
          ok: res ? res.data?.ok !== false : null, result_seq: res?.seq ?? null, output: res ? str(res.data?.output) : null,
        });
      } else if (e.kind === 'route') {
        events.push({ ...base, type: 'route', stage: str(d.stage), route: str(d.route), run_id: str(d.run_id), plan_id: str(d.plan_id), skills: routeSkills(d) });
      } else if (e.kind === 'plan') {
        events.push({ ...base, type: 'plan', plan_id: str(d.plan_id), implement_run: str(d.implement_run), skills: planTaskSkills(d) });
      } else if (e.kind === 'note' && str(d.purpose) === 'skills') {
        events.push({ ...base, type: 'inject', run_id: str(d.run_id), skills: noteSkills(d), text: str(d.text), to_model: d.host_message !== false });
      } else if (e.kind === 'tool_result') {
        // A skill the tools attached to a result by diagnostic code (plugins/tools codeskills.rs):
        // its URIs are in the result's `display.skills` (the inline text names only the code). Older
        // journals carry them inline: "[skill for E0382: skill://lang-rust/rust-borrow-checker]".
        const listed = Array.isArray(d.display?.skills) ? d.display.skills.map((uri) => normalizeSkillUri(str(uri))) : null;
        const attached = (listed ?? [...str(d.output).matchAll(/^\[skill for [^:\]]+: (skill:\/\/[^\s\]]+)\]$/gm)].map((m) => normalizeSkillUri(m[1]))).filter(Boolean);
        if (attached.length) events.push({ ...base, type: 'inject', run_id: str(d.run_id), skills: attached, text: str(d.output), to_model: true, by: 'code' });
      } else if (e.kind === 'usage') {
        events.push({
          ...base, type: 'usage', stage: str(d.stage), run_id: str(d.run_id),
          prompt: numOr0(d.prompt_tokens), cached: numOr0(d.cached_tokens), completion: numOr0(d.completion_tokens),
          prefill_ms: numOr0(d.prefill_ms), decode_ms: numOr0(d.decode_ms), wall_ms: numOr0(d.wall_ms),
        });
      }
    }
    sessions.set(id, {
      id, main: id === main, entries, context, events, newestResult,
      folded_through: numOr0(context?.data?.folded_through),
      system: str(context?.data?.system),
      skill_lines: Array.isArray(context?.data?.skill_lines) ? context.data.skill_lines : [],
    });
  }
  return { main, sessions };
}

/** Each session's `folded_through` (the context guard's fold mark) in a snapshot. */
export function foldedBySession(journals) {
  const out = {};
  for (const [id, s] of indexJournals(journals, null).sessions) out[id] = { folded_through: s.folded_through, newest_result: s.newestResult };
  return out;
}

/** Order across sessions: wall clock first, journal order within one session. */
function before(a, b) {
  if (a.session === b.session) return a.seq < b.seq;
  return a.ts !== b.ts ? a.ts < b.ts : a.seq < b.seq;
}

/**
 * Which turn (0-based) an event belongs to. Main-session entries by journal seq (each turn starts
 * at the seq the harness read before sending it); other sessions (delegated agents) by wall clock.
 */
function turnOf(ev, turns, mainId) {
  for (let j = turns.length - 1; j >= 0; j--) {
    const t = turns[j] ?? {};
    if (ev.session === mainId ? ev.seq >= numOr0(t.from_seq) : ev.ts >= numOr0(t.started_ms)) return j;
  }
  return -1;
}

/** Texts whose size in model tokens the score reports (skill bodies read or injected, the catalog). */
export function textsToCount(journals, mainSessionId) {
  const idx = indexJournals(journals, mainSessionId);
  const texts = [];
  for (const s of idx.sessions.values()) {
    for (const ev of s.events) {
      if (ev.type === 'call' && ev.uri && ev.uri !== SCHEME && ev.ok && ev.output) texts.push(ev.output);
      if (ev.type === 'inject' && ev.text) texts.push(ev.text);
    }
  }
  const main = idx.sessions.get(idx.main);
  const catalog = catalogOf(main);
  if (catalog.text) texts.push(catalog.text);
  return [...new Set(texts)];
}

/** Token count of a text: measured (`counts`: sha256 -> tokens) or estimated at 4 chars a token. */
export function tokensOf(text, counts) {
  if (!text) return { tokens: 0, estimated: false };
  const n = counts?.[sha256(text)];
  if (Number.isFinite(n)) return { tokens: n, estimated: false };
  return { tokens: Math.ceil(text.length / 4), estimated: true };
}

/** Mark per doc: ✓ must, · may, ✗ wrong (must_not), ~ wasted (neither). */
export function markOf(uri, expect) {
  if (matchesAny(uri, (expect.must ?? []).flat())) return '✓';
  if (matchesAny(uri, expect.must_not)) return '✗';
  if (matchesAny(uri, expect.may)) return '·';
  return '~';
}

/**
 * Score turn `turnIndex` of a scenario from the journal snapshot taken when that turn ended.
 *
 *   journals        snapshot after the turn (all sessions of the project)
 *   mainSessionId   the chat session the turns went to
 *   turns           [{from_seq, started_ms, ended_ms}] for turns 0..turnIndex (the runner's turn records)
 *   expect          the turn's expectations (bank.mjs: must groups, may, must_not, before_first_edit,
 *                   no_skills, route)
 *   foldedBefore    foldedBySession() of the snapshot that ended the previous turn (null for turn 0)
 *   counts          sha256(text) -> model tokens (missing texts are estimated and flagged)
 *   stages          catalog.mjs decisionStages(): per decision stage, the routes on which its
 *                   `skills` field is live (today ROUTE's ANSWER, LOOKUP, DO, DELEGATE and PLAN;
 *                   DECIDE has none). A decision on such a route reached a point where skills are
 *                   chosen even when it chose none, so a miss there is a selection miss, not a route
 *                   miss. Null: only reads, selections and injections show that the turn reached a
 *                   choice.
 */
export function scoreTurn({ journals, mainSessionId, turns, turnIndex = turns.length - 1, expect, foldedBefore = null, counts = {}, stages = null }) {
  const idx = indexJournals(journals, mainSessionId);
  const mainId = idx.main;
  const main = idx.sessions.get(mainId);
  const all = [...idx.sessions.values()].flatMap((s) => s.events.map((ev) => ({ ...ev, turn: turnOf(ev, turns, mainId) })));
  const inTurn = all.filter((ev) => ev.turn === turnIndex);
  const must = expect.must ?? [];
  const relevant = [...must.flat(), ...(expect.may ?? [])];

  // Catalog the session offered (the main session's frozen prefix).
  const catalog = catalogOf(main);
  const catalogTok = tokensOf(catalog.text, counts);
  const offeredGroups = must.map((g) => catalog.uris.some((u) => matchesAny(u, g)));
  const offered = offeredGroups.every(Boolean);

  // Layers.
  const calls = inTurn.filter((ev) => ev.type === 'call');
  const reads = calls.filter((ev) => ev.uri && ev.uri !== SCHEME);
  const listings = calls.filter((ev) => ev.uri === SCHEME).length;
  // IMPLEMENT: each plan task chose the skills of its own work (chatflow flow.rs `implement`), and
  // only those reach its run (the foreground DO decision, or the agent's task brief); the planning
  // decision's skills stay with the PLAN turn. The plan is the version this run implements
  // (`implement_run`), else the newest one before the IMPLEMENT route (of the named plan).
  const planChoices = inTurn.filter((ev) => ev.type === 'route' && ev.route === 'IMPLEMENT').flatMap((imp) => {
    const plans = all.filter((ev) => ev.type === 'plan' && ev.session === imp.session);
    const plan = plans.filter((p) => p.implement_run && p.implement_run === imp.run_id).at(-1)
      ?? plans.filter((p) => p.seq < imp.seq && (!imp.plan_id || p.plan_id === imp.plan_id)).at(-1);
    return plan ? [{ ...imp, stage: 'plan_task', skills: plan.skills }] : [];
  });
  const selections = [...inTurn.filter((ev) => ev.type === 'route'), ...planChoices].flatMap((ev) => ev.skills.map((uri) => ({ ...ev, uri, via: 'select' })));
  const injections = inTurn.filter((ev) => ev.type === 'inject').flatMap((ev) => ev.skills.map((uri, i) => ({ ...ev, uri, via: 'inject', first: i === 0 })));
  const uses = [...reads.filter((r) => r.ok).map((r) => ({ ...r, via: 'read' })), ...selections, ...injections].sort((a, b) => (before(a, b) ? -1 : before(b, a) ? 1 : 0));
  const chosen = [...new Set([...reads.map((r) => r.uri), ...selections.map((s) => s.uri), ...injections.map((s) => s.uri)])].sort();

  // First edit of the turn (any session), in journal / wall-clock order.
  const edits = calls.filter((ev) => ev.tool === 'edit').sort((a, b) => (before(a, b) ? -1 : 1));
  const firstEdit = edits[0] ?? null;

  // Recall and timing.
  const satisfied = must.map((g) => uses.some((u) => matchesAny(u.uri, g)));
  const recall = must.length ? satisfied.filter(Boolean).length / must.length : null;
  const offeredIdx = must.map((_, i) => i).filter((i) => offeredGroups[i]);
  const recallOffered = offeredIdx.length ? offeredIdx.filter((i) => satisfied[i]).length / offeredIdx.length : null;
  let late = 0;
  if (expect.before_first_edit) {
    for (const g of must) {
      const onTime = uses.some((u) => matchesAny(u.uri, g) && (!firstEdit || before(u, firstEdit)));
      if (!onTime) late++;
    }
  }

  // Precision, wrong and wasted (over everything chosen in the turn, all layers).
  const relevantChosen = chosen.filter((u) => matchesAny(u, relevant));
  const precision = chosen.length ? relevantChosen.length / chosen.length : (must.length ? null : 1);
  const wrong = chosen.filter((u) => matchesAny(u, expect.must_not));
  const wasted = chosen.filter((u) => !matchesAny(u, relevant));

  // Duplicates: a successful read of a skill whose body this session already had (an earlier
  // successful read or an injection), in this turn or an earlier one; split by whether that body
  // had been folded by the context guard when the read was made.
  const bodies = all.filter((ev) => (ev.type === 'call' && ev.uri && ev.uri !== SCHEME && ev.ok) || ev.type === 'inject')
    .flatMap((ev) => (ev.type === 'inject' ? ev.skills.map((uri) => ({ ...ev, uri, body_seq: ev.seq, foldable: false })) : [{ ...ev, body_seq: ev.result_seq ?? ev.seq, foldable: true }]));
  const duplicates = { total: 0, folded: 0, unfolded: 0, undetermined: 0, items: [] };
  for (const r of reads.filter((x) => x.ok)) {
    const earlier = bodies.filter((b) => b.session === r.session && b.uri === r.uri && b.seq < r.seq);
    if (!earlier.length) continue;
    const prev = earlier.at(-1);
    const sess = idx.sessions.get(r.session);
    let cause;
    if (!prev.foldable || prev.body_seq > r.seq) {
      cause = 'unfolded';
    } else {
      const fb = foldedBefore?.[r.session];
      const prevTurnStart = r.session === mainId ? numOr0(turns[r.turn]?.from_seq) : Infinity;
      // Folded before this turn began: the previous turn's snapshot already covers the body.
      if (fb && prev.body_seq <= fb.folded_through && prev.body_seq !== fb.newest_result && prev.seq < prevTurnStart) cause = 'folded';
      // Still not folded when this turn ended: it was in the conversation when it was read again.
      else if (prev.body_seq > sess.folded_through || prev.body_seq === sess.newestResult) cause = 'unfolded';
      else cause = 'undetermined';
    }
    duplicates.total++;
    duplicates[cause]++;
    duplicates.items.push({ uri: r.uri, seq: r.seq, step: r.step, earlier_seq: prev.seq, cause });
  }

  // Skill bodies the context guard folded away by the end of the turn (only tool results fold).
  const folded = [];
  for (const r of reads.filter((x) => x.ok && x.result_seq)) {
    const s = idx.sessions.get(r.session);
    if (r.result_seq <= s.folded_through && r.result_seq !== s.newestResult && !folded.includes(r.uri)) folded.push(r.uri);
  }

  // Cost.
  const usage = inTurn.filter((ev) => ev.type === 'usage');
  const sumU = (k) => usage.reduce((a, u) => a + u[k], 0);
  let skillTokens = 0;
  let skillEstimated = false;
  const readRows = reads.map((r) => {
    const t = r.ok ? tokensOf(r.output, counts) : { tokens: 0, estimated: false };
    skillTokens += t.tokens;
    skillEstimated ||= t.estimated;
    const dup = duplicates.items.find((d) => d.seq === r.seq);
    return {
      uri: r.uri, session: r.session, main: r.session === mainId, step: r.step, seq: r.seq, ok: r.ok, mark: markOf(r.uri, expect),
      tokens: t.tokens, chars: r.output?.length ?? 0, folded: r.ok && folded.includes(r.uri), duplicate: dup?.cause ?? null,
      after_first_edit: !!firstEdit && before(firstEdit, r),
    };
  });
  for (const inj of inTurn.filter((ev) => ev.type === 'inject')) {
    const t = tokensOf(inj.text, counts);
    skillTokens += t.tokens;
    skillEstimated ||= t.estimated;
  }
  // Model steps whose every call was a skill read (steps spent on reading skills).
  const bySteps = new Map();
  for (const c of calls) {
    if (c.step === null) continue;
    const key = `${c.session}\0${c.run_id}\0${c.step}`;
    const v = bySteps.get(key) ?? { all: true };
    v.all &&= !!c.uri;
    bySteps.set(key, v);
  }
  const prompt = sumU('prompt');
  const cached = sumU('cached');

  // Route of the turn (main session): the router's pick, then every decision; the last one is the
  // turn's outcome (PLAN writes a plan, DO changes code, ANSWER replies).
  const routes = inTurn.filter((ev) => ev.type === 'route' && ev.session === mainId);
  const route = routes.find((r) => r.stage === 'route')?.route ?? routes[0]?.route ?? null;
  const decision = routes.at(-1)?.route ?? null;
  const routePath = routes.map((r) => (r.stage === 'route' ? r.route : `${r.stage}:${r.route}`)).join(' > ');
  const routerOk = expect.route?.length ? (route !== null && expect.route.includes(route)) : null;
  const decisionOk = expect.decision?.length ? (decision !== null && expect.decision.includes(decision)) : null;
  const routeOk = routerOk === null && decisionOk === null ? null : routerOk !== false && decisionOk !== false;
  const choosing = inTurn.some((ev) => ev.type === 'route' && (stages?.[ev.stage]?.skills_on ?? []).includes(ev.route));
  const reached = calls.length > 0 || selections.length > 0 || injections.length > 0 || choosing;

  const clean = chosen.length === 0;
  const dupUnfolded = duplicates.unfolded;
  const passed = (must.length === 0 || offered) && (recall === null || recall === 1) && wrong.length === 0 && late === 0
    && dupUnfolded === 0 && (!expect.no_skills || clean) && routeOk !== false;
  let layer = null;
  if (!passed) {
    if (must.length && !offered) layer = 'catalog';
    else if (routeOk === false || (recall !== null && recall < 1 && !reached)) layer = 'route';
    else if ((recall !== null && recall < 1) || wrong.length || late || (expect.no_skills && !clean)) layer = 'selection';
    else layer = 'retention';
  }

  const t = turns[turnIndex] ?? {};
  return {
    turn: turnIndex, passed, miss_layer: layer, failure_class: layer ? `skill_${layer}` : null,
    route, decision, route_path: routePath, route_ok: routeOk, router_ok: routerOk, decision_ok: decisionOk, reached,
    catalog: { size: catalog.uris.length, tokens: catalogTok.tokens, tokens_estimated: catalogTok.estimated, offered, not_offered: must.filter((_, i) => !offeredGroups[i]) },
    reads: readRows, listings,
    selected: selections.map((s) => ({ uri: s.uri, stage: s.stage, seq: s.seq, mark: markOf(s.uri, expect) })),
    injected: injections.map((s) => ({ uri: s.uri, seq: s.seq, full: s.first, mark: markOf(s.uri, expect) })),
    chosen, marks: Object.fromEntries(chosen.map((u) => [u, markOf(u, expect)])),
    must_groups: must.length, must_satisfied: satisfied.filter(Boolean).length, recall, recall_offered: recallOffered,
    offered_groups: offeredIdx.length, offered_satisfied: offeredIdx.filter((i) => satisfied[i]).length,
    precision, relevant_chosen: relevantChosen.length, wrong, wasted,
    duplicates: { total: duplicates.total, folded: duplicates.folded, unfolded: duplicates.unfolded, undetermined: duplicates.undetermined, items: duplicates.items },
    late, first_edit: firstEdit ? { seq: firstEdit.seq, step: firstEdit.step, session: firstEdit.session } : null,
    no_skills: !!expect.no_skills, clean, folded_skills: folded,
    tokens: {
      skill_text: skillTokens, skill_text_estimated: skillEstimated, prompt, cached, completion: sumU('completion'),
      prefill_ms: sumU('prefill_ms'), decode_ms: sumU('decode_ms'), model_calls: usage.length,
      skill_steps: [...bySteps.values()].filter((v) => v.all).length, cache_reuse: prompt ? cached / prompt : null,
    },
    wall_ms: Number.isFinite(t.ended_ms) && Number.isFinite(t.started_ms) ? t.ended_ms - t.started_ms : null,
  };
}

/** A scenario's result over its turns: summed counts, pooled ratios, all turns must pass. */
export function combineTurns(turnResults) {
  const T = turnResults.filter(Boolean);
  const sum = (f) => T.reduce((a, t) => a + (f(t) ?? 0), 0);
  const groups = sum((t) => t.must_groups);
  const chosen = sum((t) => t.chosen.length);
  const offeredGroups = sum((t) => t.offered_groups);
  const offeredSatisfied = sum((t) => t.offered_satisfied);
  const anyNoSkills = T.some((t) => t.no_skills);
  return {
    turns: T.length,
    passed: T.length > 0 && T.every((t) => t.passed),
    miss_layer: T.find((t) => !t.passed)?.miss_layer ?? null,
    offered: T.every((t) => t.catalog.offered),
    must_groups: groups, must_satisfied: sum((t) => t.must_satisfied),
    recall: groups ? sum((t) => t.must_satisfied) / groups : null,
    recall_offered: offeredGroups ? offeredSatisfied / offeredGroups : null,
    offered_groups: offeredGroups, offered_satisfied: offeredSatisfied,
    chosen, relevant_chosen: sum((t) => t.relevant_chosen),
    precision: chosen ? sum((t) => t.relevant_chosen) / chosen : (groups ? null : 1),
    wrong: sum((t) => t.wrong.length), wasted: sum((t) => t.wasted.length),
    duplicates: sum((t) => t.duplicates.total), duplicates_folded: sum((t) => t.duplicates.folded),
    duplicates_unfolded: sum((t) => t.duplicates.unfolded), duplicates_undetermined: sum((t) => t.duplicates.undetermined),
    late: sum((t) => t.late), folded_skills: sum((t) => t.folded_skills.length),
    no_skills: anyNoSkills, clean: anyNoSkills ? T.filter((t) => t.no_skills).every((t) => t.clean) : null,
    reads: sum((t) => t.reads.length), selected: sum((t) => t.selected.length), injected: sum((t) => t.injected.length),
    skill_tokens: sum((t) => t.tokens.skill_text), skill_steps: sum((t) => t.tokens.skill_steps),
    prompt_tokens: sum((t) => t.tokens.prompt), cached_tokens: sum((t) => t.tokens.cached),
    completion_tokens: sum((t) => t.tokens.completion), model_calls: sum((t) => t.tokens.model_calls),
    catalog_size: T[0]?.catalog.size ?? null, catalog_tokens: T[0]?.catalog.tokens ?? null,
    wall_ms: sum((t) => t.wall_ms),
    routes: T.map((t) => t.route_path || '-'),
    marks: T.map((t) => t.marks),
  };
}
