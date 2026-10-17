#!/usr/bin/env node
// G2: no-cheating guarantees, each with a negative control.
//   1. The run sandbox blocks outbound network except localhost (a server on this machine's LAN
//      address is reachable unsandboxed but not sandboxed; children inherit the block), and the
//      kernel's denial reports come back tagged.
//   2. Hidden paths (hidden tests, upstream clones, results) are unreadable inside the sandbox and
//      the attempt is reported.
//   3. The after-attempt check finds a hidden test file copied anywhere in a workspace (exact or
//      CRLF), and agent workspaces never contain the hidden tests or example solutions.
//   4. Scoring runs in a fresh copy with the real tests restored: an agent-written fake test at the
//      hidden path cannot make a stub pass; the agent workspace is untouched by scoring.
//   5. Realistic scoring cannot be passed by making the runner skip tests.
import assert from 'node:assert/strict';
import fs from 'node:fs';
import fsp from 'node:fs/promises';
import http from 'node:http';
import os from 'node:os';
import path from 'node:path';
import { hiddenPaths, readOnlyCaches } from '../lib/runner.mjs';
import { T } from '../lib/toolchains.mjs';
import { violations, writeProfile } from '../lib/sandbox.mjs';
import { SUITES } from '../lib/suites.mjs';
import { checkHiddenAbsent, hiddenIndex, materializeWorkspace } from '../lib/workspace.mjs';
import { run, sha256, sleep, walk } from '../lib/util.mjs';
import * as PG from '../suites/polyglot/suite.mjs';
import * as RL from '../suites/realistic/suite.mjs';

const root = fs.mkdtempSync(path.join(os.tmpdir(), 'cceval-integrity-'));
const tag = `integrity-${Date.now().toString(36)}`;
let server;

async function treeHash(dir) {
  const parts = [];
  for (const f of await walk(dir, { skipDirs: ['.git'] })) {
    const abs = path.join(dir, f.rel);
    parts.push(`${f.rel}:${f.symlink ? await fsp.readlink(abs) : sha256(await fsp.readFile(abs))}`);
  }
  return sha256(parts.join('\n'));
}

function lanAddress() {
  for (const list of Object.values(os.networkInterfaces())) {
    for (const a of list ?? []) if (a.family === 'IPv4' && !a.internal) return a.address;
  }
  return null;
}

const curl = (url) => ['/usr/bin/curl', '-sS', '-m', '5', '-o', '/dev/null', '-w', '%{http_code}', url];

try {
  const profile = await writeProfile(path.join(root, 'run.sb'), { tag, denyRead: hiddenPaths([path.join(root, 'score')]), denyWrite: readOnlyCaches() });
  const sb = (argv) => run('/usr/bin/sandbox-exec', ['-f', profile, ...argv], { timeoutMs: 30000 });

  // ---- 1. network ----
  server = http.createServer((req, res) => { res.writeHead(200); res.end('ok'); });
  await new Promise((r) => server.listen(0, '0.0.0.0', r));
  const port = server.address().port;
  const lan = lanAddress();
  const started = Date.now();
  const local = await sb(curl(`http://127.0.0.1:${port}/`));
  assert.equal(local.stdout, '200', `localhost must stay reachable inside the sandbox: ${local.output}`);
  // Off-machine endpoints: reachable without the sandbox (positive control), blocked inside it.
  // (The machine's own LAN address is on-machine traffic routed over loopback; macOS's sandbox
  // treats it as localhost, which is recorded below rather than counted as a block.)
  let proved = 0;
  for (const url of ['https://example.com/', 'https://1.1.1.1/']) {
    const outside = await run('/usr/bin/curl', curl(url).slice(1), { timeoutMs: 30000 });
    const inside = await sb(curl(url));
    const child = await sb(['/bin/sh', '-c', `${curl(url).join(' ')}; exit $?`]);
    assert.notEqual(inside.code, 0, `${url} must be blocked inside the sandbox: ${inside.output}`);
    assert.notEqual(child.code, 0, `${url}: a child process inherits the block`);
    if (outside.code === 0 && /^[23]\d\d$/.test(outside.stdout)) proved++;
    console.log(`network: ${url} outside=${outside.stdout || `exit ${outside.code}`}, inside exit ${inside.code}, child exit ${child.code}`);
  }
  assert.ok(proved > 0, 'no off-machine endpoint reachable without the sandbox, so the block cannot be proven (connect the machine and rerun)');
  if (lan) {
    const own = await sb(curl(`http://${lan}:${port}/`));
    console.log(`network: own LAN address ${lan} inside sandbox -> ${own.stdout || own.code} (on-machine traffic, treated as localhost by the kernel)`);
  }

  // ---- 2. hidden paths ----
  const pgTask = await PG.loadPolyglotTask('pg-python-bowling');
  const hiddenTest = PG.hiddenFiles(pgTask)[0].abs;
  const plain = await run('/bin/cat', [hiddenTest]);
  assert.equal(plain.code, 0, 'positive control: the harness itself can read the hidden test');
  const denied = await sb(['/bin/cat', hiddenTest]);
  assert.notEqual(denied.code, 0, 'hidden test must be unreadable in the sandbox');
  assert.match(denied.output, /Operation not permitted/);
  const listing = await sb(['/bin/ls', path.dirname(path.dirname(hiddenTest))]);
  assert.notEqual(listing.code, 0, 'hidden task directory must not be listable');

  // Shared caches are read-only inside the sandbox (one task cannot alter another's tools).
  const cacheFile = path.join(T.polyglotJs, 'node_modules', 'jest', 'package.json');
  const readCache = await sb(['/bin/cat', cacheFile]);
  assert.equal(readCache.code, 0, 'caches stay readable');
  const writeCache = await sb(['/bin/sh', '-c', `echo x >> "${cacheFile}"`]);
  assert.notEqual(writeCache.code, 0, 'cache must not be writable inside the sandbox');
  const plainWrite = await run('/usr/bin/touch', ['-c', cacheFile]);
  assert.equal(plainWrite.code, 0, 'positive control: the harness itself can write the cache');
  console.log('caches: readable, not writable inside the sandbox');

  // Denials come back from the kernel log with our tag.
  let events = [];
  for (let i = 0; i < 10; i++) {
    await sleep(2000);
    events = await violations(tag, started, Date.now());
    if (events.some((e) => e.kind === 'net') && events.some((e) => e.kind === 'hidden')) break;
  }
  assert.ok(events.some((e) => e.kind === 'net'), 'network denial was logged');
  assert.ok(events.some((e) => e.kind === 'hidden' && (e.target ?? '').includes('pg-python-bowling')), 'hidden read denial was logged with its path');
  console.log(`sandbox log: ${events.filter((e) => e.kind === 'net').length} network and ${events.filter((e) => e.kind === 'hidden').length} hidden-read denials reported for tag ${tag}`);

  // ---- 3. hidden-absent check on real workspaces ----
  for (const suiteName of ['polyglot', 'realistic']) {
    const suite = SUITES[suiteName];
    const tasks = await suite.listTasks({});
    let checked = 0;
    for (const task of tasks) {
      // Templates must not contain hidden tests or upstream solutions (all tasks).
      const hidden = await hiddenIndex(suite.hiddenFiles(task));
      const template = suiteName === 'polyglot' ? path.join(PG.taskDir(task.id), 'template') : RL.templateDir(task);
      const r = await checkHiddenAbsent(template, hidden);
      assert.ok(r.ok, `${task.id}: template contains hidden content ${JSON.stringify(r.leaks)}`);
      if (suiteName === 'polyglot') {
        assert.ok(!fs.existsSync(path.join(template, '.meta')) && !fs.existsSync(path.join(template, '.docs')), `${task.id}: .meta/.docs present`);
        for (const t of task.test_files) assert.ok(!fs.existsSync(path.join(template, t)), `${task.id}: test file ${t} present`);
      }
      checked++;
    }
    console.log(`${suiteName}: ${checked} task templates free of hidden tests/solutions`);
  }
  const ws = path.join(root, 'ws');
  await materializeWorkspace(path.join(PG.taskDir(pgTask.id), 'template'), ws);
  const idx = await hiddenIndex([...PG.hiddenFiles(pgTask), ...PG.secretFiles(pgTask)]);
  assert.ok((await checkHiddenAbsent(ws, idx)).ok, 'fresh workspace passes the check');
  await fsp.mkdir(path.join(ws, 'notes'), { recursive: true });
  await fsp.writeFile(path.join(ws, 'notes', 'copied.txt'), (await fsp.readFile(hiddenTest, 'utf8')).replace(/\n/g, '\r\n'));
  const leak = await checkHiddenAbsent(ws, idx);
  assert.equal(leak.ok, false, 'negative control: a CRLF copy of the hidden test is detected');
  assert.equal(leak.leaks[0].rel, 'notes/copied.txt');
  await fsp.rm(path.join(ws, 'notes'), { recursive: true });
  await fsp.copyFile(path.join(PG.taskDir(pgTask.id), 'example', pgTask.example_files[0]), path.join(ws, 'solution_copy.py'));
  assert.equal((await checkHiddenAbsent(ws, idx)).ok, false, 'negative control: a copy of the upstream example solution is detected');
  await fsp.rm(path.join(ws, 'solution_copy.py'));
  console.log('hidden check: clean workspace ok; copied test (CRLF) and copied example solution detected');

  // ---- 4. fresh-copy scoring ----
  const fakeTest = 'def test_nothing():\n    assert True\n';
  await fsp.writeFile(path.join(ws, pgTask.test_files[0]), fakeTest);
  const before = await treeHash(ws);
  const stubScore = await PG.polyglotScore(pgTask, ws, path.join(root, 'score', 'a'), path.join(root, 'logs', 'a'), `${tag}-score`);
  assert.equal(stubScore.passed, false, 'a fake test written by the agent must not let the stub pass');
  assert.match(stubScore.output, /bowling_test\.py/);
  assert.equal(await treeHash(ws), before, 'scoring must not modify the agent workspace');
  assert.equal(await fsp.readFile(path.join(ws, pgTask.test_files[0]), 'utf8'), fakeTest);
  await fsp.copyFile(path.join(PG.taskDir(pgTask.id), 'example', pgTask.example_files[0]), path.join(ws, pgTask.solution_files[0]));
  const goodScore = await PG.polyglotScore(pgTask, ws, path.join(root, 'score', 'b'), path.join(root, 'logs', 'b'), `${tag}-score`);
  assert.equal(goodScore.passed, true, `positive control: the example solution passes with the restored tests\n${goodScore.output.slice(-800)}`);
  assert.ok(!fs.existsSync(path.join(root, 'score', 'a')), 'scoring copy is removed after scoring');
  console.log('scoring: fake test overridden (stub fails), example passes, workspace unchanged, copy removed');

  // ---- 5. realistic: skipping tests does not pass ----
  const rlTasks = await SUITES.realistic.listTasks({});
  const rlTask = rlTasks.find((t) => t.repo === 'click') ?? rlTasks.find((t) => RL.repoById(t.repo).counter === 'pytest');
  const ref = path.join(root, 'rl-ref');
  await materializeWorkspace(RL.templateDir(rlTask), ref);
  for (const f of RL.solutionFiles(rlTask)) {
    await fsp.mkdir(path.dirname(path.join(ref, f.rel)), { recursive: true });
    await fsp.copyFile(f.abs, path.join(ref, f.rel));
  }
  for (const f of rlTask.source_files.filter((x) => x.status === 'D')) await fsp.rm(path.join(ref, f.path), { force: true });
  const okRef = await RL.realisticScore(rlTask, ref, path.join(root, 'score', 'c'), path.join(root, 'logs', 'c'), `${tag}-score`);
  assert.equal(okRef.passed, true, `positive control: the upstream change passes (${JSON.stringify(okRef.counts)})`);
  await fsp.writeFile(path.join(ref, 'conftest.py'), 'import pytest\n\ndef pytest_collection_modifyitems(items):\n    for item in items:\n        item.add_marker(pytest.mark.skip(reason="skip"))\n');
  const skipped = await RL.realisticScore(rlTask, ref, path.join(root, 'score', 'd'), path.join(root, 'logs', 'd'), `${tag}-score`);
  assert.equal(skipped.exit_code, 0, 'the skip trick makes the runner exit 0');
  assert.equal(skipped.passed, false, 'but it does not pass: fewer tests passed than the reference');
  const rev = await RL.realisticScore(rlTask, RL.templateDir(rlTask), path.join(root, 'score', 'e'), path.join(root, 'logs', 'e'), `${tag}-score`);
  assert.equal(rev.passed, false, 'the reverted state (no change) fails');
  console.log(`realistic ${rlTask.id}: reference passes (${okRef.counts.passed} tests), skip-all trick fails, untouched state fails`);

  console.log('integrity verification passed');
} catch (error) {
  console.error(error);
  process.exitCode = 1;
} finally {
  server?.close();
  fs.rmSync(root, { recursive: true, force: true });
}
