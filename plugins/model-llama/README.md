# model-llama: the CortexCode model plugin (`model@1`) for Linux

`cc-model-llama` serves the `model` category over CPP/1 with **Qwen3.8-27B** on upstream
llama.cpp's `llama-server`, supervised as a child on loopback. `backend/vendor/llama.cpp` holds
llama.cpp at `468ad58` (unslothai build b11876), the same sources as CortexTerm's `model-llama`.
Swap policy: drain first. It runs on Linux only; on macOS `model-bonsai` and `model-uzu` serve.

## The GPU decides

A model runs only on a GPU. At start the plugin asks `llama-server --list-devices` which GPUs its
backends can use, and takes the best one (CUDA before Vulkan, since an NVIDIA card shows under
both; the largest within a backend), pinned with `--device` (the drafter follows it).

| GPU memory | Variant | Weights |
| --- | --- | --- |
| 24 GB or more | `qwen3.8-27b-q4`: unsloth UD-Q4_K_XL | 17.6 GB |
| 16 GB or more | `qwen3.8-27b-q3`: unsloth UD-Q3_K_XL (vision projector on the CPU) | 13.1 GB |
| less, or no GPU | none: the mesh serves | |

Without such a GPU the plugin installs nothing, loads nothing and lends nothing: `model.info`
answers `unavailable`, so the mesh's card shows no model, and every call goes to a reachable
member's Qwen (`mesh.call`), on this machine or another one.

## Speed

- **DFlash 2 drafts** (`--spec-type draft-dflash`): z-lab's block-diffusion drafter for
  Qwen3.8-27B (Q4_K_M, 1.1 GB) proposes a block of tokens in one pass, and Qwen verifies them.
  Decoding stays lossless. At most `draft_max` (7) tokens per step.
- **CUDA** for NVIDIA cards (Turing and newer; CUDA graphs on), **Vulkan** for AMD, Intel and
  NVIDIA through their drivers. The Vulkan kernels are built with cooperative matrix 1 and 2,
  integer dot product, bfloat16, FP8 and FP4 (the build machine's shader compiler is shaderc
  2026.4).
- Flash attention with a `q8_0` KV cache, checked once per machine and recorded in the graph; f16
  when it fails.
- Two slots of 32k tokens each, prompt caching per conversation (`cache_prompt`).

## Other apps' models

The other apps' models on this machine stay loaded when Qwen loads: each app lends its own (the
cortex-network). While another app on this machine already holds a Qwen, calls are forwarded to it
and no second copy of the same model loads here.

## Building

`sh build.sh` in `backend/` (cortex-pack runs it) builds `llama-server` with
`GGML_BACKEND_DL`: the CUDA backend (with `libcudart`, `libcublas` and `libcublasLt` beside it, so
only NVIDIA's driver is needed on the machine), the Vulkan backend, and the CPU variants of every
processor generation, all in `backend/target/release/llama/` with `RPATH=$ORIGIN`. A backend
whose driver the machine lacks does not load. Then the Rust backend. The release builds it in
`scripts/release/linux.Dockerfile`, which carries CUDA 13.4 and the Vulkan SDK 1.4.363 tools.

## Files

The weights, the drafter and the vision projector are pinned by repository, revision, size and
SHA-256 (`backend/src/catalog.rs`) and live in the shared models folder (`~/.cortex-models`,
`docs/SHARED-MODELS.md`), downloaded with resume and checked once.
