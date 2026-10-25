# model-bonsai: the CortexCode model plugin (`model@1`) on PrismML's llama.cpp

`cc-model-bonsai` serves the `model` category over CPP/1 with **Ternary Bonsai 2 27B** (PrismML's
ternary Qwen3.8-27B: weights in {-1, 0, +1} with FP16 group scales, Hadamard-rotated) on PrismML's
fork of llama.cpp, embedded **in-process** with Metal. `backend/vendor/llama.cpp` holds the fork's
sources at `0082ac24a89ae62132041b40e2c0170e278a98f4` (branch `prism`, see `REVISION`; patches in
`PATCHES.md`). Rust calls it through a flat C shim (`backend/native/shim.cpp`), so no llama.cpp
struct layout is copied into Rust. Swap policy: drain first.

It is the model engine for Macs with 24 GB of memory or less: `[backend.machine]
max_memory_bytes` in `plugin.toml`. When the person has not chosen an engine, the frame runs the
model plugin whose declared range holds the machine's memory (`model-uzu` declares the rest).
Settings > Model shows the engines and switches between them (`frame.plugins.switch`). The frame
drains the serving engine, starts the other in the same slot, and serves the other's panels. A
failed start brings the previous engine back. The plan and the measurements behind it are in
`docs/BONSAI-ENGINE-PLAN.md`.

## Stage

This is the first stage (phase 1 of the plan). The plugin installs, verifies, fits and loads the
model, checks the tokenizer, and answers `model.info`, `model.variants`, `model.select`,
`model.tokenize`, `model.count`, `model.slots` and `model.hold`. Its probe generates a short
answer.

`model.chat`, `model.complete`, `model.prewarm`, `model.hibernate` and `model.endpoint` answer a
plain (not retryable) `unavailable` error until the engine's scheduler lands (phase 2).

## Building

`cargo build --release` in `backend/` runs CMake on `backend/native` (the vendored llama.cpp with
Metal, its multimodal library `mtmd`, and the shim, all static) and produces
`backend/target/release/cc-model-bonsai`. The Metal kernels are embedded as source and compiled
for the running GPU at load, so the Command Line Tools and CMake are enough: no Xcode Metal
compiler. `npm install && npm run build` in `ui/` builds the card (`model-bonsai.card-model`) and
the settings panel (`model-bonsai.settings-model`).

## Variant (`model.variants`, `model.select`)

| id | file | notes |
|---|---|---|
| `bonsai2-27b-pq2` | `prism-ml/Ternary-Bonsai-2-27B-gguf@b072e1d`, `…-PQ2_0.gguf`, 7.21 GB | ternary weights in 2-bit slots; the only variant (owner's choice) |

PTQ1_0 (the same trits densely packed, 5.95 GB) gave the same greedy output 8-15% slower, and the
MLX pack on rMLX had no prefix reuse, grammar or vision (`docs/BONSAI-ENGINE-PLAN.md` §11): neither
is offered. The variant also takes `tokenizer.json` from `Qwen/Qwen3.8-27B@1d4bf0f`: the GGUF vocab
tokenizes identically, and the load refuses the model if it does not on a fixed set of probes
(chat and tool-call markup, code, several scripts, whitespace, escapes, image tokens). Files are
installed under `<models_dir>/<org>-<repo>/<revision>/` (default `$CORTEX_HOME/models`). A missing
file is downloaded from Hugging Face at the pinned revision (resumable `.part` files) and checked
against the published digest; records in `<models_dir>/.cortexcode-verified.json` make later
checks instant. Nothing is downloaded at startup for a variant that is not installed.
`cc-model-bonsai install bonsai2-27b-pq2` installs without a frame.

## Configuration (`[plugin.config]`)

| key | default | meaning |
|---|---|---|
| `variant` | `bonsai2-27b-pq2` | catalog id |
| `models_dir` | `$CORTEX_HOME/models` | where variants live |
| `memory_limit_bytes`, `min_context_per_slot` (32768), `ctx_total` | measured | operator limits and overrides |

## Memory fit (`backend/src/fit.rs`)

This is the light engine. Everything it holds stays under **12 GB** on every Mac
(`MAX_TOTAL_BYTES`), and under the machine's budget (min(Metal `recommendedMaxWorkingSetSize`,
physical RAM less a quarter), uzu's rule) on smaller ones.

llama.cpp allocates everything at load and clears it: the weights, one unified KV pool shared by
every sequence (any one may grow to all of it), one recurrent state per sequence (156.9 MB on the
27B), and the compute buffers. So a pool planned up to the budget is wired in full whether used or
not. On a 48 GB Mac that rule held 22 GB, so the pool is sized under the ceiling instead. Context
checkpoints (copies of a recurrent state) live in host memory, which on a Mac is the same memory,
so they count too.

| part | plan |
|---|---|
| weights (PQ2_0) | 7.21 GB |
| compute buffers | 0.5 GiB reserve, at a 256-token micro-batch (prefill as fast as 512: 358 vs 364 tok/s) |
| recurrent states | 3 resident conversations × 156.9 MB |
| checkpoints | 2 per conversation × 156.9 MB, filled as used |
| KV pool | q8_0 (34 KiB per token: 16 attention layers × 4 KV heads × 512 values), what is left under 12 GB, at most 131,072 tokens: about 80,896 tokens |

On a smaller budget the plan gives up resident conversations before the pool drops below 65,536
tokens. A machine that cannot hold one 32,768-token conversation is a plain load error.
`model.info.extra.fit` has the plan, the geometry and what the load really took on the GPU.

## Images

The variants carry PrismML's vision projector (`…-mmproj-Q8_0.gguf`, 0.63 GB), which llama.cpp's
`mtmd` runs. Image input lands with phase 5: until then `model.info` advertises text only.
