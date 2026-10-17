#!/usr/bin/env node
// Runs a list of bench jobs one after another; a failed job is recorded and the queue moves on.
// node bench/engines/harness/queue.mjs <jobs.json>    jobs: [{engine, config, suite, tag?, args?: []}]
import { spawn } from 'node:child_process';
import fs from 'node:fs';
import path from 'node:path';
import { HERE, ENGINES_DIR } from './lib.mjs';

const jobs = JSON.parse(fs.readFileSync(process.argv[2], 'utf8'));
const logFile = path.join(ENGINES_DIR, 'raw', 'queue.log');
const log = (s) => { const l = `[${new Date().toISOString()}] ${s}`; console.log(l); fs.appendFileSync(logFile, l + '\n'); };
const run = (j) => new Promise((resolve) => {
  const args = [path.join(HERE, 'bench.mjs'), '--engine', j.engine, '--config', j.config, '--suite', j.suite, ...(j.tag ? ['--tag', j.tag] : []), ...(j.args || [])];
  const p = spawn(process.execPath, args, { stdio: ['ignore', 'pipe', 'pipe'] });
  let last = '';
  p.stdout.on('data', (d) => { last = d.toString().trim().split('\n').pop(); });
  p.stderr.on('data', (d) => fs.appendFileSync(logFile, d));
  p.on('exit', (code) => resolve({ code, last }));
});
for (const j of jobs) {
  log(`start ${j.engine}/${j.config}/${j.suite}`);
  const r = await run(j);
  log(`end ${j.engine}/${j.config}/${j.suite} exit ${r.code} ${r.last}`);
}
log('queue done');
