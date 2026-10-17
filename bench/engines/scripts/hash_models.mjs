#!/usr/bin/env node
// Records every model file the bake-off uses: path, size, SHA-256 computed locally, and the
// SHA-256 Hugging Face publishes for that file at the pinned revision (lfs.oid). Writes
// bench/engines/results/models.json. A mismatch is reported, never hidden.
import fs from 'node:fs';
import path from 'node:path';
import os from 'node:os';
import { execFileSync } from 'node:child_process';
import { ENGINES_DIR } from '../harness/lib.mjs';

const M = path.join(os.homedir(), '.cortexcode/models');
const SETS = [
  { use: 'llama.cpp baseline, xInfer, Crane (UD-Q4_K_M GGUF)', repo: 'unsloth/Qwen3.8-27B-GGUF', rev: '4ca720788d1e01f1bff70c033e0d0028fd02e502', dir: `${M}/unsloth-Qwen3.8-27B-GGUF/4ca720788d1e01f1bff70c033e0d0028fd02e502`, files: ['Qwen3.8-27B-UD-Q4_K_M.gguf'] },
  { use: 'quality reference (Q8_0 GGUF, llama.cpp)', repo: 'unsloth/Qwen3.8-27B-GGUF', rev: '4ca720788d1e01f1bff70c033e0d0028fd02e502', dir: `${M}/unsloth-Qwen3.8-27B-GGUF-Q8_0/4ca720788d1e01f1bff70c033e0d0028fd02e502`, files: ['Qwen3.8-27B-Q8_0.gguf'] },
  { use: 'Lumen (converted to .lbc), mistral.rs (Q4_0 GGUF)', repo: 'unsloth/Qwen3.8-27B-GGUF', rev: '4ca720788d1e01f1bff70c033e0d0028fd02e502', dir: `${M}/unsloth-Qwen3.8-27B-GGUF-Q4_0/4ca720788d1e01f1bff70c033e0d0028fd02e502`, files: ['Qwen3.8-27B-Q4_0.gguf'] },
  { use: 'MLX engines (rMLX, Saragossa, lumen-rs): MLX affine 4-bit gs64', repo: 'mlx-community/Qwen3.8-27B-4bit', rev: '10c35caafbb80f7dc6a7a432cdd11af10a6d4818', dir: `${M}/mlx-community-Qwen3.8-27B-4bit/10c35caafbb80f7dc6a7a432cdd11af10a6d4818`, files: ['model-00001-of-00003.safetensors', 'model-00002-of-00003.safetensors', 'model-00003-of-00003.safetensors', 'tokenizer.json'] },
  { use: 'MTP sidecar for MLX engines', repo: 'mlx-community/Qwen3.8-27B-MTP-4bit', rev: 'b643c01b6d3b094e325edb6ebd832e16c486c575', dir: `${M}/mlx-community-Qwen3.8-27B-MTP-4bit/b643c01b6d3b094e325edb6ebd832e16c486c575`, files: ['model.safetensors'] },
  { use: 'DFlash2 drafter for llama.cpp', repo: 'ggml-org/Qwen3.8-27B-GGUF', rev: '71bc7b627595dc8a91039addd9c791ae548d6747', dir: `${M}/ggml-org-Qwen3.8-27B-GGUF-dflash/71bc7b627595dc8a91039addd9c791ae548d6747`, files: ['dflash-Qwen3.8-27B-Q8_0.gguf', 'dflash-Qwen3.8-27B-Q4_0.gguf'] },
  { use: 'uzu (Mirai-M 4-bit + speculator)', repo: 'trymirai/Qwen3.8-27B-M', rev: '8b878f57094a616c0ea4c214fe774af0cbfb4353', dir: `${M}/trymirai-Qwen3.8-27B-M/8b878f57094a616c0ea4c214fe774af0cbfb4353`, files: ['model.safetensors', 'speculator/model.safetensors'] },
  { use: 'mistral.rs ISQ source (official BF16)', repo: 'Qwen/Qwen3.8-27B', rev: '1d4bf0f2ff6012fd82039f2fa52739d0dd7c60c0', dir: `${M}/Qwen-Qwen3.8-27B/1d4bf0f2ff6012fd82039f2fa52739d0dd7c60c0`, files: Array.from({ length: 18 }, (_, i) => `model-${String(i + 1).padStart(5, '0')}-of-00018.safetensors`) },
];

const out = { generated: new Date().toISOString(), sets: [] };
for (const set of SETS) {
  const meta = JSON.parse(execFileSync('curl', ['-sL', `https://huggingface.co/api/models/${set.repo}/revision/${set.rev}?blobs=true`], { encoding: 'utf8', maxBuffer: 1 << 26 }));
  const pub = new Map(meta.siblings.map((s) => [s.rfilename, s.lfs?.sha256 ?? null]));
  const files = [];
  for (const f of set.files) {
    const p = path.join(set.dir, f);
    if (!fs.existsSync(p)) { files.push({ file: f, path: p, present: false }); continue; }
    const size = fs.statSync(p).size;
    const sha = execFileSync('shasum', ['-a', '256', p], { encoding: 'utf8', maxBuffer: 1 << 20 }).split(/\s+/)[0];
    const published = pub.get(f) ?? null;
    files.push({ file: f, path: p, present: true, size, sha256: sha, published_sha256: published, match: published ? published === sha : null });
    console.log(`${published === sha ? 'OK ' : 'BAD'} ${set.repo}/${f} ${sha}`);
  }
  out.sets.push({ ...set, files });
}
// converted artifact (no published hash: produced here by `lumen convert`)
const lbc = path.join(M, 'lumen-lbc-v0.48.1/qwen3.8-27b-q4_0.lbc');
if (fs.existsSync(lbc)) out.converted = [{ use: 'Lumen .lbc converted from Qwen3.8-27B-Q4_0.gguf by lumen v0.48.1', path: lbc, size: fs.statSync(lbc).size, sha256: execFileSync('shasum', ['-a', '256', lbc], { encoding: 'utf8' }).split(/\s+/)[0] }];
fs.mkdirSync(path.join(ENGINES_DIR, 'results'), { recursive: true });
fs.writeFileSync(path.join(ENGINES_DIR, 'results', 'models.json'), JSON.stringify(out, null, 1));
