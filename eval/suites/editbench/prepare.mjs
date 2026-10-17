// Materialize the edit-format benchmark: pick real source files from pinned commits, apply one
// reversible structural mutation each, and write the task (mutated file as the workspace template,
// original kept hidden for scoring).
import fsp from 'node:fs/promises';
import path from 'node:path';
import { HIDDEN, SRC, TOOLCHAINS } from '../../lib/paths.mjs';
import { formatSource } from '../../lib/formatters.mjs';
import { ensureNodeTools, ensurePyTools, py, T } from '../../lib/toolchains.mjs';
import { ensureDir, exists, log, prng, rmrf, run, runOk, sha256, shuffle, writeJson, writeText } from '../../lib/util.mjs';
import { EDITBENCH_REPOS, EXCLUDE, LANGS, MAX_LINES, MIN_LINES, SEED, TASKS_PER_LANG } from './repos.mjs';
import { taskPrompt } from './prompt.mjs';

const MUTATE_PY = path.join(path.dirname(new URL(import.meta.url).pathname), 'mutate.py');
export const EDITBENCH_DIR = path.join(HIDDEN, 'editbench');

async function ensureRepo(repo) {
  const dir = path.join(SRC, repo.dir);
  if (!await exists(dir)) {
    await ensureDir(dir);
    await runOk('git', ['init', '-q'], { cwd: dir });
    await runOk('git', ['remote', 'add', 'origin', repo.url], { cwd: dir });
  }
  const has = await run('git', ['cat-file', '-e', `${repo.commit}^{commit}`], { cwd: dir });
  if (has.code !== 0) {
    await runOk('git', ['fetch', '-q', '--depth', '1', 'origin', repo.commit], { cwd: dir, timeoutMs: 1800000 });
  }
  return dir;
}

async function listFiles(repo, dir) {
  const res = await runOk('git', ['ls-tree', '-r', '--name-only', repo.commit], { cwd: dir });
  return res.stdout.split('\n').filter(Boolean)
    .filter((p) => repo.include.some((re) => re.test(p)))
    .filter((p) => !EXCLUDE.some((re) => re.test(p)));
}

async function showFile(dir, commit, file) {
  const res = await run('git', ['show', `${commit}:${file}`], { cwd: dir });
  if (res.code !== 0) throw new Error(`git show ${commit}:${file} failed`);
  return res.stdout;
}

function tsLang(file) {
  if (file.endsWith('.py')) return 'python';
  if (file.endsWith('.rs')) return 'rust';
  if (file.endsWith('.tsx')) return 'tsx';
  return 'typescript';
}

async function mutateCall(req) {
  const res = await run(py(T.pyTools), ['-I', MUTATE_PY], {
    input: JSON.stringify(req), timeoutMs: 120000,
    env: { ...process.env, CCEVAL_TS_CACHE: path.join(TOOLCHAINS, 'ts-grammars') },
  });
  if (res.code !== 0) throw new Error(`mutate.py failed: ${res.stderr.slice(-2000)}`);
  return JSON.parse(res.stdout);
}

function applyMutation(source, cand) {
  const buf = Buffer.from(source, 'utf8');
  return Buffer.concat([buf.subarray(0, cand.start), Buffer.from(cand.replacement, 'utf8'), buf.subarray(cand.end)]).toString('utf8');
}

/** Quota per mutation type: equal shares, remainder to the earlier types in the fixed order. */
export function quotas(types, total) {
  const base = Math.floor(total / types.length);
  const q = {};
  types.forEach((t, i) => { q[t] = base + (i < total % types.length ? 1 : 0); });
  return q;
}

export async function prepareEditbench({ force = false } = {}) {
  const manifestPath = path.join(EDITBENCH_DIR, 'tasks.json');
  if (!force && await exists(manifestPath)) {
    log('editbench: already prepared');
    return JSON.parse(await fsp.readFile(manifestPath, 'utf8'));
  }
  await ensurePyTools();
  await ensureNodeTools();
  await rmrf(EDITBENCH_DIR);
  const tasks = [];
  const discarded = [];
  for (const [lang, spec] of Object.entries(LANGS)) {
    const rand = prng(`${SEED}:${lang}`);
    const repos = EDITBENCH_REPOS.filter((r) => r.lang === lang);
    // Interleave shuffled file lists so repositories contribute evenly.
    const perRepo = [];
    for (const repo of repos) {
      const dir = await ensureRepo(repo);
      const files = shuffle(await listFiles(repo, dir), rand);
      perRepo.push(files.map((file) => ({ repo, dir, file })));
    }
    const order = [];
    for (let i = 0; perRepo.some((l) => i < l.length); i++) for (const l of perRepo) if (i < l.length) order.push(l[i]);

    const remaining = quotas(spec.types, TASKS_PER_LANG);
    let n = 0;
    for (const { repo, dir, file } of order) {
      if (Object.values(remaining).every((v) => v === 0)) break;
      const source = await showFile(dir, repo.commit, file);
      const lines = source.split('\n').length;
      if (lines < MIN_LINES || lines > MAX_LINES) continue;
      const tl = tsLang(file);
      const res = await mutateCall({ op: 'candidates', lang: tl, source });
      if (res.parse_errors) { discarded.push({ repo: repo.id, file, reason: 'original does not parse cleanly' }); continue; }
      const available = spec.types.filter((t) => remaining[t] > 0 && res.candidates.some((c) => c.type === t));
      if (!available.length) continue;
      // Most-needed type first (ties broken by the seeded PRNG).
      available.sort((a, b) => remaining[b] - remaining[a] || (rand() < 0.5 ? -1 : 1));
      let made = false;
      for (const type of available) {
        const cands = shuffle(res.candidates.filter((c) => c.type === type), rand);
        for (const cand of cands.slice(0, 5)) {
          const mutated = applyMutation(source, cand);
          const check = await mutateCall({ op: 'check', lang: tl, source: mutated });
          if (check.parse_errors) { discarded.push({ repo: repo.id, file, type, reason: 'mutated file does not parse' }); continue; }
          const fo = await formatSource(file, source, { rustEdition: repo.rustEdition });
          if (!fo.ok) { discarded.push({ repo: repo.id, file, reason: `formatter rejects original: ${fo.error.slice(0, 200)}` }); break; }
          const fm = await formatSource(file, mutated, { rustEdition: repo.rustEdition });
          if (!fm.ok || fm.text === fo.text) { discarded.push({ repo: repo.id, file, type, reason: 'mutation not visible after formatting' }); continue; }
          n++;
          const id = `eb-${lang}-${String(n).padStart(3, '0')}`;
          const taskDir = path.join(EDITBENCH_DIR, id);
          await writeText(path.join(taskDir, 'original', file), source);
          await writeText(path.join(taskDir, 'template', file), mutated);
          const mutation = { ...cand };
          const task = {
            id, suite: 'editbench', lang, repo: repo.id, repo_url: repo.url, commit: repo.commit, path: file,
            rust_edition: repo.rustEdition ?? null,
            mutation, prompt: taskPrompt(file, mutation),
            original_sha256: sha256(source), mutated_sha256: sha256(mutated),
            formatted_original_sha256: sha256(fo.text),
          };
          await writeJson(path.join(taskDir, 'task.json'), task);
          tasks.push(task);
          remaining[type]--;
          made = true;
          break;
        }
        if (made) break;
      }
    }
    const short = Object.entries(remaining).filter(([, v]) => v > 0);
    if (short.length) throw new Error(`editbench ${lang}: not enough candidates for ${JSON.stringify(Object.fromEntries(short))}`);
    log(`editbench ${lang}: ${n} tasks`);
  }
  const manifest = { suite: 'editbench', seed: SEED, tasks_per_lang: TASKS_PER_LANG, repos: EDITBENCH_REPOS.map(({ include, ...r }) => ({ ...r, include: include.map(String) })), tasks: tasks.map((t) => ({ id: t.id, lang: t.lang, type: t.mutation.type, repo: t.repo, path: t.path })), discarded };
  await writeJson(path.join(EDITBENCH_DIR, 'discarded.json'), discarded);
  await writeJson(manifestPath, manifest);
  return manifest;
}
