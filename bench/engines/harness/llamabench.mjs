#!/usr/bin/env node
// Cross-check of the baseline with llama.cpp's own llama-bench (no HTTP, no chat template):
// pp2048 / pp8192 prefill and tg256 decode, repeated, JSON output saved raw.
// node bench/engines/harness/llamabench.mjs [--tag name] [-- extra llama-bench args]
import { spawn } from 'node:child_process';
import fs from 'node:fs';
import path from 'node:path';
import { ENGINES_DIR } from './lib.mjs';
import { W } from './engines.mjs';

const argv = process.argv.slice(2);
const dd = argv.indexOf('--');
const extra = dd >= 0 ? argv.slice(dd + 1) : [];
const tag = argv.includes('--tag') ? argv[argv.indexOf('--tag') + 1] : 'default';
const out = path.join(ENGINES_DIR, 'raw', 'llamacpp-bench', `${tag}-${new Date().toISOString().replace(/[:.]/g, '-')}`);
fs.mkdirSync(out, { recursive: true });
const meta = { tool: 'llama-bench', tag };
const args = ['-m', W.gguf_ud_q4km, '-ngl', '99', '-p', '2048,8192', '-n', '256', '-r', '3', '-o', 'json', ...extra];
meta.args = args;
const code = await new Promise((res) => {
  const p = spawn(path.join(ENGINES_DIR, 'llamacpp/build/bin/llama-bench'), args, { stdio: ['ignore', fs.openSync(path.join(out, 'bench.json'), 'w'), fs.openSync(path.join(out, 'stderr.log'), 'w')] });
  p.on('exit', res);
});
meta.exit = code; meta.status = code === 0 ? 'complete' : 'failed';
fs.writeFileSync(path.join(out, 'meta.json'), JSON.stringify(meta, null, 1));
console.log(out, meta.status);
process.exit(code);
