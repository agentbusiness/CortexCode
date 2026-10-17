#!/usr/bin/env node
// lumen-rs v0.11.1 delivers each SSE answer at once, so client timestamps cannot separate prefill and
// decode; this extracts the engine's own per-request log lines ("[mlx] seq N prefill: P tokens in
// X ms" / "[mlx] seq N done: G tokens in Y ms") into results/lumen-rs-enginelog.json (engine-reported).
import fs from 'node:fs';
import path from 'node:path';
import { ENGINES_DIR } from '../harness/lib.mjs';
import { stats } from '../harness/analyze.mjs';
const dir = fs.readdirSync(path.join(ENGINES_DIR, 'raw/lumen-rs/default')).filter((d) => d.startsWith('sweep-')).sort().pop();
const log = fs.readFileSync(path.join(ENGINES_DIR, 'raw/lumen-rs/default', dir, 'server.log'), 'utf8');
const pre = [...log.matchAll(/seq (\d+) prefill: (\d+) tokens in (\d+)ms/g)].map((m) => ({ seq: +m[1], tokens: +m[2], ms: +m[3] }));
const done = [...log.matchAll(/seq (\d+) done: (\d+) tokens in (\d+)ms/g)].map((m) => ({ seq: +m[1], tokens: +m[2], ms: +m[3] }));
const dec = done.filter((d) => d.tokens >= 200).map((d) => d.tokens / (d.ms / 1000));
const p2k = pre.filter((p) => p.tokens > 1900 && p.tokens < 2200).map((p) => p.tokens / (p.ms / 1000));
const p8k = pre.filter((p) => p.tokens > 8000 && p.tokens < 8300).map((p) => p.tokens / (p.ms / 1000));
const out = { source: `raw/lumen-rs/default/${dir}/server.log`, engine_reported: true, decode_tok_s: stats(dec), prefill2k_tok_s: stats(p2k), prefill8k_tok_s: stats(p8k), prefill: pre, done };
fs.writeFileSync(path.join(ENGINES_DIR, 'results/lumen-rs-enginelog.json'), JSON.stringify(out, null, 1));
console.log(JSON.stringify({ decode: out.decode_tok_s, p2k: out.prefill2k_tok_s, p8k: out.prefill8k_tok_s }));
