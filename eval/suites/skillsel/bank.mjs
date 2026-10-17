// Load and validate the skill-selection bank (bank.json). Validation fails loudly: every URI the
// bank names must exist in the shipped catalog (catalog.mjs), every glob must match at least one
// skill, every fixture must exist, and no expected URI may also be forbidden.
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { scanCatalog, shippedDecisionStages } from './catalog.mjs';
import { matches, matchesAny, SCHEME } from './score.mjs';

export const SUITE_DIR = path.dirname(fileURLToPath(import.meta.url));
export const BANK_FILE = path.join(SUITE_DIR, 'bank.json');
export const FIXTURES_DIR = path.join(SUITE_DIR, 'fixtures');

export class BankError extends Error {
  constructor(problems) {
    super(`skillsel bank is invalid (${problems.length} problem${problems.length === 1 ? '' : 's'}):\n  - ${problems.join('\n  - ')}`);
    this.problems = problems;
  }
}

/** Expand `@set` references in a URI list. */
function expandList(list, sets, where, problems) {
  if (list === undefined || list === null) return [];
  if (!Array.isArray(list)) { problems.push(`${where}: expected a list`); return []; }
  const out = [];
  for (const x of list) {
    if (typeof x !== 'string') { problems.push(`${where}: ${JSON.stringify(x)} is not a string`); continue; }
    if (x.startsWith('@')) {
      const set = sets[x.slice(1)];
      if (!Array.isArray(set)) { problems.push(`${where}: unknown set ${x}`); continue; }
      out.push(...set);
    } else {
      out.push(x);
    }
  }
  return [...new Set(out)];
}

function checkUris(uris, catalog, where, problems) {
  for (const u of uris) {
    if (!u.startsWith(SCHEME) || u === SCHEME) { problems.push(`${where}: ${u} is not a skill://<plugin>/<name> URI`); continue; }
    if (u.includes('*')) {
      if (![...catalog].some((c) => matches(c, u))) problems.push(`${where}: glob ${u} matches no skill in the catalog`);
    } else if (!catalog.has(u)) {
      problems.push(`${where}: ${u} does not exist in the catalog (plugins/lang-*/data/skills, plugins/skills-*/data/skills)`);
    }
  }
}

/** Route names a `route` / `decision` expectation may use, from the chat flow's schemas. */
function checkRoutes(list, allowed, field, where, problems) {
  if (list === undefined) return;
  if (!Array.isArray(list) || !list.length || !list.every((r) => typeof r === 'string')) {
    problems.push(`${where}.${field}: expected a non-empty list of route names`);
    return;
  }
  if (!allowed.length) return;
  for (const r of list) if (!allowed.includes(r)) problems.push(`${where}.${field}: ${r} is not a route the chat flow can take (${allowed.join(', ')})`);
}

/** One turn's expectations, normalized: must groups, may, must_not, flags, routes. */
function normalizeExpect(raw, sets, catalog, stages, where, problems) {
  const e = raw ?? {};
  const must = [];
  for (const [i, g] of (Array.isArray(e.must) ? e.must : []).entries()) {
    const group = expandList(typeof g === 'string' ? [g] : g, sets, `${where}.must[${i}]`, problems);
    if (!group.length) problems.push(`${where}.must[${i}]: empty group`);
    else must.push(group);
  }
  if (e.must !== undefined && !Array.isArray(e.must)) problems.push(`${where}.must: expected a list of groups`);
  const may = expandList(e.may, sets, `${where}.may`, problems);
  const mustNot = expandList(e.must_not, sets, `${where}.must_not`, problems);
  checkUris(must.flat(), catalog, `${where}.must`, problems);
  checkUris(may, catalog, `${where}.may`, problems);
  checkUris(mustNot, catalog, `${where}.must_not`, problems);
  for (const u of [...must.flat(), ...may]) {
    if (!u.includes('*') && matchesAny(u, mustNot)) problems.push(`${where}: ${u} is both expected (must/may) and forbidden (must_not)`);
  }
  if (e.no_skills && must.length) problems.push(`${where}: no_skills with must groups`);
  // `route`: the router's pick; `decision`: the turn's last decision (DECIDE's, or the router's
  // when nothing follows it), e.g. PLAN for a plan-first request whichever stage chose it.
  const decisionRoutes = [...new Set(Object.values(stages).flatMap((st) => st.routes))];
  checkRoutes(e.route, stages.route?.routes ?? [], 'route', where, problems);
  checkRoutes(e.decision, decisionRoutes, 'decision', where, problems);
  return {
    must, may, must_not: mustNot,
    before_first_edit: !!e.before_first_edit,
    no_skills: !!e.no_skills,
    ...(e.route ? { route: e.route } : {}),
    ...(e.decision ? { decision: e.decision } : {}),
  };
}

/**
 * Parse and validate a bank. Returns `{ scenarios, fixtures, catalog, raw }`; throws BankError
 * listing every problem. `catalog` (a Set of URIs), `fixturesDir` and `stages` (catalog.mjs
 * decisionStages(): the routes `route` / `decision` expectations may name) are injectable for tests.
 */
export function loadBank({ file = BANK_FILE, bank = null, catalog = null, fixturesDir = FIXTURES_DIR, stages = null } = {}) {
  const raw = bank ?? JSON.parse(fs.readFileSync(file, 'utf8'));
  const uris = catalog ?? scanCatalog().uris;
  const routeStages = stages ?? shippedDecisionStages();
  const problems = [];
  const sets = raw.sets ?? {};
  const fixtures = raw.fixtures ?? {};
  const budgetMin = Number(raw.defaults?.turn_budget_min ?? 10);
  for (const [name, f] of Object.entries(fixtures)) {
    if (f.base && !fixtures[f.base]) problems.push(`fixture ${name}: unknown base ${f.base}`);
    if (fixturesDir && !f.empty && !fs.existsSync(path.join(fixturesDir, name))) problems.push(`fixture ${name}: missing directory ${path.join(fixturesDir, name)}`);
  }
  const seen = new Set();
  const scenarios = [];
  for (const [i, s] of (raw.scenarios ?? []).entries()) {
    const where = `scenario ${s?.id ?? `#${i}`}`;
    if (!s?.id || typeof s.id !== 'string') { problems.push(`${where}: missing id`); continue; }
    if (seen.has(s.id)) problems.push(`${where}: duplicate id`);
    seen.add(s.id);
    if (!s.group || typeof s.group !== 'string') problems.push(`${where}: missing group`);
    if (!fixtures[s.fixture]) problems.push(`${where}: unknown fixture ${s.fixture}`);
    const turns = Array.isArray(s.turns) ? s.turns : [];
    if (!turns.length || !turns.every((t) => typeof t === 'string' && t.trim())) problems.push(`${where}: turns must be a non-empty list of prompts`);
    let expects;
    if (Array.isArray(s.expect)) {
      if (s.expect.length !== turns.length) problems.push(`${where}: ${s.expect.length} expectations for ${turns.length} turns`);
      expects = s.expect.map((e, k) => normalizeExpect(e, sets, uris, routeStages, `${where}.expect[${k}]`, problems));
    } else {
      const one = normalizeExpect(s.expect, sets, uris, routeStages, `${where}.expect`, problems);
      expects = turns.map(() => one);
    }
    const fixture = fixtures[s.fixture] ?? {};
    scenarios.push({
      id: s.id, group: s.group, fixture: s.fixture, lang: fixture.languages?.[0] ?? 'none', languages: fixture.languages ?? [],
      turns, expect: expects, note: s.note ?? null, prompt: turns[0],
      timeout_ms: Number(s.turn_budget_min ?? budgetMin) * 60000 * Math.max(1, turns.length),
    });
  }
  if (!scenarios.length) problems.push('no scenarios');
  if (problems.length) throw new BankError(problems);
  return { scenarios, fixtures, catalog: uris, raw };
}
