#!/usr/bin/env node
// G1 check: the engine survey covers the required engines with sourced verdicts that are
// consistent with the hard requirements, and every qualifying engine is really installed at the
// version/commit the survey records (re-read from disk here, not trusted from the file).
import fs from 'node:fs';
import path from 'node:path';
import { execFileSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';

const DIR = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const fails = [];
const check = (cond, msg) => { if (!cond) fails.push(msg); };
const sh = (cmd, args) => { try { return execFileSync(cmd, args, { encoding: 'utf8', stdio: ['ignore', 'pipe', 'ignore'] }).trim(); } catch { return null; } };

const survey = JSON.parse(fs.readFileSync(path.join(DIR, 'survey.json'), 'utf8'));
const md = fs.readFileSync(path.join(DIR, 'survey.md'), 'utf8');
const HARD = ['arch', 'metal', 'four_bit', 'streaming', 'rust_usable'];
const NICE = ['grammar', 'tool_parse', 'prefix_cache', 'batching', 'spec_mtp', 'kv_quant'];
const byId = new Map(survey.engines.map((e) => [e.id, e]));

/** Structural consistency of one engine record; returns the problems found. */
export function consistency(e) {
  const p = [];
  if (!['candidate', 'not-candidate', 'not-tested', 'excluded', 'reference', 'binding-of-baseline'].includes(e.verdict)) p.push(`${e.id}: unknown verdict ${e.verdict}`);
  for (const k of HARD) {
    const h = e.hard?.[k];
    if (!h || !('ok' in h) || typeof h.evidence !== 'string' || !h.evidence.length) p.push(`${e.id}: hard requirement ${k} lacks a verdict with evidence`);
  }
  const allOk = HARD.every((k) => e.hard?.[k]?.ok === true);
  if (e.verdict === 'candidate' && !allOk) p.push(`${e.id}: marked candidate but a hard requirement is not met`);
  if (e.verdict === 'not-candidate' && allOk) p.push(`${e.id}: marked not-candidate although every hard requirement is met (give the failing one)`);
  if (e.verdict === 'excluded' && !/raw\//.test(e.verdict_reason)) p.push(`${e.id}: exclusion without a raw-log reference`);
  if (!Array.isArray(e.sources) || !e.sources.length) p.push(`${e.id}: no sources`);
  if (!/^https:\/\/github\.com\//.test(e.repo || '')) p.push(`${e.id}: repo is not a GitHub URL`);
  if (typeof e.verdict_reason !== 'string' || e.verdict_reason.length < 20) p.push(`${e.id}: verdict reason missing`);
  return p;
}

// 1. coverage
for (const id of ['llamacpp', 'mistralrs', 'candle', 'mlx-rs', 'lumen', 'saragossa', 'rmlx']) check(byId.has(id), `required engine ${id} missing from the survey`);
check(survey.engines.filter((e) => !['llamacpp', 'mistralrs', 'candle', 'mlx-rs', 'lumen', 'saragossa', 'rmlx'].includes(e.id) && e.role !== 'reference').length >= 1, 'no other engine beyond the required list was surveyed');
check(survey.engines.filter((e) => e.role === 'reference').map((e) => e.id).includes('omlx') && byId.has('vllm-metal'), 'oMLX and vllm-metal must be recorded as references');
for (const id of ['omlx', 'vllm-metal']) check(byId.get(id)?.verdict === 'reference', `${id} must be a reference, not a candidate`);
// 2. owner's leads have an explicit assessment of the claim
for (const id of ['lumen', 'saragossa', 'rmlx']) check((byId.get(id)?.claim_assessment || '').length > 40, `owner lead ${id} lacks an assessment of the claim`);
// 3. consistency of every record
for (const e of survey.engines) fails.push(...consistency(e));
for (const e of survey.engines) for (const k of NICE) check(e.verdict !== 'candidate' || typeof e.nice?.[k] === 'boolean' || e.nice?.[k] === null, `${e.id}: nice-to-have ${k} not assessed`);
// 4. qualifying engines are installed exactly as recorded (recomputed from disk)
const quals = survey.engines.filter((e) => e.verdict === 'candidate');
check(quals.length >= 4, `only ${quals.length} qualifying engines`);
for (const e of quals) {
  const i = e.installed;
  if (!i) { fails.push(`${e.id}: qualifying engine has no installation record`); continue; }
  for (const b of i.binaries || []) {
    const p = path.join(DIR, b.path.replace(/^bench\/engines\//, ''));
    check(fs.existsSync(p), `${e.id}: binary ${b.path} missing`);
    if (fs.existsSync(p)) check(sh('shasum', ['-a', '256', p])?.split(/\s+/)[0] === b.sha256, `${e.id}: binary ${b.path} changed since the survey was written`);
  }
  check((i.binaries || []).length > 0, `${e.id}: no binary recorded`);
  if (i.method === 'source') {
    const d = path.join(DIR, i.source?.path || '');
    check(i.source?.commit && sh('git', ['-C', d, 'rev-parse', 'HEAD']) === i.source.commit, `${e.id}: source commit in survey does not match ${i.source?.path}`);
  } else {
    check(i.asset_file?.sha256 && sh('shasum', ['-a', '256', path.join(DIR, i.asset_file.file.replace(/^bench\/engines\//, ''))])?.split(/\s+/)[0] === i.asset_file.sha256, `${e.id}: release asset hash mismatch`);
    check(i.source?.commit, `${e.id}: release binary without the matching source checkout commit`);
  }
}
// 5. markdown covers every engine and verdict
for (const e of survey.engines) check(md.includes(e.name) && md.includes(`**${e.verdict}**`), `survey.md lacks ${e.name}`);
// 6. negative control: the consistency rule must reject a candidate with a failed hard requirement
const bad = JSON.parse(JSON.stringify(byId.get('saragossa')));
bad.hard.metal.ok = false;
check(consistency(bad).some((m) => m.includes('marked candidate')), 'negative control: consistency check accepted a candidate without Metal');
const bad2 = JSON.parse(JSON.stringify(byId.get('candle')));
for (const k of HARD) bad2.hard[k].ok = true;
check(consistency(bad2).some((m) => m.includes('not-candidate although')), 'negative control: accepted a not-candidate meeting every requirement');

if (fails.length) { console.error('engine survey verification FAILED:\n- ' + fails.join('\n- ')); process.exit(1); }
console.log(`${survey.engines.length} engines surveyed, ${quals.length} qualifying engines installed as recorded`);
console.log('engine survey verification passed');
