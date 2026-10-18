# CortexCode notes on the vendored llama.cpp

Source: https://github.com/PrismML-Eng/llama.cpp, branch `prism`, commit
`0082ac24a89ae62132041b40e2c0170e278a98f4` (2026-10-16), recorded in `REVISION`. PrismML's fork
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
