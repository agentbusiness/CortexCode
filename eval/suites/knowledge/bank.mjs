// Load and validate the project-knowledge bank (bank.json). Validation fails loudly: every
// scenario needs a fixture that exists, one expectation per turn, a seed that exists when named,
// and expectations made only of the fields the scorer knows.
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

export const SUITE_DIR = path.dirname(fileURLToPath(import.meta.url));
export const BANK_FILE = path.join(SUITE_DIR, 'bank.json');
export const FIXTURES_DIR = path.join(SUITE_DIR, 'fixtures');

/** The expectation fields the scorer (score.mjs) reads, with their types. */
export const EXPECT_FIELDS = {
  teaches: 'boolean', settles: 'boolean', kinds: 'string[]', accept: 'boolean', new_session: 'boolean', measured: 'boolean',
  recalled: 'boolean', file_contains: 'object', edits_only: 'string[]', read_carries_knowledge: 'string', section_read: 'string',
  answer_contains_any: 'string[]', brief_max_tokens: 'number', note_max_lines: 'number',
};

export class BankError extends Error {
  constructor(problems) {
    super(`knowledge bank is invalid (${problems.length} problem${problems.length === 1 ? '' : 's'}):\n  - ${problems.join('\n  - ')}`);
    this.problems = problems;
  }
}

function typeOk(value, type) {
  if (type === 'boolean') return typeof value === 'boolean';
  if (type === 'number') return typeof value === 'number' && Number.isFinite(value);
  if (type === 'string') return typeof value === 'string' && value.trim() !== '';
  if (type === 'string[]') return Array.isArray(value) && value.length > 0 && value.every((x) => typeof x === 'string' && x.trim());
  if (type === 'object') return value && typeof value === 'object' && !Array.isArray(value);
  return false;
}

function normalizeExpect(raw, where, problems) {
  const e = raw ?? {};
  const out = {};
  for (const [key, value] of Object.entries(e)) {
    const type = EXPECT_FIELDS[key];
    if (!type) { problems.push(`${where}.${key}: not an expectation the scorer knows (${Object.keys(EXPECT_FIELDS).join(', ')})`); continue; }
    if (!typeOk(value, type)) { problems.push(`${where}.${key}: expected ${type}`); continue; }
    out[key] = value;
  }
  if (out.file_contains && (typeof out.file_contains.path !== 'string' || !Array.isArray(out.file_contains.any) || !out.file_contains.any.length)) {
    problems.push(`${where}.file_contains: needs {path, any: [strings]}`);
  }
  if (out.kinds && out.teaches === false) problems.push(`${where}: kinds with teaches false`);
  return out;
}

/**
 * Parse and validate a bank. Returns `{ scenarios, fixtures, seeds, raw }`; throws BankError listing
 * every problem. `fixturesDir` is injectable for tests.
 */
export function loadBank({ file = BANK_FILE, bank = null, fixturesDir = FIXTURES_DIR } = {}) {
  const raw = bank ?? JSON.parse(fs.readFileSync(file, 'utf8'));
  const problems = [];
  const fixtures = raw.fixtures ?? {};
  const seeds = raw.seeds ?? {};
  const budgetMin = Number(raw.defaults?.turn_budget_min ?? 15);
  for (const [name, f] of Object.entries(fixtures)) {
    if (fixturesDir && !fs.existsSync(path.join(fixturesDir, name))) problems.push(`fixture ${name}: missing directory ${path.join(fixturesDir, name)}`);
    if (!Array.isArray(f.languages)) problems.push(`fixture ${name}: languages must be a list`);
  }
  for (const [name, seed] of Object.entries(seeds)) {
    const ok = Array.isArray(seed) ? seed.every((item) => item && typeof item.kind === 'string' && typeof item.name === 'string' && typeof item.text === 'string')
      : seed && typeof seed === 'object' && Number.isFinite(seed.count) && typeof seed.kind === 'string' && typeof seed.name === 'string' && typeof seed.text === 'string';
    if (!ok) problems.push(`seed ${name}: a list of items {kind, name, text, ...} or a generator {count, kind, name, text}`);
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
    if (s.seed !== undefined && !seeds[s.seed]) problems.push(`${where}: unknown seed ${s.seed}`);
    const turns = Array.isArray(s.turns) ? s.turns : [];
    if (!turns.length || !turns.every((t) => typeof t === 'string' && t.trim())) problems.push(`${where}: turns must be a non-empty list of prompts`);
    const expects = Array.isArray(s.expect) ? s.expect : turns.map(() => s.expect);
    if (expects.length !== turns.length) problems.push(`${where}: ${expects.length} expectations for ${turns.length} turns`);
    const normalized = expects.map((e, k) => normalizeExpect(e, `${where}.expect[${k}]`, problems));
    if (normalized[0]?.new_session) problems.push(`${where}.expect[0]: the first turn cannot start a new session`);
    const fixture = fixtures[s.fixture] ?? {};
    scenarios.push({
      id: s.id, group: s.group, fixture: s.fixture, seed: s.seed ?? null, lang: fixture.languages?.[0] ?? 'none', languages: fixture.languages ?? [],
      turns, expect: normalized, note: s.note ?? null, prompt: turns[0],
      timeout_ms: Number(s.turn_budget_min ?? budgetMin) * 60000 * Math.max(1, turns.length),
    });
  }
  if (!scenarios.length) problems.push('no scenarios');
  if (problems.length) throw new BankError(problems);
  return { scenarios, fixtures, seeds, raw };
}

/** The items a seed stands for: the list itself, or `count` items from the generator's templates. */
export function seedItems(seed) {
  if (Array.isArray(seed)) return seed;
  return Array.from({ length: seed.count }, (_, i) => {
    const n = String(i + 1);
    return { kind: seed.kind, name: seed.name.replaceAll('{n}', n), text: seed.text.replaceAll('{n}', n), source: seed.source ?? 'user', by: 'seed' };
  });
}
