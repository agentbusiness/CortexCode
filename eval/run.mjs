#!/usr/bin/env node
// Entry point for benchmark runs.
//
//   node eval/run.mjs --suite <editbench|polyglot|realistic|livecodebench|bigcodebench|rustevo|webbench|swebench|terminalbench|skillsel|all>
//        --model <variant> [--limit N]
//        [--system cortex|aider|mini|terminus|all] [--edit-format hashline|replace|whole|all] [--runs N]
//        [--lang L[,L]] [--group G[,G]] [--since YYYY-MM-DD] [--until YYYY-MM-DD] [--tasks id,id]
//        [--timeout-min M] [--parallel N] [--results-dir DIR] [--aider-edit-format diff] [--keep-workspaces]
//        [--suite-opt key=value[,key=value]]
//
// --lang selects languages (polyglot: python,javascript,rust,cpp,go,java); --group selects a suite's
// task groups (bigcodebench: complete,instruct; livecodebench: platform; rustevo: change type;
// webbench: project; swebench: repository, e.g. django/django; terminalbench: category);
// --since/--until select LiveCodeBench problems by contest date.
//
// Container suites (Docker; CortexCode reaches the task container through its container plugin):
//   swebench       SWE-bench Verified, graded by the official harness. Systems: cortex, mini
//                  (mini-swe-agent's SWE-bench runner); aider is not an agent harness for it.
//                  --suite-opt images=auto|arm64|x86_64 (default auto: a native arm64 image where
//                  prepare validated one, else the published x86_64 image).
//   terminalbench  Terminal-Bench 2.0, run by Harbor. Systems: cortex, terminus (Harbor's Terminus-2).
//                  The task's own agent timeout applies (--timeout-min overrides it, recorded);
//                  --suite-opt build=prebuilt|native (default prebuilt: the task's published image).
//
// Skill selection (docs/SKILL-SELECTION-TESTS.md):
//   skillsel       scenarios from suites/skillsel/bank.json on tiny fixture projects; scores which
//                  skills each turn read (or a decision chose and the host injected), when, and at what
//                  cost. CortexCode only, 3 runs by default; --group positive|multi|negative|tempting|
//                  conversation|catalog, --tasks P1,N3. Per-run summary.md and results.jsonl; compare
//                  runs with node eval/suites/skillsel/compare.mjs <run-dir|model-dir> ...
//
// Project knowledge (docs/PROJECT-KNOWLEDGE-PLAN.md P7):
//   knowledge      7 scenarios from suites/knowledge/bank.json on the bowling fixture: a fact taught in
//                  one session recalled in the next, no card on an ordinary request, how often a general
//                  statement makes a card (measured), a refuted theory avoided, a README-only answer, a
//                  decision recalled with its reason, and the cost of the brief and the pre-read note.
//                  CortexCode only, 3 runs by default; --tasks K1,K4. Per-run summary.md and results.jsonl.
//
// Defaults follow the benchmark design: editbench runs CortexCode in every edit format, 3 runs;
// polyglot and realistic run CortexCode (product default edit format) plus both baselines, 1 run;
// the container suites run CortexCode plus their own baseline.
// Runs resume: finished tasks (result.json) are skipped, harness errors are retried.
// The per-task timeout is a wall-clock budget (not a cap on output or reasoning); exceeding it is
// recorded as the failure class `timeout`.
import { EDIT_FORMATS } from './lib/driver.mjs';
import { runSuite, suiteSystems } from './lib/runner.mjs';
import { ALL_SUITES, SUITES } from './lib/suites.mjs';
import { log, parseArgs } from './lib/util.mjs';

const args = parseArgs(process.argv.slice(2));
if (!args.suite || !args.model) {
  console.error(`usage: node eval/run.mjs --suite <${ALL_SUITES.join('|')}|all> --model <variant> [--limit N] [--system cortex|aider|mini|terminus|all] [--edit-format F|all] [--runs N] [--lang L] [--group G] [--since D] [--until D] [--suite-opt k=v,k=v]`);
  process.exit(2);
}

const suites = args.suite === 'all' ? ALL_SUITES : String(args.suite).split(',');
for (const s of suites) {
  if (!ALL_SUITES.includes(s)) { console.error(`unknown suite ${s} (known: ${ALL_SUITES.join(', ')})`); process.exit(2); }
}
// Suite options (--suite-opt images=arm64,build=native): passed to every suite's task listing,
// environment and baseline hooks, and recorded in run.json.
const suiteOpts = {};
for (const kv of String(args['suite-opt'] ?? '').split(',').filter(Boolean)) {
  const i = kv.indexOf('=');
  if (i <= 0) { console.error(`--suite-opt expects key=value, got ${kv}`); process.exit(2); }
  suiteOpts[kv.slice(0, i)] = kv.slice(i + 1);
}

const plan = [];
for (const suite of suites) {
  const own = suiteSystems(SUITES[suite]);
  const systems = args.system ? (args.system === 'all' ? own : String(args.system).split(',')) : (suite === 'editbench' ? ['cortex'] : own);
  for (const system of systems) {
    if (suite === 'editbench' && system !== 'cortex') {
      console.error('editbench measures CortexCode edit formats; baselines run polyglot and realistic');
      process.exit(2);
    }
    if (!own.includes(system)) {
      console.error(SUITES[suite].unsupportedSystems?.[system] ?? `${suite} runs the systems ${own.join(', ')}, not ${system}`);
      process.exit(2);
    }
    const formats = system !== 'cortex' ? [null]
      : args['edit-format'] ? (args['edit-format'] === 'all' ? EDIT_FORMATS : String(args['edit-format']).split(','))
        : (suite === 'editbench' ? EDIT_FORMATS : [null]);
    for (const editFormat of formats) {
      plan.push({
        suite, system, editFormat, model: String(args.model),
        runs: Number(args.runs ?? SUITES[suite].defaultRuns ?? (suite === 'editbench' ? 3 : 1)),
        limit: args.limit ? Number(args.limit) : undefined,
        lang: args.lang, group: args.group, since: args.since, until: args.until,
        ids: args.tasks ? String(args.tasks).split(',') : undefined,
        timeoutMs: args['timeout-min'] ? Number(args['timeout-min']) * 60000 : undefined,
        parallel: args.parallel ? Number(args.parallel) : undefined,
        resultsDir: args['results-dir'], aiderEditFormat: args['aider-edit-format'] ?? 'diff',
        keepWorkspaces: !!args['keep-workspaces'], suiteOpts,
      });
    }
  }
}

let failed = false;
for (const p of plan) {
  log(`== ${p.suite} / ${p.system}${p.editFormat ? ` (${p.editFormat})` : ''} / ${p.model}`);
  try {
    const out = await runSuite(p);
    for (const s of out) log(`${s.runDir}: ${s.passed}/${s.total} passed${s.complete ? '' : ' (incomplete)'}`);
  } catch (error) {
    failed = true;
    log(`run failed: ${error?.stack ?? error}`);
  }
}
process.exit(failed ? 1 : 0);
