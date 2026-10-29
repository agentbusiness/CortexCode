# CortexCode patches to the vendored uzu

Source: https://github.com/trymirai/uzu at `1763a366c048e2065343619a28524d493faf5391` (main,
2026-10-28; release 0.6.13 plus 14 commits up to #1027, before the WebGPU runtime merge
`f1231705`), see `REVISION`. Upgraded from `391abc29` (0.6.7) on 2026-10-28: the patches merged
as text except the attention state, which upstream rebuilt around `KVStorage` for the int8 KV cache
(P1 below). Only the crates the engine needs are vendored: `uzu-engine`, `uzu-engine-macros`, `legacy/shoji` (stream metrics type), `legacy/bindings`
and `legacy/bindings-types` (proc macros shoji uses). The workspace manifest lists only these and
drops the workspace dependencies of the crates left out; `uzu-engine`'s dev-dependencies and its
`unit/` folder are removed (its unit tests are not built here).

uzu's engine API is async since 0.6.13 (for WebGPU): on Metal its futures wait for the device. The
patches below that wait for a command buffer (snapshots, restores, token embeddings, the vision
tower) drive it with `futures::executor::block_on`, as uzu itself does in its synchronous paths.

Each change is marked `CortexCode patch Pn` in the source.

## P1: state snapshots (context checkpoints, hibernation)

`MixerState` gains `encode_snapshot` / `encode_restore` (attention, Gated DeltaNet, short
convolution), `TransformerState` and `DFlashState` snapshot their layers, and
`engine/language_model/state.rs` adds `LanguageModelStateSnapshot` with
`LanguageModel::{snapshot_state, restore_state, read_state_snapshot}` and `write_to`.

- `SnapshotMode::Full` copies everything (attention K/V rows `[0, length)`, recurrent state, the
  speculator's context) into CPU-visible buffers; written to disk it restores into a fresh state of the
  same model. Used for hibernation.
- Attention layers copy what their `KVStorage` holds for rows `[0, length)` (`KVStorage::row_buffers`):
  keys and values in bf16, or with the int8 KV cache the codes and then the per-key scales. Int8 is
  laid out by blocks of 32 keys, so the rows are a prefix of every buffer. The layer's scalars name
  the format; a snapshot of the other format, or of a build before int8 (five scalars), does not
  restore, and that conversation starts cold.
- `SnapshotMode::Checkpoint` copies the recurrent state and records attention lengths only: an
  append-only cache's earlier rows stay valid, so a state can be rolled back to the checkpoint.
  Used for context checkpoints (prefix reuse when a history diverges) and stored next to a hibernated
  state so a regenerated turn still rolls back after a restore.

uzu has no way to shorten a state (Gated DeltaNet is recurrent), so without P1 a conversation whose
history changed after a point could only be re-prefilled from the start, and nothing could be saved.

## P2: constrained decoding on llguidance, GBNF via Lark

uzu 0.6.7 constrains sampling with xgrammar-rs 0.2.8. With the Qwen tokenizer its token mask
disagreed with its own matcher: a random walk through the mask of a JSON-string grammar (the shape
chatflow and agents send) produced "mask allowed 46102 (\"\\tms\") but accept rejected it" after
`{"route":"answer","files":[":@"\n\n\t\t\t\t\t ... ,"\\rome js` (a raw tab inside a JSON string,
which the grammar forbids), and uzu then fails the whole generation ("Grammar rejected the token").
`grammar/grammar.rs` keeps uzu's interface (`next_bitmask`, `accept_token`, `rollback`,
`is_terminated`, trigger engagement after `</think>`) and implements it on `llguidance` 1.9.1 with
`toktrie_hf_tokenizers` (the tokenizer's own `tokenizer.json`), whose mask comes from the same token
trie the matcher consumes. The compiler (`grammar_compiler`, built once per model) replaces the
per-request xgrammar compiler. `GrammarConfig::Lark` replaces nothing upstream relies on; GBNF is
translated to Lark by the plugin (`src/constraint.rs`). Stop tokens end a sequence without being fed
to the matcher; a rollback removes exactly the tokens the matcher consumed.

The mask also prunes the speculator's draft trees and is applied to every node the target model
verifies (uzu's own design), so drafted tokens that violate the grammar are never accepted.

## P3: per-request speculation switch and presence penalty

`LanguageModelStreamOptions` gains `speculation` (draft with the speculator, default true) and
`penalty_window_includes_state` (default true, uzu's behaviour). `SamplingMethod::Stochastic`
gains `presence_penalty`; the `RepetitionPenalty` kernels (Metal and CPU) subtract it after the
multiplicative repetition penalty, once per distinct token in the window. uzu's penalty kernel
supports only flat batches, so penalized requests run without drafting (the plugin turns
speculation off for them).

## P4: state accessors

`LanguageModelState::{max_context_length, set_sampling_seed, clear_pending_output,
has_pending_output}`: per-request seeds (uzu derives every sample's seed from the state's seed and
the position) and dropping the token a finished stream sampled but never fed back.

## P5: device facts

`Context::device_facts` (default: none; Metal: name, GPU cores and family, whether the M5 matrix
units are in use, `recommendedMaxWorkingSetSize`, `currentAllocatedSize`, sparse buffer support),
exposed as `Engine::device_facts`. `LanguageModelState::kv_bytes_per_token` (through
`TransformerState` and `MixerState::kv_bytes_per_token`, attention only) measures the attention KV
bytes one more token takes in the format uzu chose for this device; the plugin sizes its pool by it
(Qwen3.8-27B-M on M5: 33,024 B with int8 instead of 65,536 B with bf16).

## P6: releasing cached buffers

uzu's `BlockAllocator` keeps every freed dense buffer in a per-size cache that never shrinks, so the
one-off sizes of state snapshots stayed allocated after a hibernation (measured: a 675 MiB snapshot
of a 20k-token 4B state stayed resident after it was dropped; with the trim the device returned to
its post-load allocation). `BlockAllocator::trim`, `Context::trim_buffer_cache` and
`Engine::trim_buffer_cache` release the cache; the plugin calls it after a hibernation or restore.

## P7: image input (vision tower, MRoPE, embedding injection)

Called "P5" in `docs/VISION-RESEARCH.md` (written before P5 and P6 above existed). uzu 0.6.7 has no
image path at all; Mirai's checkpoints drop the vision tower and collapse Qwen's interleaved
MRoPE to 1-D (exact for text only). P7 adds what the language model side needs plus the encoder.

- `engine/vision/mod.rs`: `Engine::load_vision_model` and `VisionModel::encode`, the
  Qwen3-VL/3.5 tower (no DeepStack) on uzu's own kernels: the matmul kernel with fused bias
  (patch embedding as a linear over the flattened patch, QKV, projections, MLPs, merger),
  `NormalizationKernel` as LayerNorm with bias (`subtract_mean`, f32 affine), `ActivationKernel`
  (GELU tanh in the blocks, exact GELU in the merger), `TensorAddScaleKernel` for residuals,
  `AttentionPrepareKernel` with a caller-built 2-D rotary table, and the non-causal
  `AttentionKernel` over the whole image. The tower's 72-wide heads are zero-padded per head to
  128 at load (uzu's attention kernels take 64/128/256; zero channels change nothing, the scale
  stays 1/sqrt(72)), so the resident tower is 1.14 GB instead of 0.92. Position embeddings are
  bilinearly interpolated on the CPU with the reference's bf16 arithmetic; one command buffer per
  block, the residual stream in one device buffer. `VisionModelConfig::{resident_bytes,
  scratch_bytes}` bound its memory (measured on Qwen3.8-27B's tower: 1.143 GB resident, 0.364 GB
  scratch for 8,160 patches; the bounds are 1.156 and 0.449). The plugin runs it in its own
  `Engine` (own context and command queue) on its own thread.
- `engine/language_model/images.rs`: `ImageEmbedding` / `ImageInput` in
  `LanguageModelStreamOptions::images` and `plan_input`, which places an image's
  `rows × columns` tokens relative to a stream's input (negative start: the image began in an
  earlier input) and gives every row its (T, H, W) position: text `p = context + rope_delta`
  on all axes, image tokens `(s, s + row, s + column)`, text after an image at `s + max(rows,
  columns)` (Qwen's `get_rope_index`). `LanguageModel::token_embeddings` and `model_dim` serve
  checks and validation.
- `encodable_block/decoder.rs`: `EmbeddingRows` blitted over the embedding lookup's rows. The
  lookup undoes the table's incoherence (Hadamard) rotation, so the residual stream is in the
  original basis and the merger's bf16 rows go in unchanged.
- `encodable_block/transformer.rs`: `TransformerState::rope_delta` (Qwen's `rope_deltas`; 0 for
  text, so text positions are computed exactly as before) and `Transformer::encode_with_rope_rows`
  (explicit per-row positions). Decoding, including speculative trees, uses
  `context + height + rope_delta`.
- `encodable_block/mixer/attention/rope.rs`: the tables as pure functions (`rope_tables`, the
  former body unchanged, and `mrope_tables`, interleaved: pair `i` reads H when `i % 3 == 1` and
  `i < 3·section[1]`, W when `i % 3 == 2` and `i < 3·section[2]`, else T). A batch whose rows all
  have equal axes uses the 1-D table, bit for bit.
- `engine/language_model/stream/stream.rs`: per prefill batch, the rows and positions of the
  images in it; after the input, the state's rope delta.
- `engine/language_model/state.rs`: snapshots carry the rope delta (`UZUSNAP3`; `UZUSNAP2` files
  still read, as delta 0); `LanguageModelState::rope_delta`.

The speculator (DFlash) keeps its own sequential positions; verification stays lossless after
images, only the acceptance rate can change.

## P8: no draft past the context's end

`speculators/dflash_tfm.rs` `make_shape`: a DFlash draft block always takes `dflash_depth` positions
(the block size, 16), whatever depth the tree is cut to, and `encode_draft` asserts they fit the
draft model's RoPE capacity. uzu cut only the tree, so a generation that reached the last 16
positions of the context panicked ("DFlash block positions exceed configured RoPE capacity") and the
conversation was lost. With fewer positions left than a block, the stream decodes without drafting
and ends at the context's end as a plain stream does.
