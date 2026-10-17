#!/usr/bin/env node
// Compare skill-selection runs scenario by scenario.
//
//   node eval/suites/skillsel/compare.mjs <path> <path> [...] [--out FILE] [--json]
//
// Each <path> is one run directory (…/skillsel/cortex/<model>/run-1) or a model directory, whose
// run-* children are pooled (mean and spread over runs). Every number is re-derived from the run's
// journals (results.mjs), so a corrected bank re-scores old runs.
import path from 'node:path';
import { parseArgs, writeText } from '../../lib/util.mjs';
import { loadRunResults, runDirsUnder } from './results.mjs';
import { aggregateRuns, fmtStat, METRICS } from './summary.mjs';

const changed = (a, b, eps = 1e-9) => a !== null && b !== null && Math.abs(a - b) > eps;

/** Load and aggregate every input. */
export async function loadInputs(paths) {
  const inputs = [];
  for (const p of paths) {
    const dirs = runDirsUnder(path.resolve(p));
    if (!dirs.length) throw new Error(`${p}: no run directory (run.json) here or in run-*/`);
    const runs = [];
    for (const d of dirs) runs.push(await loadRunResults(d));
    inputs.push({ label: p, runs, agg: aggregateRuns(runs) });
  }
  return inputs;
}

function table(head, rows) {
  return [`| ${head.join(' | ')} |`, `| ${head.map(() => '---').join(' | ')} |`, ...rows.map((r) => `| ${r.join(' | ')} |`)].join('\n');
}

/** Markdown comparison: aggregate metrics side by side, then one row per scenario. */
export function renderComparison(inputs) {
  const L = ['# Skill-selection comparison', ''];
  L.push(inputs.map((x, i) => `${String.fromCharCode(65 + i)} = \`${x.label}\` (${x.agg.runs} run${x.agg.runs === 1 ? '' : 's'})`).join('  \n'), '');
  const names = inputs.map((_, i) => String.fromCharCode(65 + i));
  L.push('## Overall', '');
  L.push(table(['metric', ...names], METRICS.map(([k, label, kind]) => [label, ...inputs.map((x) => fmtStat(x.agg.overall[k], kind))])), '');
  const groups = [...new Set(inputs.flatMap((x) => Object.keys(x.agg.by_group)))];
  for (const g of groups) {
    L.push(`## Group ${g}`, '');
    const keys = ['pass_rate', 'recall', 'precision', 'wrong', 'wasted', 'duplicates', 'late', 'negative_clean', 'skill_tokens_per_turn', 'prompt_tokens_per_turn'];
    L.push(table(['metric', ...names], METRICS.filter(([k]) => keys.includes(k)).map(([k, label, kind]) => [label, ...inputs.map((x) => (x.agg.by_group[g] ? fmtStat(x.agg.by_group[g][k], kind) : '-'))])), '');
  }
  L.push('## Per scenario', '');
  L.push('Each cell: met k/n, recall, precision, wrong, wasted, duplicates, skill tokens; skills used with ✓ must, · may, ✗ wrong, ~ wasted and the number of runs. **Bold** id: the inputs differ in met rate, recall or wrong reads.', '');
  const ids = [...new Set(inputs.flatMap((x) => Object.keys(x.agg.by_scenario)))];
  const rows = ids.map((id) => {
    const cells = inputs.map((x) => x.agg.by_scenario[id]);
    const base = cells[0];
    const differs = cells.some((c) => c && base && (c.passed / c.runs !== base.passed / base.runs || changed(c.recall.mean, base.recall.mean) || changed(c.wrong.mean, base.wrong.mean)));
    return [
      differs ? `**${id}**` : id, cells.find(Boolean)?.group ?? '-',
      ...cells.map((c) => (c ? `${c.passed}/${c.runs} R ${fmtStat(c.recall, 'r')} P ${fmtStat(c.precision, 'r')} ✗${fmtStat(c.wrong, 'n')} ~${fmtStat(c.wasted, 'n')} dup ${fmtStat(c.duplicates, 'n')} tok ${fmtStat(c.skill_tokens, 'n')}<br>${Object.entries(c.reads).sort((a, b) => b[1] - a[1]).map(([m, n]) => `${m[0]}${m.slice(1).replace(/^skill:\/\/[^/]+\//, '')}×${n}`).join(' ') || '-'}` : '-')),
    ];
  });
  L.push(table(['scenario', 'group', ...names], rows), '');
  return L.join('\n');
}

if (import.meta.url === `file://${process.argv[1]}`) {
  const args = parseArgs(process.argv.slice(2));
  if (args._.length < 1) {
    console.error('usage: node eval/suites/skillsel/compare.mjs <run-dir|model-dir> [<run-dir|model-dir> ...] [--out FILE] [--json]');
    process.exit(2);
  }
  const inputs = await loadInputs(args._);
  const text = args.json ? JSON.stringify(inputs.map((x) => ({ label: x.label, ...x.agg })), null, 2) : renderComparison(inputs);
  if (args.out) { await writeText(path.resolve(args.out), text); console.log(`wrote ${args.out}`); } else process.stdout.write(text + '\n');
}
