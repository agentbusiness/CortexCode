#!/usr/bin/env node
// Materialize every suite offline (network is used here, never during runs), then write
// eval/manifest.json: per-suite task ids, counts and a content hash of every task directory.
//
//   node eval/prepare.mjs [--suite editbench|polyglot|realistic|livecodebench|bigcodebench|rustevo|webbench|swebench|terminalbench|all]
//        [--force] [--only repo,repo|project,project|instance,instance|task,task] [--no-validate]
//        [--images x86_64|arm64|all] [--validate] [--workers N]
//
// Container suites: swebench and terminalbench prepare their datasets, task stores and harness venvs
// (swebench 4.1.0, harbor 0.24.0) without Docker. Docker work is opt-in:
//   --suite swebench --images x86_64|arm64|all [--validate]   pull/build the harness's instance
//        images; --validate grades each instance's gold patch in them (the default image policy uses
//        a native arm64 image only where its gold patch resolved)
//   --suite terminalbench --validate   run Harbor's oracle agent on every task (recorded controls)
//
// Each suite records its upstream sources (URL, pinned revision, SHA-256 of downloaded artifacts)
// and its controls (reference solutions must pass, untouched starting states must fail) in its
// tasks.json; the manifest carries the sources, counts, exclusions and a content hash per task.
import fsp from 'node:fs/promises';
import path from 'node:path';
import { HIDDEN, MANIFEST } from './lib/paths.mjs';
import { ensureAider, ensureBats, ensureMini, ensureNodeTools, ensurePolyglotJs, ensurePolyglotPy, ensurePyTools, PINS } from './lib/toolchains.mjs';
import { log, parseArgs, readJson, sha256, walk, writeJson } from './lib/util.mjs';
import { prepareEditbench } from './suites/editbench/prepare.mjs';
import { preparePolyglot } from './suites/polyglot/prepare.mjs';
import { prepareRealistic } from './suites/realistic/mine.mjs';
import { prepareLivecodebench } from './suites/livecodebench/prepare.mjs';
import { prepareBigcodebench } from './suites/bigcodebench/prepare.mjs';
import { prepareRustevo } from './suites/rustevo/prepare.mjs';
import { prepareWebbench } from './suites/webbench/prepare.mjs';
import { prepareSwebench } from './suites/swebench/prepare.mjs';
import { prepareTerminalbench } from './suites/terminalbench/prepare.mjs';

export const PREPARED_SUITES = ['editbench', 'polyglot', 'realistic', 'livecodebench', 'bigcodebench', 'rustevo', 'webbench', 'swebench', 'terminalbench'];

/** Content hash of a task directory: sorted (path, sha256) of every file. */
export async function taskDirHash(dir) {
  const files = await walk(dir);
  const parts = [];
  for (const f of files) {
    const abs = path.join(dir, f.rel);
    const data = f.symlink ? `symlink:${await fsp.readlink(abs)}` : await fsp.readFile(abs);
    parts.push(`${f.rel}\0${sha256(data)}`);
  }
  return sha256(parts.join('\n'));
}

export async function suiteEntry(name, manifestTasks, meta = {}) {
  const tasks = [];
  const byLang = {};
  const byGroup = {};
  let excluded = 0;
  for (const t of manifestTasks) {
    tasks.push({ id: t.id, lang: t.lang, ...(t.group ? { group: t.group } : {}), ...(t.excluded ? { excluded: t.excluded } : {}), hash: await taskDirHash(path.join(HIDDEN, name, t.id)) });
    if (t.excluded) { excluded++; continue; }
    byLang[t.lang] = (byLang[t.lang] ?? 0) + 1;
    if (t.group) byGroup[t.group] = (byGroup[t.group] ?? 0) + 1;
  }
  const entry = { count: tasks.length - excluded, by_lang: byLang };
  if (Object.keys(byGroup).length) entry.by_group = byGroup;
  if (excluded) entry.excluded = excluded;
  if (meta.sources) entry.sources = meta.sources.map(({ path: _p, ...r }) => r);
  entry.tasks = tasks;
  return entry;
}

async function main() {
  const args = parseArgs(process.argv.slice(2));
  const which = !args.suite || args.suite === 'all' ? PREPARED_SUITES : String(args.suite).split(',');
  const force = !!args.force;
  const validateAll = !args['no-validate'];
  const only = args.only ? String(args.only).split(',') : null;
  await ensurePyTools();
  await ensureNodeTools();
  await ensurePolyglotPy();
  await ensurePolyglotJs();
  await ensureBats();
  await ensureAider();
  await ensureMini();
  if (which.includes('editbench')) await prepareEditbench({ force });
  if (which.includes('polyglot')) await preparePolyglot({ force });
  if (which.includes('realistic')) await prepareRealistic({ force, only });
  if (which.includes('livecodebench')) await prepareLivecodebench({ force, validateAll });
  if (which.includes('bigcodebench')) await prepareBigcodebench({ force, validateAll });
  if (which.includes('rustevo')) await prepareRustevo({ force, validateAll });
  if (which.includes('webbench')) await prepareWebbench({ force, validateAll, only });
  const workers = args.workers ? Number(args.workers) : 2;
  if (which.includes('swebench')) await prepareSwebench({ force, images: args.images ?? null, validate: !!args.validate, only, workers });
  if (which.includes('terminalbench')) await prepareTerminalbench({ force, validate: !!args.validate, only, workers });

  // Suites not named in --suite keep their existing manifest entry (their task stores are not
  // touched by this run); `--suite all` (the default) re-reads every suite.
  const previous = await readJson(MANIFEST, { suites: {} });
  const manifest = { generated: new Date().toISOString(), pins: PINS, suites: {} };
  for (const name of PREPARED_SUITES) {
    if (!which.includes(name) && previous.suites?.[name]) { manifest.suites[name] = previous.suites[name]; continue; }
    const m = await readJson(path.join(HIDDEN, name, 'tasks.json'), null);
    if (!m) continue;
    manifest.suites[name] = await suiteEntry(name, m.tasks, { sources: m.sources });
    log(`${name}: ${manifest.suites[name].count} tasks ${JSON.stringify(manifest.suites[name].by_lang)}${manifest.suites[name].excluded ? `, ${manifest.suites[name].excluded} excluded` : ''}`);
  }
  await writeJson(MANIFEST, manifest);
  log(`wrote ${MANIFEST}`);
}

if (import.meta.url === `file://${process.argv[1]}`) {
  main().catch((e) => { console.error(e); process.exit(1); });
}
