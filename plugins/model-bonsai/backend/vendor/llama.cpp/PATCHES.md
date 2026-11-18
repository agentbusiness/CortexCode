# CortexCode notes on the vendored llama.cpp

Source: https://github.com/PrismML-Eng/llama.cpp, branch `prism`, commit
`3f035ba664f8f08c3d9432b6421f8f1a0da1f37a` (reviewed 2026-11-18), recorded in `REVISION`. PrismML's fork
adds the ternary quantization types of the Bonsai models (`PQ2_0`, `PTQ1_0`) with Metal kernels,
and applies the Hadamard rotation the files declare (`prism.hadamard.*` metadata). Stock llama.cpp
cannot load these files.

Only the sources this plugin builds were copied, byte for byte: `CMakeLists.txt`, `cmake/`,
`include/`, `src/`, `ggml/` (the core with the CPU, BLAS and Metal backends only), `tools/mtmd/`
(the library and its model sources: no CLI, tests or sample media), `vendor/` (header libraries)
and the licenses. Tests, tools, the server, the web UI, examples and the other GPU backends are not
included.

No local patches. A change goes here with a `CortexCode patch` marker in the source.

## Upgrading

Copy the same file set from the new commit, re-apply any patch listed here, update `REVISION` and
this file, then build the plugin and check it in the app.

## 2026-11-18 source upgrade

Rebased the unmodified source subset onto PrismML-Eng/llama.cpp prism
3f035ba664f8f08c3d9432b6421f8f1a0da1f37a. Retains the ternary PQ2_0/PTQ1_0 kernels,
Hadamard metadata, Metal kernels and mtmd/MTP interfaces. No local vendor patches.
Source archive SHA256: 98a3c799fbde80b0460ea821974e129a1892f2402610abc5f3baf8ef5f721411.
Shared-prefix cache paths include the engine REVISION, so this source change starts a new
cache and reads the original tokens again. If a restore otherwise fails, restore_seed drops
that cached state and re-prefills. The upstream sequence-file format version change does not
itself protect the raw get/set_data_ext payload used by this shim.
No builds or runtime checks were performed.
