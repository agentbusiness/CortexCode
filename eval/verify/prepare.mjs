#!/usr/bin/env node
// G1: harness unit tests pass and every suite's task set is materialized offline.
//   - unit tests (node --test);
//   - per-suite task counts printed and checked against eval/manifest.json (ids and content hashes);
//   - independent recomputation of each suite's construction from the cached upstream sources:
//     editbench mutations re-applied to the pinned originals, polyglot exercise counts from the
//     pinned commit, realistic hidden tests and reverted sources re-read from the cached git history
//     plus each task's recorded verification (original passes, reverted fails);
//   - offline proof: scoring for one task per language runs under the network-denying sandbox with
//     dependencies served only from the caches.
import assert from 'node:assert/strict';
import { execFileSync } from 'node:child_process';
import fs from 'node:fs';
import fsp from 'node:fs/promises';
import os from 'node:os';
import path from 'node:path';
import { EVAL_ROOT, HIDDEN, MANIFEST, SRC, TOOLCHAINS } from '../lib/paths.mjs';
import { py, T } from '../lib/toolchains.mjs';
import { materializeWorkspace } from '../lib/workspace.mjs';
import { readJson, run, sha256 } from '../lib/util.mjs';
import { taskDirHash } from '../prepare.mjs';
import { EDITBENCH_REPOS } from '../suites/editbench/repos.mjs';
import * as PG from '../suites/polyglot/suite.mjs';
import * as RL from '../suites/realistic/suite.mjs';

const tmp = fs.mkdtempSync(path.join(os.tmpdir(), 'cceval-prepare-verify-'));
const gitShow = (dir, rev, file) => execFileSync('git', ['cat-file', 'blob', `${rev}:${file}`], { cwd: dir, maxBuffer: 1 << 30 });

try {
  // ---- unit tests ----
  const unit = await run(process.execPath, ['--test', 'test/*.test.mjs'], { cwd: EVAL_ROOT, timeoutMs: 600000 });
  assert.equal(unit.code, 0, `unit tests failed:\n${unit.output.slice(-3000)}`);
  console.log(`unit tests: ${unit.output.match(/# pass (\d+)/)?.[1]} passed, ${unit.output.match(/# fail (\d+)/)?.[1]} failed`);

  // ---- manifest ----
  const manifest = await readJson(MANIFEST);
  for (const name of ['editbench', 'polyglot', 'realistic']) {
    const m = manifest.suites[name];
    assert.ok(m, `manifest lacks ${name}`);
    const tasks = (await readJson(path.join(HIDDEN, name, 'tasks.json'))).tasks;
    assert.equal(tasks.length, m.count, `${name}: ${tasks.length} materialized vs ${m.count} in manifest`);
    const want = new Map(m.tasks.map((t) => [t.id, t.hash]));
    for (const t of tasks) {
      assert.ok(want.has(t.id), `${name}: ${t.id} not in manifest`);
      assert.equal(await taskDirHash(path.join(HIDDEN, name, t.id)), want.get(t.id), `${name}/${t.id}: content differs from manifest`);
    }
    console.log(`${name}: ${tasks.length} tasks ${JSON.stringify(m.by_lang)} match the manifest`);
  }

  // ---- editbench: recompute every mutation from the pinned upstream file ----
  const eb = (await readJson(path.join(HIDDEN, 'editbench', 'tasks.json'))).tasks;
  const typesPerLang = { ts: 6, python: 5, rust: 5 };
  const ebCounts = {};
  for (const t of eb) {
    const task = await readJson(path.join(HIDDEN, 'editbench', t.id, 'task.json'));
    const repo = EDITBENCH_REPOS.find((r) => r.id === task.repo);
    const upstream = gitShow(path.join(SRC, repo.dir), repo.commit, task.path);
    const original = await fsp.readFile(path.join(HIDDEN, 'editbench', t.id, 'original', task.path));
    assert.ok(upstream.equals(original), `${t.id}: original differs from ${repo.id}@${repo.commit}:${task.path}`);
    const m = task.mutation;
    assert.equal(upstream.subarray(m.start, m.end).toString('utf8'), m.original, `${t.id}: recorded span does not hold the original text`);
    const expected = Buffer.concat([upstream.subarray(0, m.start), Buffer.from(m.replacement), upstream.subarray(m.end)]);
    const template = await fsp.readFile(path.join(HIDDEN, 'editbench', t.id, 'template', task.path));
    assert.ok(expected.equals(template), `${t.id}: template is not the original with the recorded mutation`);
    assert.ok(!template.equals(original), `${t.id}: mutation is a no-op`);
    assert.ok(task.prompt.includes(`\`${task.path}\``) && task.prompt.includes(`\`${m.function}\``), `${t.id}: prompt lacks file or function`);
    const key = `${task.lang}:${m.type}`;
    ebCounts[key] = (ebCounts[key] ?? 0) + 1;
  }
  for (const [lang, n] of Object.entries(typesPerLang)) {
    const counts = Object.entries(ebCounts).filter(([k]) => k.startsWith(`${lang}:`)).map(([, v]) => v);
    assert.equal(counts.length, n, `${lang}: ${counts.length} mutation types, expected ${n}`);
    assert.equal(counts.reduce((a, b) => a + b, 0), 60, `${lang}: 60 tasks`);
    assert.ok(Math.max(...counts) - Math.min(...counts) <= 1, `${lang}: unbalanced types ${counts}`);
  }
  console.log(`editbench: all ${eb.length} mutations re-derived from pinned upstream files; types balanced ${JSON.stringify(ebCounts)}`);

  // ---- polyglot: counts from the pinned commit, hidden files, validation ----
  const pgManifest = await readJson(path.join(HIDDEN, 'polyglot', 'tasks.json'));
  const pgSrc = path.join(SRC, 'polyglot-benchmark');
  for (const lang of PG.POLYGLOT.languages) {
    const listing = execFileSync('git', ['ls-tree', '--name-only', `${PG.POLYGLOT.commit}:${lang}/exercises/practice`], { cwd: pgSrc, encoding: 'utf8' }).split('\n').filter(Boolean);
    const ours = pgManifest.tasks.filter((t) => t.lang === lang).map((t) => t.slug).sort();
    assert.deepEqual(ours, listing.sort(), `${lang}: exercise set differs from upstream ${PG.POLYGLOT.commit}`);
  }
  for (const t of pgManifest.tasks) {
    const task = await PG.loadPolyglotTask(t.id);
    for (const f of task.test_files) {
      const up = gitShow(pgSrc, PG.POLYGLOT.commit, `${task.lang}/exercises/practice/${task.slug}/${f}`);
      assert.ok(up.equals(await fsp.readFile(path.join(PG.taskDir(t.id), 'tests', f))), `${t.id}: hidden test differs from upstream`);
      assert.ok(!fs.existsSync(path.join(PG.taskDir(t.id), 'template', f)), `${t.id}: test visible in template`);
    }
    const v = pgManifest.validation.find((x) => x.id === t.id);
    assert.ok(v, `${t.id}: not validated`);
    const ok = v.stub === 'fail' && v.example === 'pass';
    assert.ok(ok || v.note, `${t.id}: validation stub=${v.stub} example=${v.example} without a recorded upstream reason`);
  }
  const exceptions = pgManifest.validation.filter((v) => v.note);
  console.log(`polyglot: ${pgManifest.tasks.length} exercises equal upstream at ${PG.POLYGLOT.commit.slice(0, 10)}; ${pgManifest.tasks.length - exceptions.length} validated (stub fails, example passes); upstream exceptions: ${exceptions.map((e) => `${e.id} (${e.note})`).join('; ') || 'none'}`);

  // ---- realistic: re-read from cached history, verification evidence, balance ----
  const rlManifest = await readJson(path.join(HIDDEN, 'realistic', 'tasks.json'));
  const rl = rlManifest.tasks;
  const byLang = {};
  for (const t of rl) {
    const task = await RL.loadRealisticTask(t.id);
    const repo = RL.repoById(task.repo);
    const src = path.join(SRC, repo.dir);
    for (const f of RL.hiddenFiles(task)) {
      assert.ok(gitShow(src, task.commit, f.rel).equals(await fsp.readFile(f.abs)), `${t.id}: hidden ${f.rel} differs from commit`);
    }
    for (const f of task.source_files) {
      const at = path.join(RL.templateDir(task), f.path);
      if (f.status === 'A') assert.ok(!fs.existsSync(at), `${t.id}: added source ${f.path} present in template`);
      else assert.ok(gitShow(src, task.parent, f.path).equals(await fsp.readFile(at)), `${t.id}: ${f.path} is not the parent version`);
    }
    const dir = path.join(HIDDEN, 'realistic', t.id, 'logs');
    const r1 = await readJson(path.join(dir, 'reference-1', 'result.json'));
    const r2 = await readJson(path.join(dir, 'reference-2', 'result.json'));
    const rv = await readJson(path.join(dir, 'reverted', 'result.json'));
    assert.ok(r1.passed && r2.passed && r1.counts.passed > 0, `${t.id}: original commit not verified passing`);
    assert.ok(!rv.passed, `${t.id}: reverted state not verified failing`);
    assert.ok(task.subject.trim().length > 0 && task.prompt.includes(task.subject), `${t.id}: task statement missing`);
    byLang[task.lang] = (byLang[task.lang] ?? 0) + 1;
  }
  assert.ok(rl.length >= 40, `realistic: ${rl.length} tasks < 40`);
  for (const lang of ['rust', 'ts', 'python', 'bash']) assert.ok((byLang[lang] ?? 0) >= 8, `realistic: ${lang} has ${byLang[lang] ?? 0} tasks (< 8)`);
  const owner = rl.filter((t) => t.origin === 'owner').length;
  assert.ok(owner >= 1, 'realistic: no tasks from the local reference repository');
  const reasons = {};
  for (const d of rlManifest.discarded) reasons[d.reason.replace(/\d+/g, 'N')] = (reasons[d.reason.replace(/\d+/g, 'N')] ?? 0) + 1;
  console.log(`realistic: ${rl.length} tasks ${JSON.stringify(byLang)}, ${owner} from the local reference repository; each verified (original passes twice, reverted fails); ${rlManifest.discarded.length} candidates discarded: ${JSON.stringify(reasons)}`);

  // ---- offline proof: scoring under the network-denying sandbox from caches only ----
  for (const lang of PG.POLYGLOT.languages) {
    const t = pgManifest.tasks.find((x) => x.lang === lang && pgManifest.validation.find((v) => v.id === x.id && !v.note));
    const task = await PG.loadPolyglotTask(t.id);
    const ws = path.join(tmp, t.id);
    await materializeWorkspace(path.join(PG.taskDir(t.id), 'template'), ws, { links: PG.workspaceLinks(task) });
    for (const x of PG.exampleTargets(task)) {
      await fsp.mkdir(path.dirname(path.join(ws, x.to)), { recursive: true });
      await fsp.copyFile(path.join(PG.taskDir(t.id), 'example', x.from), path.join(ws, x.to));
    }
    const res = await PG.polyglotScore(task, ws, path.join(tmp, 'score', t.id), path.join(tmp, 'log', t.id), 'prepare-offline');
    assert.ok(res.passed, `${t.id}: offline scoring of the example failed\n${res.output.slice(-1500)}`);
    console.log(`offline: ${t.id} example passes under the sandbox (${res.ms} ms)`);
  }
  for (const lang of ['rust', 'ts', 'python', 'bash']) {
    const t = rl.find((x) => x.lang === lang);
    if (!t) continue;
    const task = await RL.loadRealisticTask(t.id);
    const ws = path.join(tmp, t.id);
    await materializeWorkspace(RL.templateDir(task), ws, { exclude: RL.workspaceExcludes(task) });
    for (const f of RL.solutionFiles(task)) {
      await fsp.mkdir(path.dirname(path.join(ws, f.rel)), { recursive: true });
      await fsp.copyFile(f.abs, path.join(ws, f.rel));
    }
    for (const f of task.source_files.filter((x) => x.status === 'D')) await fsp.rm(path.join(ws, f.path), { force: true });
    const res = await RL.realisticScore(task, ws, path.join(tmp, 'score', t.id), path.join(tmp, 'log', t.id), 'prepare-offline');
    assert.ok(res.passed, `${t.id}: offline scoring of the upstream change failed\n${res.output.slice(-1500)}`);
    console.log(`offline: ${t.id} upstream change passes under the sandbox with cached deps (${res.counts.passed} tests, ${res.ms} ms)`);
  }
  // Baseline toolchains import without network.
  for (const [name, venv, mod] of [['aider', T.aider, 'aider.coders'], ['mini-swe-agent', T.mini, 'minisweagent.agents.default']]) {
    const res = await run(py(venv), ['-c', `import ${mod}`], { timeoutMs: 120000, env: { ...process.env, HF_HUB_OFFLINE: '1' } });
    assert.equal(res.code, 0, `${name} import failed: ${res.output.slice(-800)}`);
  }
  assert.ok(fs.existsSync(path.join(T.bats, 'bin', 'bats')) && fs.existsSync(path.join(TOOLCHAINS, 'ts-grammars')));
  console.log('toolchains: aider and mini-swe-agent import offline; bats, grammars, node and cargo caches present');
  console.log(`manifest sha256 ${sha256(fs.readFileSync(MANIFEST))}`);
  console.log('prepare verification passed');
} catch (error) {
  console.error(error);
  process.exitCode = 1;
} finally {
  fs.rmSync(tmp, { recursive: true, force: true });
}
