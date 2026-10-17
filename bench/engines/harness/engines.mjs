// Engine adapters: how to launch each engine configuration and how to phrase a request so the
// prompt is the same token sequence everywhere (Qwen3.8 chat template, thinking disabled, greedy).
import path from 'node:path';
import { ENGINES_DIR, MODELS, flattenMessages } from './lib.mjs';

export const W = {
  gguf_ud_q4km: path.join(MODELS, 'unsloth-Qwen3.8-27B-GGUF/4ca720788d1e01f1bff70c033e0d0028fd02e502/Qwen3.8-27B-UD-Q4_K_M.gguf'),
  gguf_q8_0: path.join(MODELS, 'unsloth-Qwen3.8-27B-GGUF-Q8_0/4ca720788d1e01f1bff70c033e0d0028fd02e502/Qwen3.8-27B-Q8_0.gguf'),
  gguf_q4_0: path.join(MODELS, 'unsloth-Qwen3.8-27B-GGUF-Q4_0/4ca720788d1e01f1bff70c033e0d0028fd02e502/Qwen3.8-27B-Q4_0.gguf'),
  dflash_q8_0: path.join(MODELS, 'ggml-org-Qwen3.8-27B-GGUF-dflash/71bc7b627595dc8a91039addd9c791ae548d6747/dflash-Qwen3.8-27B-Q8_0.gguf'),
  dflash_q4_0: path.join(MODELS, 'ggml-org-Qwen3.8-27B-GGUF-dflash/71bc7b627595dc8a91039addd9c791ae548d6747/dflash-Qwen3.8-27B-Q4_0.gguf'),
  mlx_4bit: path.join(MODELS, 'mlx-community-Qwen3.8-27B-4bit/10c35caafbb80f7dc6a7a432cdd11af10a6d4818'),
  mlx_mtp_4bit: path.join(MODELS, 'mlx-community-Qwen3.8-27B-MTP-4bit/b643c01b6d3b094e325edb6ebd832e16c486c575'),
  bf16_dir: path.join(MODELS, 'Qwen-Qwen3.8-27B/1d4bf0f2ff6012fd82039f2fa52739d0dd7c60c0'),
  view_mlx4_mtp: path.join(ENGINES_DIR, 'views/qwen38-27b-mlx4-mtp'),
  small_mlx4: path.join(MODELS, 'mlx-community-Qwen3.5-4B-4bit/0e7ffd5c629ef7719d4cbc04069232580bfa9d9c'),
  small_q4km: path.join(MODELS, 'unsloth-Qwen3.5-4B-GGUF/e87f176479d0855a907a41277aca2f8ee7a09523/Qwen3.5-4B-Q4_K_M.gguf'),
};

// ------------------------------------------------------------------ llama.cpp (baseline)
const LLAMA_BIN = path.join(ENGINES_DIR, 'llamacpp/build/bin/llama-server');
function llamaArgs(c, port, modelOverride) {
  const a = ['-m', modelOverride || c.model || W.gguf_ud_q4km, '--host', '127.0.0.1', '--port', String(port), '-ngl', '99',
    '-c', String(c.ctx ?? 196608), '-np', String(c.np ?? 4), '-fa', c.fa ?? 'on', '-b', String(c.b ?? 2048), '-ub', String(c.ub ?? 512),
    '-ctk', c.kv ?? 'f16', '-ctv', c.kv ?? 'f16', '--jinja', '--no-webui', '--metrics', '--fit', 'off'];
  if (c.kvu !== false) a.push('-kvu'); else a.push('-no-kvu');
  if (c.threads) a.push('-t', String(c.threads), '-tb', String(c.threads));
  if (c.mmap === false) a.push('--no-mmap');
  if (c.mlock) a.push('--mlock');
  if (c.cacheRam !== undefined) a.push('--cache-ram', String(c.cacheRam));
  if (c.checkpoints !== undefined) a.push('--ctx-checkpoints', String(c.checkpoints));
  if (c.spec) {
    a.push('--spec-type', c.spec);
    if (c.draft) a.push('-md', c.draft, '-ngld', '99');
    if (c.nmax) a.push('--spec-draft-n-max', String(c.nmax));
    if (c.nmin !== undefined) a.push('--spec-draft-n-min', String(c.nmin));
  }
  if (c.extra) a.push(...c.extra);
  return a;
}
const llamaConfigs = {
  // the model plugin's shape: unified KV pool, 4 slots, flash attention, f16 KV
  default: {},
  'ub1024': { ub: 1024 }, 'ub2048': { ub: 2048, b: 2048 }, 'ub4096': { ub: 4096, b: 4096 },
  'fa-off': { fa: 'off' },
  'kv-q8': { kv: 'q8_0' },
  'np1': { np: 1, ctx: 65536 },
  't4': { threads: 4 }, 't12': { threads: 12 },
  'no-mmap': { mmap: false }, 'mlock': { mlock: true },
  'mtp1': { spec: 'draft-mtp', nmax: 1 }, 'mtp2': { spec: 'draft-mtp', nmax: 2 }, 'mtp3': { spec: 'draft-mtp', nmax: 3 }, 'mtp4': { spec: 'draft-mtp', nmax: 4 },
  'dflash-q8': { spec: 'draft-dflash', draft: W.dflash_q8_0 }, 'dflash-q4': { spec: 'draft-dflash', draft: W.dflash_q4_0 },
  'ngram-mod': { spec: 'ngram-mod' }, 'ngram-simple': { spec: 'ngram-simple' },
  // prompt-cache / context-checkpoint knobs, judged on the multi-turn agent loop
  'agent-default': { sweepAgent: true }, 'agent-ckpt8': { sweepAgent: true, checkpoints: 8 }, 'agent-ckpt128': { sweepAgent: true, checkpoints: 128 },
  'agent-cram0': { sweepAgent: true, cacheRam: 0 }, 'agent-cram32k': { sweepAgent: true, cacheRam: 32768 },
  // quality reference (higher precision weights, same engine)
  'ref-q8': { model: W.gguf_q8_0, ctx: 32768, np: 1 },
};

// ------------------------------------------------------------------ Saragossa (pure Rust, metal-rs)
const SARAGOSSA_BIN = path.join(ENGINES_DIR, 'saragossa/src/target/release/saragossa');
const SARAGOSSA_KEY = 'bench-local-key';
const saragossaConfigs = {
  // autoregressive on the plain MLX 4-bit checkpoint (no MTP sidecar present)
  default: { dir: 'mlx' },
  'explore-fused': { dir: 'mlx', env: { SARAGOSSA_RUST_OPT_PROFILE: 'explore-fused' } },
  'ssm-bf16': { dir: 'mlx', env: { SARAGOSSA_RUST_LINEAR_SSM_BF16: '1' } },
  'kv-bf16': { dir: 'mlx', env: { SARAGOSSA_RUST_KV_BF16: '1' } },
  'tg-rows-8': { dir: 'mlx', env: { SARAGOSSA_RUST_LINEAR_DELTA_TG_ROWS: '8' } },
  // MTP head from mlx-community/Qwen3.8-27B-MTP-4bit as the sidecar
  mtp1: { dir: 'mtp', env: { SARAGOSSA_RUST_MTP_MAX_DRAFT: '1' } },
  mtp2: { dir: 'mtp', env: { SARAGOSSA_RUST_MTP_MAX_DRAFT: '2' } },
  mtp3: { dir: 'mtp', env: { SARAGOSSA_RUST_MTP_MAX_DRAFT: '3' } },
  'mtp-adaptive': { dir: 'mtp', env: { SARAGOSSA_RUST_MTP_MAX_DRAFT: '3', SARAGOSSA_RUST_MTP_ADAPTIVE_DEPTH: '1' } },
};

// ------------------------------------------------------------------ mistral.rs
const MISTRALRS_BIN = path.join(ENGINES_DIR, 'mistralrs/src/target/release/mistralrs');
function mistralArgs(c, port, o) {
  const a = ['serve', '-p', String(port), '--host', '127.0.0.1', '--no-ui', '--disable-access-log'];
  a.push('--max-seqs', String(c.maxSeqs ?? 8), '--prefix-cache-n', String(c.prefixN ?? 16));
  if (c.paged) { a.push('--paged-attn', 'on'); if (c.paCtx) a.push('--pa-context-len', String(c.paCtx)); if (c.paType) a.push('--pa-cache-type', c.paType); if (c.paBlock) a.push('--pa-block-size', String(c.paBlock)); }
  else a.push('--paged-attn', 'off');
  if (c.mtp) { a.push('--mtp'); if (c.mtpN) a.push('--mtp-n-predict', String(c.mtpN)); }
  if (c.maxSeqLen) a.push('--max-seq-len', String(c.maxSeqLen));
  if (c.maxBatch) a.push('--max-batch-size', String(c.maxBatch));
  const src = o.modelOverride || c.src;
  if (src.endsWith('.gguf')) a.push('-f', src);
  else a.push('-m', src);
  if (c.isq) a.push('--isq', c.isq);
  return a;
}
const mistralConfigs = {
  // (MLX checkpoints do not load: conv1d weight layout [C,K,1] vs expected [C,1,K], see the smoke log)
  // plain Q4_0 GGUF (the UD-Q4_K_M file carries IQ4_XS/IQ4_NL/IQ3_S tensors mistral.rs refuses)
  'gguf-q4_0': { src: W.gguf_q4_0 },
  'gguf-q4_0-paged': { src: W.gguf_q4_0, paged: true, paCtx: 196608 },
  // official BF16 safetensors quantized in situ to AFQ4 (MLX-style affine 4-bit) at load; the
  // only mistral.rs path with the built-in MTP head (needs paged attention)
  'isq-afq4': { src: W.bf16_dir, isq: 'afq4' },
  'isq-afq4-paged': { src: W.bf16_dir, isq: 'afq4', paged: true, paCtx: 196608 },
  'isq-afq4-mtp': { src: W.bf16_dir, isq: 'afq4', paged: true, paCtx: 196608, mtp: true },
  'isq-afq4-mtp2': { src: W.bf16_dir, isq: 'afq4', paged: true, paCtx: 196608, mtp: true, mtpN: 2 },
  'isq-afq4-mtp3': { src: W.bf16_dir, isq: 'afq4', paged: true, paCtx: 196608, mtp: true, mtpN: 3 },
  'isq-q4k': { src: W.bf16_dir, isq: 'q4k' },
};

// ------------------------------------------------------------------ rMLX (Rust server on mlx-c)
const RMLX_DIR = path.join(ENGINES_DIR, 'rmlx');
const RMLX_ENV = { DYLD_LIBRARY_PATH: `${RMLX_DIR}/deps/mlx-c/lib:${RMLX_DIR}/deps/mlx-wheel/mlx/lib` };
const rmlxConfigs = {
  default: {},
  'kv8': { args: ['--kv-bits', '8', '--kv-group-size', '64'] },
  'prefill-chunk-4096': { env: { RMLX_PREFILL_CHUNK: '4096', RMLX_PREFILL_CHUNK_QWEN3_5_MOE: '4096' } },
  'prefill-chunk-1024': { env: { RMLX_PREFILL_CHUNK: '1024', RMLX_PREFILL_CHUNK_QWEN3_5_MOE: '1024' } },
  mtp1: { mtp: 1 }, mtp2: { mtp: 2 }, mtp3: { mtp: 3 },
};

// ------------------------------------------------------------------ Lumen (faisalmumtaz89/Lumen)
const LUMEN_BIN = path.join(ENGINES_DIR, 'lumen/src/target/release/lumen-server');
const LUMEN_LBC = path.join(MODELS, 'lumen-lbc-v0.48.1/qwen3.8-27b-q4_0.lbc');
const lumenConfigs = {
  // repetition penalty off: the engine's default 1.05 would change greedy output (documented knob)
  default: {},
  'gpu-sampler': { env: { LUMEN_METAL_GPU_SAMPLER: '1' } },
  'unretained-cb': { env: { LUMEN_METAL_UNRETAINED_CMDBUFS: '1' } },
  'sync-weights': { args: ['--sync'] },
  'kv-f32': { args: ['--kv-precision', 'f32'] },
};

// ------------------------------------------------------------------ xInfer (guoqingbao; release binary, see survey)
const XINFER_BIN = path.join(ENGINES_DIR, 'xinfer/release-0.14.5/xinfer');
const xinferConfigs = {
  // the UD-Q4_K_M GGUF does not load (IQ4_NL tensor in blk.21 attn_qkv: "IQ4_NL CPU dequantization
  // is not implemented", raw/xinfer/default/sweep-2026-10-15T00-32-42-208Z), so the plain Q4_0 GGUF
  default: {},
  'gguf-q4_0': { model: W.gguf_q4_0, maxLen: 49152 },
  'seqs8': { seqs: 8 },
  'reasoning-flag-off': { disableReasoning: false },
  'kv-fp8': { args: ['--kvcache-dtype', 'fp8'] },
  'chunk-2048': { args: ['--prefill-chunk-size', '2048'] },
  'chunk-8192': { args: ['--prefill-chunk-size', '8192'] },
  mtp1: { args: ['--num-speculative-tokens', '1'] }, mtp2: { args: ['--num-speculative-tokens', '2'] }, mtp3: { args: ['--num-speculative-tokens', '3'] },
};

// ------------------------------------------------------------------ Crane (candle-based)
const CRANE_BIN = path.join(ENGINES_DIR, 'crane/src/target/release/crane-serve');
const craneConfigs = {
  default: {},
  'kv-int8': { env: { CRANE_KV_QUANT: 'int8' } },
  'dtpseq-64': { args: ['--decode-tokens-per-seq', '64'] },
  'prefill-chunk-4096': { env: { CRANE_PREFILL_CHUNK: '4096' } },
};

// ------------------------------------------------------------------ uzu (trymirai; release CLI, see survey)
const UZU_BIN = path.join(ENGINES_DIR, 'uzu/release-0.6.7/mirai-aarch64-apple-darwin');
// served through uzu's registry id (a local directory path loses the registry's chat encoding);
// its cache ~/.cache/mirai/models/mirai/alibaba-qwen3.8-27b-mirai-mirai-m-4/8b878f5... holds hardlinks
// of the verified download in ~/.cortexcode/models/trymirai-Qwen3.8-27B-M/8b878f5...
const UZU_M = 'trymirai/Qwen3.8-27B-M';
const UZU_S = path.join(MODELS, 'trymirai-Qwen3.8-27B-S-experimental/32d4c0422a14353808659f2f2333b207bc1dd12a');
const UZU_S_BIN = path.join(UZU_S, 'uzu/uzu');
const uzuConfigs = {
  // uzu's own 4-bit format (Mirai-M, comparable to UD-Q4_K_M per its card); the bundled
  // speculator is used automatically when present in the model directory
  default: {},
  'no-prefix-cache': { args: ['--no-prefix-cache'] },
  // Mirai S (2.4-bit, experimental) on the prebuilt runtime its HF repo ships (uzu branch
  // ryan/qtip-s-agent @ e743055a), and Mirai M on that same runtime for comparison
  S: { bin: 'S', model: UZU_S },
  'M-on-S-runtime': { bin: 'S' },
  // speculator disabled (lossless check and non-speculative speed): see harness/uzu-nospec.sh
  'no-spec': { noSpec: true },
};

// ------------------------------------------------------------------ lumen-rs (rabbitson87; v0.11.1 release sidecar, see survey)
const LUMENRS_BIN = path.join(ENGINES_DIR, 'lumen-rs/release-0.11.1/Lumen.app/Contents/MacOS/lumen-server');
const lumenRsConfigs = {
  default: {},
  'batch-decode': { env: { LUMEN_MLX_BATCH_DECODE: '1' } },
  'server-mode': { env: { LUMEN_MLX_SERVER_MODE: '1' } },
  'kv-q8': { env: { LUMEN_QWEN35_QUANT_KV: '1' } },
};

export const ENGINES = {
  'lumen-rs': {
    role: 'candidate',
    concurrency: true,
    configs: lumenRsConfigs,
    launch(c, port, o = {}) {
      return { cmd: LUMENRS_BIN, args: [], env: { MODEL_ID: o.modelOverride || W.mlx_4bit, PORT: String(port), HOST: '127.0.0.1', ...(c.env || {}) }, modelName: 'default' };
    },
    body(b) { return { ...b, chat_template_kwargs: { enable_thinking: false }, enable_thinking: false }; },
  },

  uzu: {
    role: 'candidate',
    concurrency: true,
    configs: uzuConfigs,
    launch(c, port, o = {}) {
      const bin = c.bin === 'S' ? UZU_S_BIN : UZU_BIN;
      const args = ['server', '--model', o.modelOverride || c.model || UZU_M, '--host', '127.0.0.1', '--port', String(port), ...(c.args || [])];
      if (c.noSpec) return { cmd: path.join(ENGINES_DIR, 'harness/uzu-nospec.sh'), args: [bin, ...args], env: {}, modelName: 'default' };
      return { cmd: bin, args, env: {}, modelName: 'default' };
    },
    body(b) { return { ...b, enable_thinking: false }; },
  },

  crane: {
    role: 'candidate',
    concurrency: true,
    configs: craneConfigs,
    launch(c, port, o = {}) {
      return {
        cmd: CRANE_BIN, args: ['-m', o.modelOverride || W.gguf_ud_q4km, '--host', '127.0.0.1', '-p', String(port), '-c', String(c.conc ?? 4), '--context', c.context ?? '64K', ...(c.args || [])],
        env: { ...(c.env || {}) }, modelName: 'default',
      };
    },
    body(b) { return { ...b, chat_template_kwargs: { enable_thinking: false }, enable_thinking: false }; },
  },

  xinfer: {
    role: 'candidate',
    concurrency: true,
    configs: xinferConfigs,
    launch(c, port, o = {}) {
      return {
        cmd: XINFER_BIN, args: ['--m', o.modelOverride || c.model || W.gguf_ud_q4km, '--server', `127.0.0.1:${port}`, '--max-num-seqs', String(c.seqs ?? 4), '--max-model-len', String(c.maxLen ?? 65536), ...(c.disableReasoning === false ? [] : ['--disable-reasoning']), ...(c.args || [])],
        env: {}, modelName: 'default',
      };
    },
    body(b) { return { ...b, enable_thinking: false }; },
  },

  lumen: {
    role: 'candidate',
    concurrency: true, // one engine worker runs one job at a time; concurrent clients queue (measured as such)
    configs: lumenConfigs,
    launch(c, port, o = {}) {
      return {
        cmd: LUMEN_BIN, args: ['--model', o.modelOverride || LUMEN_LBC, '--host', '127.0.0.1', '--port', String(port), '--context-len', String(c.ctx ?? 65536), '--backend', 'metal', '--inbox-size', '64', ...(c.args || [])],
        env: { LUMEN_REPETITION_PENALTY: '1.0', ...(c.env || {}) }, modelName: 'qwen3.8-27b',
      };
    },
    body(b) { return { ...b, enable_thinking: false, chat_template_kwargs: { enable_thinking: false } }; },
  },

  rmlx: {
    role: 'candidate',
    concurrency: true, // one GPU permit, FIFO: concurrent clients queue (measured as such)
    configs: rmlxConfigs,
    launch(c, port, o = {}) {
      const a = ['serve', '--model', o.modelOverride || W.mlx_4bit, '--host', '127.0.0.1', '--port', String(port), '--max-ctx', String(c.maxCtx ?? 65536),
        '--enable-thinking', 'false', '--metrics', 'off', '--max-tokens-cap', '65536', '--max-queue-depth', '64', ...(c.args || [])];
      if (c.mtp) a.push('--draft-model', W.mlx_mtp_4bit, '--draft-kind', 'mtp', '--draft-block-size', String(c.mtp + 1));
      return { cmd: path.join(RMLX_DIR, 'src/target/release/rmlx'), args: a, env: { ...RMLX_ENV, ...(c.env || {}) }, modelName: path.basename(o.modelOverride || W.mlx_4bit) };
    },
    body(b) { return { ...b, chat_template_kwargs: { enable_thinking: false } }; },
  },

  mistralrs: {
    role: 'candidate',
    concurrency: true,
    configs: mistralConfigs,
    launch(c, port, o = {}) { return { cmd: MISTRALRS_BIN, args: mistralArgs(c, port, o), env: { MISTRALRS_METAL_PRECOMPILE: '0' }, modelName: 'default' }; },
    body(b) { return { ...b, enable_thinking: false }; },
  },

  saragossa: {
    role: 'candidate',
    concurrency: true, // the server handles one request at a time; concurrent clients queue (measured as such)
    flatten: true,
    configs: saragossaConfigs,
    launch(c, port, o = {}) {
      const dir = o.modelOverride || (c.dir === 'mtp' ? W.view_mlx4_mtp : W.mlx_4bit);
      return {
        cmd: SARAGOSSA_BIN, args: ['serve', '--port', String(port), '--api-key', SARAGOSSA_KEY, '--model', `q38=${dir}`, '--backend', 'metal', '--preload', '--read-timeout-secs', '600'],
        env: { ...(c.env || {}) }, headers: { authorization: `Bearer ${SARAGOSSA_KEY}` }, modelName: 'q38',
      };
    },
    body(b) { return { ...b, messages: flattenMessages(b.messages, b.tools), tools: undefined }; },
  },

  llamacpp: {
    role: 'baseline',
    concurrency: true,
    configs: llamaConfigs,
    launch(c, port, o = {}) { return { cmd: LLAMA_BIN, args: llamaArgs(c, port, o.modelOverride), env: {}, modelName: 'qwen3.8-27b' }; },
    body(b) { return { ...b, chat_template_kwargs: { enable_thinking: false } }; },
  },
};

/** Merge the knob sets of composed configs (used when the best values from separate sweeps combine). */
export function compose(...cs) { return Object.assign({}, ...cs); }
