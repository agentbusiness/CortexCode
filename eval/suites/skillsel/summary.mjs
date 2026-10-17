// Skill-selection aggregates and tables: per run (summary.md), over runs (mean and spread, for the
// report and compare.mjs). Pure functions over the results loaded by results.mjs or the report's
// analyzed runs.
import { combineTurns } from './score.mjs';

const finite = (x) => typeof x === 'number' && Number.isFinite(x);

/** Mean, sample standard deviation (null below two values), min, max, n. */
export function stats(xs) {
  const v = xs.filter(finite);
  if (!v.length) return { n: 0, mean: null, sd: null, min: null, max: null };
  const mean = v.reduce((a, x) => a + x, 0) / v.length;
  const sd = v.length > 1 ? Math.sqrt(v.reduce((a, x) => a + (x - mean) ** 2, 0) / (v.length - 1)) : null;
  return { n: v.length, mean, sd, variance: sd === null ? null : sd * sd, min: Math.min(...v), max: Math.max(...v) };
}

const ratio = (a, b) => (b ? a / b : null);

/** Metrics of one run over its scenarios (micro-averaged: pooled groups, reads and turns). */
export function runMetrics(scenarios) {
  const S = scenarios.filter((s) => s.combined);
  const sum = (f) => S.reduce((a, s) => a + (f(s.combined) ?? 0), 0);
  const turns = sum((c) => c.turns);
  const negatives = S.filter((s) => s.combined.no_skills);
  return {
    scenarios: S.length,
    turns,
    pass_rate: ratio(S.filter((s) => s.combined.passed).length, S.length),
    recall: ratio(sum((c) => c.must_satisfied), sum((c) => c.must_groups)),
    recall_offered: ratio(sum((c) => c.offered_satisfied), sum((c) => c.offered_groups)),
    // Nothing chosen and nothing expected is precise (1.0); nothing chosen but something expected is undefined.
    precision: sum((c) => c.chosen) ? sum((c) => c.relevant_chosen) / sum((c) => c.chosen) : (S.length && !sum((c) => c.must_groups) ? 1 : null),
    wrong: sum((c) => c.wrong),
    wasted: sum((c) => c.wasted),
    duplicates: sum((c) => c.duplicates),
    duplicates_folded: sum((c) => c.duplicates_folded),
    duplicates_unfolded: sum((c) => c.duplicates_unfolded),
    duplicates_undetermined: sum((c) => c.duplicates_undetermined),
    late: sum((c) => c.late),
    negative_clean: ratio(negatives.filter((s) => s.combined.clean).length, negatives.length),
    catalog_misses: S.filter((s) => s.combined.must_groups && !s.combined.offered).length,
    folded_skills: sum((c) => c.folded_skills),
    reads_per_turn: ratio(sum((c) => c.reads), turns),
    skill_tokens_per_turn: ratio(sum((c) => c.skill_tokens), turns),
    skill_steps_per_turn: ratio(sum((c) => c.skill_steps), turns),
    prompt_tokens_per_turn: ratio(sum((c) => c.prompt_tokens), turns),
    cache_reuse: ratio(sum((c) => c.cached_tokens), sum((c) => c.prompt_tokens)),
    catalog_tokens: ratio(sum((c) => c.catalog_tokens), S.filter((s) => finite(s.combined.catalog_tokens)).length),
    wall_s_per_turn: ratio(sum((c) => c.wall_ms) / 1000, turns),
  };
}

export const METRICS = [
  ['scenarios', 'scenarios', 'n'],
  ['pass_rate', 'expectations met', 'pct'],
  ['recall', 'recall', 'r'],
  ['recall_offered', 'recall (offered only)', 'r'],
  ['precision', 'precision', 'r'],
  ['wrong', 'wrong reads', 'n'],
  ['wasted', 'wasted reads', 'n'],
  ['duplicates', 'duplicate reads', 'n'],
  ['duplicates_unfolded', '  of which body unfolded', 'n'],
  ['duplicates_folded', '  of which body folded', 'n'],
  ['duplicates_undetermined', '  of which undetermined', 'n'],
  ['late', 'late reads', 'n'],
  ['negative_clean', 'negative cleanliness', 'pct'],
  ['catalog_misses', 'catalog misses', 'n'],
  ['folded_skills', 'skill bodies folded', 'n'],
  ['reads_per_turn', 'skill reads / turn', 'f'],
  ['skill_tokens_per_turn', 'skill text tokens / turn', 'n'],
  ['skill_steps_per_turn', 'steps spent on skill reads / turn', 'f'],
  ['prompt_tokens_per_turn', 'prompt tokens / turn', 'n'],
  ['cache_reuse', 'prefix cache reuse', 'pct'],
  ['catalog_tokens', 'catalog size (tokens)', 'n'],
  ['wall_s_per_turn', 'wall s / turn', 'f'],
];

/** Scenario groups in bank order of appearance, then any others. */
const GROUP_ORDER = ['positive', 'multi', 'general', 'plan', 'negative', 'tempting', 'conversation', 'catalog'];
function groupsOf(runs) {
  const seen = new Set(runs.flatMap((r) => r.scenarios.map((s) => s.group).filter(Boolean)));
  return [...GROUP_ORDER.filter((g) => seen.has(g)), ...[...seen].filter((g) => !GROUP_ORDER.includes(g)).sort()];
}

/**
 * Aggregate several runs of the same bank: every metric per run, then mean and spread over runs,
 * overall, per group and per scenario.
 */
export function aggregateRuns(runs) {
  const per = (filter) => runs.map((r) => runMetrics(r.scenarios.filter(filter)));
  const statsOf = (perRun) => Object.fromEntries(METRICS.map(([k]) => [k, stats(perRun.map((m) => m[k]))]));
  const overall = statsOf(per(() => true));
  const byGroup = {};
  for (const g of groupsOf(runs)) byGroup[g] = statsOf(per((s) => s.group === g));
  const ids = [...new Set(runs.flatMap((r) => r.scenarios.map((s) => s.id)))];
  const byScenario = {};
  for (const id of ids) {
    const rows = runs.map((r) => r.scenarios.find((s) => s.id === id)).filter((s) => s?.combined);
    const c = rows.map((s) => s.combined);
    const reads = {};
    for (const s of rows) {
      const seen = new Set();
      for (const t of s.turns) for (const [uri, mark] of Object.entries(t.marks ?? {})) seen.add(`${mark}${uri}`);
      for (const m of seen) reads[m] = (reads[m] ?? 0) + 1;
    }
    byScenario[id] = {
      group: rows[0]?.group ?? runs.flatMap((r) => r.scenarios).find((s) => s.id === id)?.group ?? null,
      runs: rows.length,
      passed: c.filter((x) => x.passed).length,
      miss_layers: c.filter((x) => !x.passed).map((x) => x.miss_layer ?? 'unknown'),
      recall: stats(c.map((x) => x.recall)),
      precision: stats(c.map((x) => x.precision)),
      wrong: stats(c.map((x) => x.wrong)),
      wasted: stats(c.map((x) => x.wasted)),
      duplicates: stats(c.map((x) => x.duplicates)),
      late: stats(c.map((x) => x.late)),
      clean: c.some((x) => x.no_skills) ? stats(c.map((x) => (x.clean ? 1 : 0))) : null,
      offered: c.every((x) => x.offered),
      skill_tokens: stats(c.map((x) => x.skill_tokens)),
      prompt_tokens: stats(c.map((x) => x.prompt_tokens)),
      wall_s: stats(c.map((x) => x.wall_ms / 1000)),
      routes: [...new Set(c.flatMap((x) => x.routes))],
      reads,
    };
  }
  return { runs: runs.length, run_ids: runs.map((r) => r.run), overall, by_group: byGroup, by_scenario: byScenario };
}

/** Build the run shape aggregateRuns wants from the report's analyzed run (reportlib.analyzeRun). */
export function runFromAnalyzed(analyzed) {
  const scenarios = analyzed.tasks.filter((t) => !t.missing).map((t) => {
    const turns = t.attempts.map((a) => a.rescored).filter((r) => r?.ok);
    const combined = turns.length ? combineTurns(turns) : null;
    if (combined && t.attempts.length > turns.length) combined.passed = false;
    return { id: t.id, group: t.group ?? turns[0]?.group ?? null, turns, combined };
  });
  return { run: analyzed.run, model: analyzed.model, system: analyzed.system, scenarios };
}

// ---------- rendering ----------

function fmt(x, kind) {
  if (!finite(x)) return 'n/a';
  if (kind === 'pct') return `${(100 * x).toFixed(0)}%`;
  if (kind === 'r') return x.toFixed(2);
  if (kind === 'f') return x.toFixed(1);
  return Number.isInteger(x) ? String(x) : x.toFixed(1);
}

/** `mean ± sd` (sd only with two or more runs). */
export function fmtStat(s, kind) {
  if (!s || s.mean === null) return 'n/a';
  return s.sd === null ? fmt(s.mean, kind) : `${fmt(s.mean, kind)} ± ${fmt(s.sd, kind === 'pct' ? 'pct' : kind === 'n' ? 'f' : kind)}`;
}

function table(head, rows) {
  return [`| ${head.join(' | ')} |`, `| ${head.map(() => '---').join(' | ')} |`, ...rows.map((r) => `| ${r.join(' | ')} |`)].join('\n');
}

const short = (uri) => uri.replace(/^skill:\/\/[^/]+\//, '');

function readsCell(turn) {
  const parts = [];
  for (const r of turn.reads ?? []) parts.push(`${r.mark}${short(r.uri)}${r.ok === false ? '(failed)' : ''}${r.duplicate ? `(dup:${r.duplicate})` : ''}${r.folded ? '(folded)' : ''}${r.after_first_edit ? '(late)' : ''}`);
  for (const s of turn.selected ?? []) parts.push(`${s.mark}${short(s.uri)}[selected:${s.stage}]`);
  for (const s of turn.injected ?? []) parts.push(`${s.mark}${short(s.uri)}[injected${s.full ? '' : ':line'}]`);
  return parts.join(' ') || '-';
}

/** summary.md of one run: one row per turn, then totals per group. */
export function renderRunSummary(run) {
  const L = [];
  L.push(`# Skill selection: ${run.system ?? 'cortex'} / ${run.model ?? '?'} / run ${run.run}`, '');
  L.push('Marks: ✓ must, · may, ✗ wrong (must_not), ~ wasted. `dup:` re-read of a body the session already had (folded / unfolded / undetermined when it was read again); `late` read after the first edit.', '');
  const rows = [];
  for (const s of run.scenarios) {
    if (s.missing) { rows.push([s.id, '-', '-', 'not run', '-', '-', '-', '-', '-', '-']); continue; }
    if (!s.turns.length) { rows.push([s.id, s.group ?? '-', '-', `error: ${String(s.error ?? 'no turns').split('\n')[0].slice(0, 80)}`, '-', '-', '-', '-', '-', '-']); continue; }
    for (const t of s.turns) {
      rows.push([
        s.turns.length > 1 ? `${s.id}.${t.turn + 1}` : s.id, s.group ?? '-', t.route_path || '-', readsCell(t),
        fmt(t.recall, 'r'), fmt(t.precision, 'r'), t.no_skills ? (t.clean ? 'clean' : 'read') : '-',
        `${t.tokens.skill_text}${t.tokens.skill_text_estimated ? '~' : ''} / ${t.tokens.prompt}`,
        t.wall_ms === null ? '-' : String(Math.round(t.wall_ms / 1000)),
        t.passed ? 'yes' : `no (${t.miss_layer ?? 'earlier turn'})`,
      ]);
    }
  }
  L.push(table(['scenario', 'group', 'route', 'skills used', 'recall', 'precision', 'negative', 'skill tok / prompt tok', 's', 'met'], rows), '');
  L.push('Skill token counts marked `~` are estimated (4 characters a token) because the model tokenizer was not reachable.', '');
  L.push('## Totals per group', '');
  const groups = groupsOf([run]);
  const per = [['all', runMetrics(run.scenarios)], ...groups.map((g) => [g, runMetrics(run.scenarios.filter((s) => s.group === g))])];
  L.push(table(['metric', ...per.map(([g]) => g)], METRICS.map(([k, label, kind]) => [label, ...per.map(([, m]) => fmt(m[k], kind))])), '');
  return L.join('\n');
}

/** Report section body for one system/model: aggregates over runs, per group, per scenario. */
export function renderAggregate(agg) {
  const L = [];
  const spread = agg.runs > 1 ? `mean ± standard deviation over ${agg.runs} runs (${agg.run_ids.join(', ')})` : `one run (${agg.run_ids.join(', ')}); run it at least 3 times (--runs 3) before comparing`;
  L.push(`Metrics per run, then ${spread}. Recall and precision are pooled over the scenarios of a run (satisfied must groups / must groups; must-or-may skills / distinct skills used).`, '');
  const groups = Object.keys(agg.by_group);
  L.push(table(['metric', 'all', ...groups], METRICS.map(([k, label, kind]) => [label, fmtStat(agg.overall[k], kind), ...groups.map((g) => fmtStat(agg.by_group[g][k], kind))])), '');
  L.push('Per scenario:', '');
  L.push(table(['scenario', 'group', 'met', 'recall', 'precision', 'wrong', 'wasted', 'dup', 'late', 'negative clean', 'skill tok', 'prompt tok', 'wall s', 'routes', 'skills used (runs)'],
    Object.entries(agg.by_scenario).map(([id, s]) => [
      id, s.group ?? '-', `${s.passed}/${s.runs}${s.miss_layers.length ? ` (${[...new Set(s.miss_layers)].join(', ')})` : ''}${s.offered ? '' : ' catalog miss'}`,
      fmtStat(s.recall, 'r'), fmtStat(s.precision, 'r'), fmtStat(s.wrong, 'n'), fmtStat(s.wasted, 'n'), fmtStat(s.duplicates, 'n'), fmtStat(s.late, 'n'),
      s.clean ? fmtStat(s.clean, 'pct') : '-', fmtStat(s.skill_tokens, 'n'), fmtStat(s.prompt_tokens, 'n'), fmtStat(s.wall_s, 'f'),
      s.routes.join(', ') || '-',
      Object.entries(s.reads).sort((a, b) => b[1] - a[1]).map(([m, n]) => `${m[0]}${short(m.slice(1))}×${n}`).join(' ') || '-',
    ])), '');
  return L.join('\n');
}
