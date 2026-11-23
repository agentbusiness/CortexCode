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


## P1–P8 rebase onto Uzu0.7.1 — 2026-11-18

Upstream90f8bdb7be9cbe0285efe96e37509a1041ddb138 (tag0.7.1), source SHA256
6b083018764051deb9046b57ba9d97739a6b459d257d3ca0bca89117698a31ee.

P1: snapshot buffers now use GlobalGpuBuffer. The snapshot retains its backend context;
write_to copies the completed GPU payload out through Context::copyout, while read_state_snapshot
uploads through create_buffer_cpu_gpu/handoff. Attention copies all four KVCache code/scale
buffers and records the complete KVLayout; older or different physical layouts fail restore,
so the existing owner re-prefills the conversation. Append-only checkpoints remain length-only;
ring checkpoints copy their rows. UZUSNAP4 rejects UZUSNAP2/3 immediately at the magic
header, before reading or uploading their old layout. DeltaNet/short-convolution/drafter states stay covered.

P2: llguidance remains the grammar provider. Upstream0.7 always exposes constrained decoding;
the grammar feature name remains as a compatibility feature. Tokenizer JSON still bridges
llguidance's own adapter; the engine tokenizers dependency is0.23.2 to match the plugin's
concrete tokenizer value. The obsolete xgrammar compile-error variant is removed.

P3/P4: presence penalty, sampling seed, generated-only penalty window, speculation control,
context capacity and pending-output operations remain. Sampling kernels keep the original
per-distinct-token presence penalty. Stream public methods keep existing callers unchanged.

P5/P6: device facts, KV byte accounting (from KVLayout code+scale sizes), allocator trimming
remain. Both CPU and Metal GPU buffer kinds follow the0.7.1 API.

P7: vision uses async ParameterLoader reads driven synchronously at the existing caller API;
uploads use CPU/GPU handoff and outputs use Context::copyout. The removed full-precision attention
interface is replaced by0.7.1's query/KV code+scale preparation with the same padded head count,
non-causal mask, original head-dimension scale and two-dimensional RoPE tables. Vision attention
now follows upstream quantization and can change image embeddings/generated text numerically.
Image input overlay, MRoPE positions, rope delta, DFlash verification positions and snapshot
rope delta remain. Older KV snapshots are rejected rather than reinterpreted.

P8: the full DFlash draft block must fit the remaining context; retained.

Additional retained local stream design: Uzu0.7.1 introduced pipelined eager generation.
Cortex's existing stream state machine is retained and adapted to GPU copyout because its
scheduler interleaves one forward pass at a time and its callers use pending-output/empty-input
continuation. This preserves those interfaces instead of silently switching scheduler semantics.

No builds, tests, benchmarks, checks or app execution performed. Source candidate only.


### Complete0.7 rebase corrections

P1 also carries the new recurrent `is_empty` state in a validated0/1 snapshot scalar.
Restoring a used state into a fresh one must not trigger0.7's deferred initialization and erase
the restored context. An empty snapshot payload is explicitly zeroed instead of copying
uninitialized device allocations. UZUSNAP4 rejects old2/3 headers before parsing/uploading.
P5 restores the removed GPU-family enumeration/accessor and derives the report from the device.
P3 optional Vulkan math/signatures and Slang stub signatures match the presence-penalty API;
portable grammar dependencies include llguidance's explicit wasm feature on wasm32.

## P9: persisted0.6 model packages at the0.7 load boundary

`utils/model_format.rs` normalizes known0.6 structural fields in memory before strict typed
configuration reading. Installed model/config/weight files, tensor payloads and byte offsets are
unchanged. All public model-loading/discovery boundaries and the DFlash loader use it. Safetensors
headers normalize names and serialized HybridSpec metadata, keeping original dtype/shape/ranges
and read tracking. Mixed schemas, unknown fields and migrated-name collisions remain errors.

Old transformer layers become explicit additive residual sublayers, preserving every norm,
convolution and body configuration. Embeddings keep the old no-index-permutation setting;
DeltaNet keeps the old fixed SiLU alpha1 gate. HybridSpec moves the old Hadamard block size and
processing mode into HadamardSpec; sign tensor names stay unchanged. Old SpeculatorModelConfig
is unwrapped and its tensor/metadata `speculator.` root removed in memory. ChatCodec's parser
pattern moves into ResponseFormat. Cortex's own template/reasoning parser continues reading
its untouched original model config, including its original thinking tag.

PLE remains an epilogue injection, followed by a scalar when present. Rare0.6 scalar layers
without PLE require two explicit legacy sublayer hooks: scale the combined mixer residual
before MLP pre-normalization, and scale the MLP post-normalized update before its residual add.
These default off for new0.7 configs. Current pinned Qwen3.8/LFM2.5 configs have no PLE/scalars.
Additional kernel stages and upstream attention changes may alter floating-point results;
no claim of bit-identical inference is made.

The Cortex engine thread drives upstream's async language-model load with a current-thread tokio
runtime: 0.7.1's load calls `futures::executor::block_on` itself (the matmul weight layouts,
`backends/metal/kernel/matmul/layout/`), which panics inside another `block_on` on the same thread.
No blanket synchronous language-load wrapper is added. All P1–P8 operations are retained;
full local-difference inventories and the independent source review accompany the migration.

### First build of the0.7.1 rebase — 2026-11-18

The rebase compiled with these corrections: `FlatTrie::len` (test-only upstream) and
`TrieNode::flat` (dropped upstream) are kept for the retained stream scheduler; `BatchTopology::new`
takes its nodes by value; the decode pass's token ids are a `&dyn GpuBuffer`, as0.7.1's encoders
take GPU buffers; the vision tower's GELU activations pass alpha 1.0 (it scales SiLU only).

## P10: several streams in one forward pass (batched decode)

uzu runs one sequence per forward pass, so N streams interleaved each decode at about 1/N of one
stream's speed and the GPU reads the 27B weights N times for N streams' tokens. A pass costs
about the same up to the rows the device's tree budget was tuned for (the weights dominate), so
P10 runs every generating stream's rows in one pass.

- `Mixer::{batch_supported, encode_batch}` with `MixerBatchPart` (a part's first row, its own
  `BatchTopology`, RoPE tables and state). Attention and Gated DeltaNet run their projections once
  over all rows; the cache write and attention (`Attention::attend_core`), and the convolution and
  recurrence (`DeltaNet::{flat_core, tree_verify_core}`), run per part on a copy of its rows (the
  kernels read from offset zero) and their outputs are copied back in order before the out
  projection. A single stream still takes `Mixer::encode`, which calls the same cores: its
  kernels and their order are unchanged. Short convolution has no batched form.
- `TransformerLayer::{batch_supported, encode_batch}`: a layer batches when its mixer does and it
  has no sublayer convolution and no n-gram injection (everything else is row-wise; stream ops
  use only the row count). `Transformer::{batch_supported, encode_batch}` (additive residual
  only) with `TransformerBatchSequence`: each sequence's rotary tables from its own context and
  rope delta (text only), each layer's state per sequence (shared-KV layers read their owner's),
  the captured layers' rows copied out per sequence for its speculator.
  `Decoder::{batch_supported, encode_batch}` returns every row's logits and, per sequence, its
  final hidden rows and captured features.
- `LanguageModelStream::generate` is split into `begin` (uzu's code up to the input of the pass:
  the previous output, the acceptance, the speculator's tree) and `encode_single` (uzu's own pass,
  unchanged); `accept_pass` is uzu's tree acceptance, shared with `settle`.
  `LanguageModel::generate_batch` steps several streams: they run in lockstep (a stream holding
  accepted tokens hands one out per step while the others report `BatchStep::Waiting`; the pass
  runs when every stream needs one), each drafts within `tree_budget / streams` nodes
  (`DFlashTfmSpeculator::make_shape_within`; under two nodes it decodes without drafting), and the
  members' inputs (resolved on the CPU: constant tokens only) go through one `encode_batch`, then
  each member's own sampling, acceptance and penalty ring, as `encode_single` does them. The pass
  is waited for, so the next step reads its outcome without a wait. One member left takes
  `encode_single`; a model without batch support steps the streams one after another.
- Prefill is not batched (a 1024-token chunk is compute-bound on its own). The drafters still run
  one stream after another, each its own small pass.

Source candidate: not built, not run.
