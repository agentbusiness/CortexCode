// Mine realistic tasks from repository history (SWE-smith style):
//   1. take a commit that changes source and tests together;
//   2. revert its source change (tests go back to the parent's version, new tests are removed);
//   3. the commit message (and a linked issue, fetched now and cached) is the task;
//   4. the commit's tests are the hidden verifier.
// A task is kept only if the original commit passes the hidden tests (twice, to reject flaky
// tests) and the reverted state fails them. Every discarded candidate is recorded with its reason.
import { execFile } from 'node:child_process';
import fs from 'node:fs';
import fsp from 'node:fs/promises';
import path from 'node:path';
import { SRC, tmpRoot } from '../../lib/paths.mjs';
import { ensureBats, ensureNodeTools } from '../../lib/toolchains.mjs';
import { cloneTree, ensureDir, exists, log, rmrf, run, runOk, writeJson } from '../../lib/util.mjs';
import { MINING, REALISTIC_REPOS } from './repos.mjs';
import { cacheDeps, prebuildRust } from './deps.mjs';
import { realisticPrompt, realisticScore, REALISTIC_DIR } from './suite.mjs';

async function ensureSource(repo) {
  const dir = path.join(SRC, repo.dir);
  if (await exists(dir)) return dir;
  if (repo.local) {
    // Read-only copy of the owner's repository: `git clone` only reads the original.
    await runOk('git', ['clone', '-q', '--no-hardlinks', repo.local, dir], { timeoutMs: 1800000 });
  } else {
    await runOk('git', ['clone', '-q', repo.url, dir], { timeoutMs: 1800000 });
  }
  return dir;
}

async function git(dir, args, opts = {}) {
  const res = await run('git', args, { cwd: dir, timeoutMs: 600000, ...opts });
  if (res.code !== 0) throw new Error(`git ${args.join(' ')}: ${res.stderr.slice(0, 500)}`);
  return res.stdout;
}

async function listCommits(dir, n) {
  const out = await git(dir, ['log', '--no-merges', `-n${n}`, '--format=%H%x00%P%x00%aI%x00%s%x00%b%x1e']);
  return out.split('\x1e').map((r) => r.replace(/^\n/, '')).filter(Boolean).map((r) => {
    const [sha, parents, date, subject, body] = r.split('\x00');
    return { sha, parent: parents.split(' ')[0], date, subject, body: (body ?? '').trim() };
  }).filter((c) => c.parent);
}

async function changes(dir, parent, sha) {
  const out = await git(dir, ['diff', '--no-renames', '--name-status', parent, sha]);
  const files = out.split('\n').filter(Boolean).map((l) => {
    const [status, ...rest] = l.split('\t');
    return { status: status[0], path: rest.join('\t') };
  });
  const num = await git(dir, ['diff', '--no-renames', '--numstat', parent, sha]);
  const lines = {};
  for (const l of num.split('\n').filter(Boolean)) {
    const [a, d, ...p] = l.split('\t');
    lines[p.join('\t')] = (Number(a) || 0) + (Number(d) || 0);
  }
  return files.map((f) => ({ ...f, lines: lines[f.path] ?? 0 }));
}

async function showTo(dir, rev, file, dest) {
  // Binary-safe blob read.
  const buf = await new Promise((resolve, reject) => {
    execFile('git', ['cat-file', 'blob', `${rev}:${file}`], { cwd: dir, encoding: 'buffer', maxBuffer: 1 << 30 },
      (err, stdout) => (err ? reject(new Error(`git cat-file ${rev}:${file}: ${err.message}`)) : resolve(stdout)));
  });
  await ensureDir(path.dirname(dest));
  await rmrf(dest);
  // Preserve the executable bit recorded in git (scripts under test must stay runnable).
  const ls = await run('git', ['ls-tree', rev, '--', file], { cwd: dir });
  const mode = ls.stdout.split(' ')[0];
  if (mode === '120000') {
    await fsp.symlink(buf.toString('utf8'), dest);
    return;
  }
  await fsp.writeFile(dest, buf, { mode: mode === '100755' ? 0o755 : 0o644 });
}

async function exportTree(dir, rev, dest) {
  await rmrf(dest);
  await ensureDir(dest);
  await runOk('/bin/sh', ['-c', `git -C "${dir}" archive --format=tar ${rev} | tar -x -C "${dest}"`], { timeoutMs: 600000 });
}

/** GitHub issue text linked through the commit's PR (closing keywords), fetched at prepare time. */
async function linkedIssues(repo, commit) {
  if (!repo.url) return [];
  const slug = repo.url.replace(/^https:\/\/github.com\//, '').replace(/\.git$/, '');
  const refs = new Set();
  const closing = /\b(?:close[sd]?|fix(?:e[sd])?|resolve[sd]?)\s+#(\d+)/gi;
  for (const m of `${commit.subject}\n${commit.body}`.matchAll(closing)) refs.add(Number(m[1]));
  const pr = commit.subject.match(/\(#(\d+)\)\s*$/);
  if (pr) {
    const res = await run('gh', ['api', `repos/${slug}/pulls/${pr[1]}`], { timeoutMs: 60000 });
    if (res.code === 0) {
      const body = JSON.parse(res.stdout).body ?? '';
      for (const m of body.matchAll(closing)) refs.add(Number(m[1]));
    }
  }
  const issues = [];
  for (const n of [...refs].sort((a, b) => a - b)) {
    const res = await run('gh', ['api', `repos/${slug}/issues/${n}`], { timeoutMs: 60000 });
    if (res.code !== 0) continue;
    const j = JSON.parse(res.stdout);
    if (j.pull_request) continue;
    issues.push({ number: n, title: j.title, body: j.body ?? '', url: j.html_url });
  }
  return issues;
}

function classify(repo, files) {
  const source = [], tests = [], other = [];
  for (const f of files) {
    if (repo.testRe.test(f.path)) tests.push(f);
    else if (repo.sourceRe.test(f.path)) source.push(f);
    else other.push(f);
  }
  return { source, tests, other };
}

/** Build template (reverted state), hidden tests and the reference solution for a candidate. */
async function buildCandidate(dir, commit, cls, out) {
  const template = path.join(out, 'template');
  await exportTree(dir, commit.sha, template);
  for (const f of cls.source) {
    const at = path.join(template, f.path);
    if (f.status === 'A') await rmrf(at);
    else await showTo(dir, commit.parent, f.path, at);
    if (f.status !== 'D') await showTo(dir, commit.sha, f.path, path.join(out, 'solution', f.path));
  }
  for (const f of cls.tests) {
    const at = path.join(template, f.path);
    if (f.status === 'A') await rmrf(at);
    else await showTo(dir, commit.parent, f.path, at);
    if (f.status !== 'D') await showTo(dir, commit.sha, f.path, path.join(out, 'hidden', f.path));
  }
}

async function referenceWorkspace(task, candDir, dest) {
  await cloneTree(path.join(candDir, 'template'), dest);
  for (const f of task.source_files) {
    if (f.status === 'D') await rmrf(path.join(dest, f.path));
    else {
      await ensureDir(path.dirname(path.join(dest, f.path)));
      await fsp.copyFile(path.join(candDir, 'solution', f.path), path.join(dest, f.path));
    }
  }
}

function wordCount(s) {
  return s.trim().split(/\s+/).filter(Boolean).length;
}

export async function mineRepo(repo, { target, scratchRoot, discarded }) {
  const dir = await ensureSource(repo);
  const commits = await listCommits(dir, MINING.scanCommits);
  const accepted = [];
  for (const commit of commits) {
    if (accepted.length >= target) break;
    const id = `rl-${repo.id}-${commit.sha.slice(0, 10)}`;
    const reject = (reason, extra = {}) => discarded.push({ id, repo: repo.id, commit: commit.sha, subject: commit.subject, reason, ...extra });
    const files = await changes(dir, commit.parent, commit.sha);
    const cls = classify(repo, files);
    if (!cls.source.length || !cls.tests.length) continue; // not a source+test commit: not a candidate
    const runnable = cls.tests.filter((f) => f.status !== 'D' && repo.runnable(f.path)).map((f) => f.path);
    if (!runnable.length) { reject('no runnable test file among the changed tests'); continue; }
    if (cls.source.length > MINING.maxSourceFiles) { reject(`source files changed ${cls.source.length} > ${MINING.maxSourceFiles}`); continue; }
    const srcLines = cls.source.reduce((s, f) => s + f.lines, 0);
    if (srcLines > MINING.maxSourceChangedLines) { reject(`source lines changed ${srcLines} > ${MINING.maxSourceChangedLines}`); continue; }
    if (wordCount(commit.subject) < MINING.minSubjectWords && !commit.body) { reject(`commit message too short (${wordCount(commit.subject)} words, no body)`); continue; }

    const candDir = path.join(scratchRoot, 'cand', id);
    await rmrf(candDir);
    await buildCandidate(dir, commit, cls, candDir);
    const task = {
      id, suite: 'realistic', repo: repo.id, lang: repo.lang, origin: repo.local ? 'owner' : 'public',
      source_repo: repo.url ?? 'local reference repository', commit: commit.sha, parent: commit.parent, date: commit.date,
      subject: commit.subject, body: commit.body, issues: [],
      source_files: cls.source.map((f) => ({ path: f.path, status: f.status, lines: f.lines })),
      other_files: cls.other.map((f) => ({ path: f.path, status: f.status })),
      hidden_tests: cls.tests.filter((f) => f.status !== 'D').map((f) => f.path),
      test_deletions: cls.tests.filter((f) => f.status === 'D').map((f) => f.path),
      runnable_tests: runnable, source_lines_changed: srcLines,
    };
    try {
      task.deps = await cacheDeps(repo, path.join(candDir, 'template'));
    } catch (error) {
      reject('dependency install failed', { error: String(error).slice(0, 600) });
      await rmrf(candDir);
      continue;
    }
    const verify = path.join(scratchRoot, 'verify');
    const logs = path.join(candDir, 'logs');
    // (a) original commit passes the hidden tests, twice.
    await referenceWorkspace(task, candDir, path.join(verify, 'reference'));
    const ref1 = await realisticScore(task, path.join(verify, 'reference'), path.join(verify, 'run'), path.join(logs, 'reference-1'), `mine-${repo.id}`, { taskDir: candDir });
    if (!ref1.passed || ref1.counts.passed === 0) {
      reject('original commit does not pass its tests', { exit_code: ref1.exit_code, counts: ref1.counts, timed_out: ref1.timed_out, tail: ref1.output.slice(-1200) });
      await rmrf(candDir);
      continue;
    }
    task.reference = { passed: ref1.counts.passed, ms: ref1.ms };
    const ref2 = await realisticScore(task, path.join(verify, 'reference'), path.join(verify, 'run'), path.join(logs, 'reference-2'), `mine-${repo.id}`, { taskDir: candDir });
    if (!ref2.passed) {
      reject('original commit passes only intermittently (flaky)', { counts: ref2.counts });
      await rmrf(candDir);
      continue;
    }
    // (b) reverted state fails them.
    const rev = await realisticScore(task, path.join(candDir, 'template'), path.join(verify, 'run'), path.join(logs, 'reverted'), `mine-${repo.id}`, { taskDir: candDir });
    if (rev.passed) {
      reject('reverted state already passes the commit tests', { counts: rev.counts });
      await rmrf(candDir);
      continue;
    }
    task.reverted = { exit_code: rev.exit_code, counts: rev.counts, timed_out: rev.timed_out, ms: rev.ms };
    task.issues = await linkedIssues(repo, commit);
    task.prompt = realisticPrompt(task);
    const final = path.join(REALISTIC_DIR, id);
    await rmrf(final);
    await ensureDir(REALISTIC_DIR);
    await fsp.rename(candDir, final);
    if (repo.deps.kind === 'cargo') {
      const shared = process.env.CCEVAL_MINING_CARGO_TARGET;
      delete process.env.CCEVAL_MINING_CARGO_TARGET;
      task.prebuild = await prebuildRust(repo, task, path.join(final, 'template'));
      process.env.CCEVAL_MINING_CARGO_TARGET = shared;
    }
    await writeJson(path.join(final, 'task.json'), task);
    accepted.push(task);
    log(`realistic ${repo.id}: accepted ${id} (${accepted.length}/${target}) ref ${task.reference.passed} tests, reverted exit ${rev.exit_code}`);
  }
  return accepted;
}

export async function prepareRealistic({ force = false, only = null } = {}) {
  const manifestPath = path.join(REALISTIC_DIR, 'tasks.json');
  if (!force && !only && await exists(manifestPath)) {
    log('realistic: already prepared');
    return JSON.parse(await fsp.readFile(manifestPath, 'utf8'));
  }
  await ensureNodeTools();
  await ensureBats();
  const prev = await exists(manifestPath) ? JSON.parse(await fsp.readFile(manifestPath, 'utf8')) : { tasks: [], discarded: [] };
  const scratchRoot = path.join(tmpRoot(), 'cceval-mine');
  const tasks = only ? prev.tasks.filter((t) => !only.includes(t.repo)) : [];
  const discarded = only ? prev.discarded.filter((d) => !only.includes(d.repo)) : [];
  for (const repo of REALISTIC_REPOS) {
    if (only && !only.includes(repo.id)) continue;
    for (const old of fs.existsSync(REALISTIC_DIR) ? fs.readdirSync(REALISTIC_DIR) : []) {
      if (old.startsWith(`rl-${repo.id}-`)) await rmrf(path.join(REALISTIC_DIR, old));
    }
    const repoDiscarded = [];
    if (repo.deps.kind === 'cargo') process.env.CCEVAL_MINING_CARGO_TARGET = path.join(scratchRoot, repo.id, 'cargo-target');
    else delete process.env.CCEVAL_MINING_CARGO_TARGET;
    const accepted = await mineRepo(repo, { target: MINING.perRepoTarget[repo.id], scratchRoot: path.join(scratchRoot, repo.id), discarded: repoDiscarded });
    discarded.push(...repoDiscarded);
    tasks.push(...accepted.map((t) => ({ id: t.id, repo: t.repo, lang: t.lang, origin: t.origin, commit: t.commit, subject: t.subject })));
    await writeJson(manifestPath, manifestOf(tasks, discarded));
  }
  delete process.env.CCEVAL_MINING_CARGO_TARGET;
  await rmrf(scratchRoot);
  const manifest = manifestOf(tasks, discarded);
  await writeJson(manifestPath, manifest);
  return manifest;
}

function manifestOf(tasks, discarded) {
  const counts = {};
  for (const t of tasks) counts[t.lang] = (counts[t.lang] ?? 0) + 1;
  return {
    suite: 'realistic', mining: { ...MINING }, repos: REALISTIC_REPOS.map((r) => ({ id: r.id, lang: r.lang, url: r.url ?? null, local: r.local ? 'local' : null })),
    counts, tasks, discarded,
  };
}

