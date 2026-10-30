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

Settings > Model shows it inside the serving engine's card (the weights belong to the engine that
loads them): one line for a single variant, a list with Select for several.

| id | file | notes |
|---|---|---|
| `bonsai2-27b-pq2-mtp` | `ProCreations/Ternary-Bonsai-2-27B-MTP@efffdea`, `…-PQ2_0-MTP-Q8_0.gguf`, 7.66 GB | the default (owner's choice): PrismML's PQ2_0 file unchanged plus Qwen3.8's MTP layer, trained further on Bonsai 2's output; the engine drafts with it |
| `bonsai2-27b-pq2` | `prism-ml/Ternary-Bonsai-2-27B-gguf@b072e1d`, `…-PQ2_0.gguf`, 7.21 GB | ternary weights in 2-bit slots, no drafts |

## Drafts (MTP)

With the MTP variant, each generation step feeds the token being handed out together with the MTP
head's guess of the token after it (`fit::DRAFT_TOKENS` = 1). The token sampled from the first row
is the real next token; when it equals the guess, the second row gives the token after it, so the
step yields two tokens; else the guess is removed through a recurrent-state snapshot (`n_rs_seq`).
Sampling is unchanged (the grammar, penalties and seed see exactly the tokens plain decoding
would), so the output is the same. A second context holds the MTP layer's own cache; every row the
model reads is passed to it with the model's hidden state at the position before. One draft, not
two or three: without the M5's tensor units, checking two tokens costs 1.42x one and three 1.91x
(measured on the 27B with the tensor units off: code 24 -> 33 tok/s with one draft, 30 with two,
28 with three; prose about even). `usage.draft_total` / `draft_accepted` report each request.

PTQ1_0 (the same trits densely packed, 5.95 GB) gave the same greedy output 8-15% slower, and the
MLX pack on rMLX had no prefix reuse, grammar or vision (`docs/BONSAI-ENGINE-PLAN.md` §11): neither
is offered. The variant also takes `tokenizer.json` from `Qwen/Qwen3.8-27B@1d4bf0f`: the GGUF vocab
tokenizes identically, and the load refuses the model if it does not on a fixed set of probes
(chat and tool-call markup, code, several scripts, whitespace, escapes, image tokens). Files are
installed under `<models_dir>/<org>-<repo>/<revision>/` (default `$CORTEX_HOME/models`). A missing
file is downloaded from Hugging Face at the pinned revision (resumable `.part` files) and checked
against the published digest; digest records in the memory plugin's graph
(`(:ModelFile)-[:ON]->(:Machine)`, shared with the uzu plugin) make later checks instant. Nothing is downloaded at startup for a variant that is not installed.
`cc-model-bonsai install bonsai2-27b-pq2-mtp` installs without a frame.

## Configuration (`[plugin.config]`)

| key | default | meaning |
|---|---|---|
| `variant` | `bonsai2-27b-pq2-mtp` | catalog id |
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
