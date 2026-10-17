#!/usr/bin/env node
// Language-specific suites (LiveCodeBench, BigCodeBench-Hard, RustEvo², Web-Bench, and the Aider
// Polyglot C++/Go/Java exercises): prepared offline, pinned, and graded by the benchmarks' own tests
// whose controls hold. No model is loaded. Checks:
//   1. every suite's task store matches eval/manifest.json (ids, exclusions, content hashes);
//   2. every downloaded artifact still has its pinned SHA-256; every source records a revision;
//   3. the recorded controls: every included task's reference (or LiveCodeBench test oracle) passes
//      and its untouched starting state fails; excluded tasks carry a reason; rates are printed;
//   4. no agent workspace template contains a hidden test or reference file (content hashes);
//   5. fresh controls now, under the network-denying sandbox: a deterministic sample per suite (and
//      per new polyglot language) is graded again; positives must pass and negatives must fail;
//   6. the runner lists exactly the included tasks.
//
//   node eval/verify/langsuites.mjs [--suite livecodebench,bigcodebench,rustevo,webbench,polyglot] [--sample N]
import assert from 'node:assert/strict';
import fs from 'node:fs';
import fsp from 'node:fs/promises';
import os from 'node:os';
import path from 'node:path';
import { HIDDEN, MANIFEST } from '../lib/paths.mjs';
import { SUITES } from '../lib/suites.mjs';
import { checkHiddenAbsent, hiddenIndex, materializeWorkspace } from '../lib/workspace.mjs';
import { parseArgs, readJson, sha256File } from '../lib/util.mjs';
import { taskDirHash } from '../prepare.mjs';
import * as LCBP from '../suites/livecodebench/prepare.mjs';
import * as BCBP from '../suites/bigcodebench/prepare.mjs';
import * as REP from '../suites/rustevo/prepare.mjs';
import * as WBP from '../suites/webbench/prepare.mjs';
import * as PG from '../suites/polyglot/suite.mjs';

const args = parseArgs(process.argv.slice(2));
const which = args.suite ? String(args.suite).split(',') : ['livecodebench', 'bigcodebench', 'rustevo', 'webbench', 'polyglot'];
const SAMPLE = Number(args.sample ?? 2);
const tmp = fs.mkdtempSync(path.join(os.tmpdir(), 'cceval-langsuites-'));

// Positive/negative control keys recorded by each suite's prepare.
const CONTROLS = {
  livecodebench: { pos: 'oracle', neg: ['template', 'wrong'] },
  bigcodebench: { pos: 'canonical', neg: ['template'] },
  rustevo: { pos: 'reference', neg: ['stub'] },
  webbench: { pos: 'reference', neg: ['initial'] },
};

/** Deterministic sample: evenly spaced over the included tasks. */
function sample(list, n) {
  if (list.length <= n) return list;
  return Array.from({ length: n }, (_, i) => list[Math.floor((i * (list.length - 1)) / Math.max(1, n - 1))]);
}

async function checkSources(name, sources) {
  for (const s of sources ?? []) {
    assert.ok(s.url, `${name}: source without URL`);
    assert.ok(s.revision, `${name}: source ${s.name} without a pinned revision`);
    if (s.sha256 && s.path && fs.existsSync(s.path) && fs.statSync(s.path).isFile()) {
      assert.equal(await sha256File(s.path), s.sha256, `${name}: ${s.path} no longer matches its pinned sha256`);
    }
  }
}

async function checkStore(name, m, manifest) {
  const entry = manifest.suites[name];
  assert.ok(entry, `manifest lacks ${name}`);
  const want = new Map(entry.tasks.map((t) => [t.id, t]));
  for (const t of m.tasks) {
    const e = want.get(t.id);
    assert.ok(e, `${name}: ${t.id} not in the manifest`);
    assert.equal(!!e.excluded, !!t.excluded, `${name}/${t.id}: exclusion differs from the manifest`);
    assert.equal(await taskDirHash(path.join(HIDDEN, name, t.id)), e.hash, `${name}/${t.id}: content differs from the manifest`);
  }
  assert.equal(m.tasks.length, entry.tasks.length, `${name}: ${m.tasks.length} tasks vs ${entry.tasks.length} in the manifest`);
}

function checkRecordedControls(name, m) {
  const c = CONTROLS[name];
  const v = new Map(m.validation.map((x) => [x.id, x]));
  let pos = 0;
  let neg = 0;
  for (const t of m.tasks) {
    const x = v.get(t.id);
    assert.ok(x, `${name}/${t.id}: no recorded controls`);
    if (t.excluded) { assert.ok(t.excluded.length > 10, `${name}/${t.id}: excluded without a reason`); continue; }
    assert.equal(x[c.pos], 'pass', `${name}/${t.id}: positive control ${c.pos}=${x[c.pos]}`);
    const repeat = { webbench: 'reference_repeat', rustevo: 'reference_repeat', bigcodebench: 'canonical_repeat' }[name];
    if (repeat) assert.equal(x[repeat], 'pass', `${name}/${t.id}: positive control not deterministic`);
    if (name === 'webbench') assert.ok(x.initial === 'fail' || x.initial_last === 'fail', `${name}/${t.id}: starting state passes`);
    else for (const k of c.neg) assert.ok(x[k] === 'fail' || x[k] === 'not-applicable', `${name}/${t.id}: negative control ${k}=${x[k]}`);
    pos++;
    neg++;
  }
  const all = m.validation.length;
  const posAll = m.validation.filter((x) => x[c.pos] === 'pass').length;
  const negOk = (x) => (name === 'webbench' ? x.initial === 'fail' || x.initial_last === 'fail' : c.neg.every((k) => x[k] === 'fail' || x[k] === 'not-applicable'));
  const negAll = m.validation.filter(negOk).length;
  const excluded = m.tasks.filter((t) => t.excluded);
  const reasons = {};
  for (const t of excluded) { const k = t.excluded.replace(/\(.*?\)/g, '(..)'); reasons[k] = (reasons[k] ?? 0) + 1; }
  console.log(`${name}: ${m.tasks.length - excluded.length} included of ${m.tasks.length}; recorded controls over all: positive ${c.pos} ${posAll}/${all}, negative ${c.neg.join('+')} ${negAll}/${all}; included: ${pos} positive pass, ${neg} negative fail; excluded ${excluded.length} ${JSON.stringify(reasons)}`);
}

async function checkTemplates(name, m) {
  const suite = SUITES[name];
  let n = 0;
  for (const t of m.tasks.filter((x) => !x.excluded)) {
    const task = await suite.loadTask(t.id);
    const hidden = await hiddenIndex(suite.hiddenFiles(task));
    const res = await checkHiddenAbsent(path.join(HIDDEN, name, t.id, 'template'), hidden);
    assert.ok(res.ok, `${name}/${t.id}: template contains hidden files ${JSON.stringify(res.leaks)}`);
    n++;
  }
  console.log(`${name}: ${n} templates free of hidden tests and reference files`);
}

async function freshControls(name, m) {
  const included = m.tasks.filter((t) => !t.excluded);
  const picks = name === 'bigcodebench'
    ? [...sample(included.filter((t) => t.group === 'complete'), SAMPLE), ...sample(included.filter((t) => t.group === 'instruct'), SAMPLE)]
    : sample(included, SAMPLE);
  for (const p of picks) {
    const task = await SUITES[name].loadTask(p.id);
    let v;
    if (name === 'livecodebench') v = await LCBP.validate(task, { sample: JSON.parse(await fsp.readFile(path.join(HIDDEN, name, p.id, 'tests', 'tests.json'), 'utf8')) });
    else if (name === 'bigcodebench') v = await BCBP.validate(task, JSON.parse(await fsp.readFile(path.join(HIDDEN, name, p.id, 'reference', 'canonical.json'), 'utf8')));
    else if (name === 'rustevo') v = await REP.validate(task, await fsp.readFile(path.join(HIDDEN, name, p.id, 'reference', 'lib.rs'), 'utf8'));
    else v = await WBP.validate(task);
    const c = CONTROLS[name];
    assert.equal(v[c.pos], 'pass', `${name}/${p.id}: fresh positive control failed`);
    if (name === 'webbench') assert.ok(v.initial === 'fail' || v.initial_last === 'fail', `${name}/${p.id}: fresh negative control passed`);
    else for (const k of c.neg) assert.ok(v[k] === 'fail' || v[k] === 'not-applicable', `${name}/${p.id}: fresh negative control ${k}=${v[k]}`);
    console.log(`${name}: fresh controls on ${p.id}: ${c.pos} pass (${v[`${c.pos}_ms`]} ms), ${c.neg.map((k) => `${k} ${v[k]}`).join(', ')}`);
  }
}

async function polyglotNewLanguages(manifest) {
  const m = await readJson(path.join(HIDDEN, 'polyglot', 'tasks.json'));
  await checkSources('polyglot', m.sources);
  const v = new Map(m.validation.map((x) => [x.id, x]));
  for (const lang of ['cpp', 'go', 'java']) {
    const tasks = m.tasks.filter((t) => t.lang === lang);
    assert.ok(tasks.length > 0, `polyglot: no ${lang} exercises`);
    const ex = tasks.filter((t) => v.get(t.id)?.example === 'pass').length;
    const stubFail = tasks.filter((t) => v.get(t.id)?.stub === 'fail').length;
    for (const t of tasks) {
      const x = v.get(t.id);
      assert.ok((x.example === 'pass' && x.stub === 'fail') || x.note, `${t.id}: example=${x.example} stub=${x.stub} without a recorded upstream reason`);
    }
    console.log(`polyglot ${lang}: ${tasks.length} exercises; upstream example passes ${ex}/${tasks.length}; stub fails ${stubFail}/${tasks.length} (stub passes only where upstream ships working code: ${tasks.filter((t) => v.get(t.id)?.stub === 'pass').map((t) => t.slug).join(', ') || 'none'})`);
    for (const t of sample(tasks.filter((x) => !v.get(x.id)?.note), SAMPLE)) {
      const task = await PG.loadPolyglotTask(t.id);
      for (const mode of ['stub', 'example']) {
        const ws = path.join(tmp, `${t.id}-${mode}`);
        await materializeWorkspace(path.join(PG.taskDir(t.id), 'template'), ws, { links: PG.workspaceLinks(task) });
        if (mode === 'example') {
          for (const x of PG.exampleTargets(task)) {
            await fsp.mkdir(path.dirname(path.join(ws, x.to)), { recursive: true });
            await fsp.copyFile(path.join(PG.taskDir(t.id), 'example', x.from), path.join(ws, x.to));
          }
        }
        const res = await PG.polyglotScore(task, ws, path.join(tmp, 'score', `${t.id}-${mode}`), path.join(tmp, 'log', `${t.id}-${mode}`), 'langsuites');
        assert.equal(res.passed, mode === 'example', `${t.id}: fresh ${mode} control ${res.passed ? 'passed' : 'failed'}\n${res.output.slice(-1500)}`);
      }
      console.log(`polyglot ${lang}: fresh controls on ${t.id}: example passes, stub fails (sandboxed, offline)`);
    }
  }
  const entry = manifest.suites.polyglot;
  for (const lang of ['cpp', 'go', 'java']) assert.equal(entry.by_lang[lang], m.tasks.filter((t) => t.lang === lang).length, `manifest polyglot ${lang} count`);
  const listed = await SUITES.polyglot.listTasks({ lang: 'cpp,go,java' });
  assert.equal(listed.length, m.tasks.filter((t) => ['cpp', 'go', 'java'].includes(t.lang)).length, 'runner lists every C++/Go/Java exercise');
}

try {
  const manifest = await readJson(MANIFEST);
  for (const name of which.filter((n) => n !== 'polyglot')) {
    const m = await readJson(path.join(HIDDEN, name, 'tasks.json'));
    await checkSources(name, m.sources);
    await checkStore(name, m, manifest);
    checkRecordedControls(name, m);
    await checkTemplates(name, m);
    const listed = await SUITES[name].listTasks({});
    assert.equal(listed.length, m.tasks.filter((t) => !t.excluded).length, `${name}: runner lists ${listed.length} tasks`);
    await freshControls(name, m);
  }
  if (which.includes('polyglot')) await polyglotNewLanguages(manifest);
  console.log('language suites verification passed');
} catch (error) {
  console.error(error);
  process.exitCode = 1;
} finally {
  fs.rmSync(tmp, { recursive: true, force: true });
}
