# model-uzu: the CortexCode model plugin (`model@1`) on uzu

`cc-model-uzu` serves the `model` category over CPP/1 with Mirai's [uzu](https://github.com/trymirai/uzu)
engine (Rust, Metal, the M5 matrix units, speculative decoding with each model's bundled
speculator) embedded **in-process** as a library: `backend/vendor/uzu` holds the engine crates at
`391abc29abb26f18aa812f6f02e8b023aed11364` (release 0.6.7, see `backend/vendor/uzu/REVISION`) with the
patches in `backend/vendor/uzu/PATCHES.md`. There is no HTTP server inside and no subprocess. Swap policy:
drain first. The plugin is one self-contained folder (`plugin.toml`; the Rust
workspace in `backend/`, the card and settings panels in `ui/`); its only
outside dependencies are `cc-protocol` (`../../../protocol` from `backend/`) and `@cortex/sdk`.

## Building

uzu compiles its Metal kernels at build time with Xcode's Metal toolchain (`xcrun -sdk macosx metal`;
the Command Line Tools have no Metal compiler). `backend/.cargo/config.toml` points `DEVELOPER_DIR` at
`/Applications/Xcode.app` for this plugin only (the global `xcode-select` is untouched);
`backend/rust-toolchain.toml` pins Rust 1.99.0 (uzu needs 1.98+; the repository pins 1.94.1). `SDKROOT`
stays the repository's pin. `cargo build --release` in `backend/` produces
`backend/target/release/cc-model-uzu`; `npm install && npm run build` in `ui/` builds the card
(`model-uzu.card-model`, `card.model@1`) and the settings panel (`model-uzu.settings-model`,
`settings.model@1`).
`--no-default-features --features cpu` builds uzu's reference CPU backend instead (no Metal toolchain
needed; far too slow for use, a development aid only).

## Variants (`model.variants`, `model.select`)

Settings > Model shows them inside the serving engine's card (the weights belong to the engine
that loads them): one line when the engine offers one variant, a list with Select or Download when
it offers several. Below, the loaded model's facts (`model.info`).

| id | repository @ revision | notes |
|---|---|---|
| `qwen3.8-27b-m` (default) | `trymirai/Qwen3.8-27B-M@8b878f5` | Mirai M 4-bit, DFlash speculator |
| `qwen3.5-4b-m` (tests) | `trymirai/Qwen3.5-4B-M@c04a485` | no speculator; not listed by `model.variants` unless it is the running variant |

Installed under `<models_dir>/<org>-<repo>/<revision>/` (default `$CORTEX_HOME/models`). A missing
file is downloaded from Hugging Face at the pinned revision (resumable `.part` files) and every file
is checked against the digest Hugging Face publishes (SHA-256 `lfs.oid` for weights, the git blob
SHA-1 for small files); digest records in the memory plugin's graph (`(:ModelFile)-[:ON]->(:Machine)`,
valid for the file's size and modification time) make later checks instant. Nothing is ever downloaded at startup for a variant that is not installed; afterwards
everything runs offline. `cc-model-uzu install <variant> [--models-dir DIR]` installs without a frame.

The 27B variant also has a vision tower (image input, see [Images](#images-vendor-patch-p7)): only
its tensors are fetched from the upstream checkpoint into `<variant dir>/vision/` in the same step
that downloads the model (a first start, `model.select`, `cc-model-uzu install <variant>`), and by
`cc-model-uzu install-vision [variant]` for a model installed without it (progress as
`model.install` events; a failed fetch leaves the model text-only and does not stop the load).

`trymirai/Qwen3.8-27B-S-experimental` is not offered: its 2.4-bit QTIP codec runs only on Mirai's
`ryan/qtip-s-agent` branch (`e743055a`), which diverged from main 159 commits earlier with a
different crate layout (`crates/backend-uzu`) and engine API, so it cannot share this process's engine;
Mirai ships it only as its own prebuilt server binary.

## Configuration (`[plugin.config]`)

| key | default | meaning |
|---|---|---|
| `variant` | `qwen3.8-27b-m` | catalog id |
| `models_dir` | `$CORTEX_HOME/models` | where variants live |
| `speculative` | `auto` | `auto` (draft with the model's speculator when it has one) or `off` |
| `slots` | none | operator cap on resident conversations; by default as many are resident as the shared pool holds |
| `memory_limit_bytes`, `min_context_per_slot` (32768), `ctx_total`, `prefill_chunk` (1024) | measured | operator limits and overrides |
| `vision` | `auto` | `auto`: image input with the variant's vision tower when installed (`model.select` installs it); `off`: text only, the tower is never installed or loaded |
| `idle_spill_s` | 30 | seconds a conversation stays resident after its last request before it is spilled to disk (0: kept until the pool needs the room) |
| `hibernate_limit_bytes` | 32 GiB | disk bound of the spilled conversations; the oldest beyond it are deleted (that conversation starts cold) |
| `vision_idle_unload_s` | 300 | seconds without an image after which the vision tower (and its scratch, and the image-embedding cache) is unloaded |
| `max_image_pixels` | 2400000 | pixel budget per image; larger images are downscaled (aspect kept) to at most this many pixels, about 2,340 tokens |

## How it runs

- **Memory fit, once per load** (`backend/src/fit.rs`): budget = min(Metal `recommendedMaxWorkingSetSize`,
  physical RAM less a quarter): the machine itself, never what happens to be free. Metal memory is
  wired (macOS cannot page or compress it, and raises no memory pressure for it), so the budget is a
  hard ceiling; with nothing held back the 27B filled the whole working set (41.6 GB of 48 GB) and
  the GPU driver panicked the machine. The pool is everything the weights and a
  compute reserve leave, read from the model's `config.json` (27B: 64 KiB of attention KV per token
  plus 20 KiB for the speculator's context). uzu keeps KV in sparse Metal buffers committed as a
  conversation grows, so the pool is shared: any one conversation may grow to all of it, up to the
  native context. No conversation count is planned: a conversation is made resident while the pool
  has room for its fixed state (its recurrent state, 2 to 8 context checkpoints, at most a
  sixteenth of the room each, and its suffix rows; 27B: 157 MB per copy) beside the contexts held,
  else the least recently used idle one is spilled to disk, else the request queues. A
  conversation is resident only while it works: idle for `idle_spill_s` (no request running or
  waiting for it: its turn ended), it is spilled to disk, and its next request restores it without
  re-prefill, so memory holds the weights and the conversations being worked on. `model.info`
  reports `slots` as the upper bound (each conversation with no context). Running requests are
  interleaved one forward pass at a time (concurrency, not parallel decoding). A machine whose pool
  cannot hold one conversation with 32k tokens is a plain load error.
- **Templating and parsing** (`backend/src/template.rs`, `backend/src/parse.rs`): prompts are rendered with the
  Qwen3.8 template (`templates/`) and tokenized with the model's tokenizer; output tokens are
  detokenized incrementally and split structurally into reasoning, content and native tool calls
  (`<tool_call><function=…><parameter=…>`). Tool arguments stream as JSON text built from the
  parameters (string parameters per the tool schema are streamed; others are emitted as JSON when
  they parse); a call cut off by a token limit keeps its raw, unterminated text.
- **Per-conversation cache** (`backend/src/engine.rs`): every `cache_key` (session or agent id; derived from
  the conversation start when absent) has its own uzu state and context checkpoints of it (a hybrid
  model's recurrent state cannot be truncated, only rolled back). When the runtime renders a
  request it finds its reuse points: the end of the leading system block and the end of the history
  before the last message, each as the tokens two renders share where the prefix is continued by a
  user and by an assistant message (mechanical, from the template). The prefill checkpoints them
  and the point before the last prompt token. A request continues its state when the state is a
  prefix of the new prompt, else rolls back to the longest checkpoint that is, else starts cold;
  `cached_tokens` reports what was reused. A caller that ends every call with a per-call message
  (chatflow's stage message) so resumes each call at the history the calls share (4B, a 9.7k-token
  chatflow prompt: 3.5 s cold, 0.07-0.23 s resumed; `tests/prefix_cache.rs`).
- **Shared leading blocks** (`Seed` in `backend/src/engine.rs`): conversations of one project start
  with the same system block (system prompt, tools), but every cache key had its own state, so a
  new conversation read the whole block cold. The prefill that reaches a prompt's leading-block
  reuse point keeps a full copy of the state there (attention rows included; a checkpoint holds only
  their count). It is counted against the pool, at most 2 are kept, and the least recently used is
  dropped before admission would wait for room. A request whose own state covers less of its prompt
  starts from the longest copy that is a prefix of it (4B, 9.7k-token chatflow prompt: a new
  conversation's ROUTE 7.0 s cold → 0.14 s, the same greedy output; `tests/prefix_cache.rs`).
  `model.prewarm` with `shared_only` prefills only that block, for a conversation not created yet,
  and keeps nothing under its key; when the block is kept already it ends at admission, without a
  slot (`tests/prewarm.rs`).
- **Effort last** (`ChatTemplate::rendering` in `backend/src/template.rs`): Qwen3.8's template writes
  a reasoning-effort sentence ("Reasoning effort is set to low/xhigh. …"; `high` = `xhigh`, and the
  default) at the very start of the system turn; `none` and `medium` write none. So calls of
  different efforts on one key (chatflow: ROUTE/DECIDE at `none`, then ASK/DO/REPLY at the
  decided effort) shared 1 token and reused nothing. The plugin therefore always renders the
  system turn without it and appends the sentence the template would have written to the last
  message (chatflow's stage message, a caller's last user message), after every reuse point. The
  sentence comes from the template itself: a probe (a system and a user message, with the
  request's tools) is rendered at the request's effort and at the first accepted effort whose
  conversation renders like thinking off; the one run of text the first adds to the second is the
  sentence, appended preceded by the whitespace the template put around it. A template that
  announces nothing, or not as one inserted run, renders unchanged. The thinking budget still
  follows the requested effort. Measured on the 4B (`tests/prefix_cache.rs`, ROUTE/none, ASK/high,
  ASK/high after a tool call): calls 2 and 3 reuse 9679 and 9705 tokens (the shared history;
  the template's own placement left 1), prefill 67 and 92 ms against 2.8-2.9 s cold, output
  identical to the cold runs. A caller whose last message is a tool result gets the sentence inside
  that result, and a caller that keeps one announced effort across an agent loop without a per-call
  closing message now resumes at the history before the last message instead of continuing the
  whole state (the sentence moves with the last message).
  Idle conversations stay resident while the pool has room; the least recently used is spilled to
  disk (a hibernation) when room is needed and restored, without re-prefill, when its key returns.
- **Scheduling**: uzu decodes one sequence per forward pass (no batched multi-sequence decode), so
  concurrent requests are interleaved pass by pass, and prompts are prefilled in 1024-token steps.
  The highest priority class always steps first (foreground preempts agent work within one forward
  pass); requests rotate within a class; requests beyond the slots (or for a busy key) queue by
  priority, first come first served.
- **Prewarm and progress** (`backend/src/progress.rs`): `model.prewarm` prefills a prompt at
  background priority without generating. A real request never waits for one: a request of a
  higher class for the same conversation stops the running prewarm at its next step and continues
  from what it prefilled (its state and checkpoints are kept); without a free slot, a lower-class
  prewarm on another conversation stops; a prewarm still waiting beside a real request for its
  conversation is dropped. Every request publishes `model.progress` (`model@1` topics): queued
  (with the requests ahead), prefill (`prefill_done` / `prefill_total`), decode (`generated`,
  `tok_s`) and done (`finish`), at most one event per 250 ms within a phase.
- **Constraints** (`backend/src/constraint.rs`, vendor patch P2): `json_schema`, GBNF `grammar` (translated to
  llguidance's Lark dialect; regular rules become single lexemes) and `tool_choice` `required` or a
  named function (a grammar of the native call markup) are enforced by llguidance inside uzu's
  sampler, also on the speculative path (the mask prunes draft trees and every verified node). Drafted
  tokens are checked with llguidance's non-mutating `try_consume_tokens`: its `consume_token` puts the
  matcher into a permanent error state on a rejected token, which silently dropped every draft under a
  grammar (measured on the 27B: JSON decisions at 10-13 tok/s with 0 drafts accepted, 34-38 tok/s
  after; `tests/grammar_tokens.rs` guards it). With thinking on, the constraint engages after `</think>`.
- **Thinking budget**: a reasoning block that reaches its budget (`thinking.budget_tokens`, else by
  effort: low 2048, medium 4096, high/default 8192, xhigh 16384 tokens) is closed by feeding Qwen's own
  closing sentence and `</think>`; the model then answers on the same state (the reasoning is kept, a
  constraint applies from the answer's first token). Runaway reasoning otherwise ran for tens of
  thousands of characters in agent turns.
- **Sampling**: temperature (0 = greedy), top-k, top-p, min-p, repetition and presence penalties
  (vendor patch P3; penalized requests sample without drafting, which uzu's penalty kernel requires),
  and `seed` (per request; uzu derives each sample from it and the position, so a seed reproduces a
  sample). `n_probs` is refused: uzu samples on the GPU and never returns logits.
- **Hibernation**: `model.hibernate` writes a key's full state (attention KV, recurrent state,
  speculator context and its checkpoints) to the plugin data directory and frees it (the device
  allocation shrinks; vendor patch P6); the next request on the key restores it (`model.restore`
  event). Before a swap, `export_state` hibernates every idle conversation and the successor adopts
  them, so conversations survive a plugin upgrade.
- **Repetition guard** (`backend/src/repeat.rs`): a stream whose tail is one block written 8+ times (512+
  bytes) ends with `finish_reason: "repeated"`; there is no output cap.

## Images (vendor patch P7)

Qwen3.8-27B is a vision-language model; Mirai's checkpoint is its language model only. P7 (called
P5 in `docs/VISION-RESEARCH.md`; see `backend/vendor/uzu/PATCHES.md`) runs the model's own vision
tower and feeds its output to the Mirai M language model.

- **Weights** (`backend/src/vision/install.rs`): the 333 `model.visual.*` tensors (460,730,096
  parameters, bf16) of `Qwen/Qwen3.8-27B@1d4bf0f`, which lie as one contiguous 921,460,192-byte
  range at the end of `model-00001-of-00018.safetensors`. Two small range requests read the
  shard's header (its length is pinned; the tensor count, parameter count and contiguity are
  checked), one resumable range request fetches the bytes, and they are written as a standalone
  safetensors file with a sorted header, so the result is deterministic: its SHA-256
  (`ad17b11c…f421ee`) is pinned, verified and recorded like every model file; an install record
  in the graph (`(:VisionInstall)-[:ON]->(:Machine)`) holds the source range and the shard's
  published SHA-256. `config.json` and
  `preprocessor_config.json` come whole (git blob ids pinned). A server that answers a range
  request with the whole file is refused.
- **On demand** (`backend/src/engine.rs`, `backend/src/vision/worker.rs`): when a request has an
  image that is not in the reused prefix and not in the embedding cache (RAM, by SHA-256 and grid,
  256 MiB), the engine starts the vision thread, which loads the tower (0.3–0.5 s) in its own uzu
  context, so its own Metal command queue, and encodes there. The engine keeps stepping the other
  conversations; the request prefills up to its first pending image and waits there for the
  embeddings. The thread, the tower, its scratch and the cache are released
  `vision_idle_unload_s` after the last encode (the idle worker wakes for it). Text-only requests
  never touch the tower.
- **Memory**: the fitted plan does not reserve the tower (an installed tower leaves the KV pool
  as it is). Before the tower loads, and before every encode, the engine measures the device
  (Metal `currentAllocatedSize`, the measurement the fit uses) against the fit's budget: when the
  tower (1.14 GB) plus the encode's scratch (up to 0.45 GB for 2.4 MP) does not fit, idle
  conversations are hibernated, least recently used first, until it does; when none is left the
  image request fails with "not enough memory to load vision now: …" (retryable), never an
  out-of-memory. While the tower is resident, admission counts it as pool tokens (its measured
  bytes over the KV bytes per token), so growing conversations spill instead of overcommitting.
  Unloading changes nothing else. `model.vision` events report loads, encodes (with
  timings) and unloads; `model.info.vision` reports availability, `loaded`, the pixel budget and
  the idle period (`extra.input_modalities` lists `image` when available).
- **Preprocessing** (`backend/src/vision/preprocess.rs`, CPU): decode PNG/JPEG/WebP/GIF/BMP, drop
  alpha, `smart_resize` to multiples of 32 px within `[65536, max_image_pixels]` (Python's
  half-to-even rounding), Pillow's fixed-point bicubic (which torchvision's antialiased uint8
  path matches), `(x - 127.5) / 127.5`, the frame doubled for the temporal patch, patches in
  merge-window order. Tokens per image = (H/32)·(W/32): a 1920×1080 screenshot is 1920×1088,
  2,040 tokens.
- **Encoder** (uzu, Metal): see P7. Measured on this M5 Pro: 1.143 GB resident (heads padded
  72 → 128), 0.36 GB of scratch for a 1920×1088 screenshot (8,160 patches); 30–65 ms for 96
  patches, 1.9–2.9 s for the screenshot. It shares the GPU with decoding: in the concurrency test a
  text conversation kept decoding during an encode, at a reduced rate.
- **Prompt** (`backend/src/template.rs`, `backend/src/vision/prompt.rs`): `model@1` messages with
  `parts` render as Qwen's content list (`<|vision_start|><|image_pad|><|vision_end|>` per image,
  images in tool results inside `<tool_response>`), byte-identical to the official template; each
  `<|image_pad|>` then becomes the image's `rows × columns` pad tokens, sized from the part's
  width and height (else the file header) without encoding, so `model.count` is exact. Text that
  spells the placeholder token is refused.
- **Cache identity**: all pad tokens share one id, so prefix matching, checkpoints and
  hibernation compare an identity in which each image's run carries a key from its SHA-256 and
  grid; hibernation files store the image spans. The file is re-hashed when encoded: a file
  that no longer matches its digest is refused.
- **MRoPE**: positions are planned per input by uzu (P7), including images cut by the
  1024-token prefill steps; the state's rope delta travels with checkpoints and snapshots.

Still to verify on the 27B (needs the model run): answers on real screenshots against
Qwen3.8-27B UD-Q4_K_M + mmproj (the 4-bit Mirai M was distilled on text only), DFlash acceptance
after images, and prefill/encode latency end to end.
