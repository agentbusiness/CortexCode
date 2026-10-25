# C/C++ (`lang-cpp`)

Everything CortexCode knows about C and C++, as one `lang@1` plugin (the `lang` family, see
[docs/LANGUAGE-PLUGINS.md](../../docs/LANGUAGE-PLUGINS.md)): the code graph's facts of C and C++
sources and headers and `CMakeLists.txt`, clangd for diagnostics, definition, references and hover
(pointed at the workspace's `compile_commands.json`), the CMake / ctest / make verifiers, and the
C/C++ skills. A `LanguagePack` served by `cc-lang-kit`, built like
[lang-rust](../lang-rust/README.md).

| Path | What |
|---|---|
| `plugin.toml` | identity, build command and output, `data/` as a backend asset, source location, verification commands |
| `backend/` | Cargo workspace (depends on `cc-lang-kit` by path), binary `cc-lang-cpp`: category `lang-cpp`, contract `lang@1` |
| `data/skills/` | the skills (`<name>.md`: line 1 the one-sentence summary, a blank line, the Markdown body) |

## What it serves

- **Languages**, exactly the memory plugin's (code-graph ids stay the same): `cpp` (prefix `cpp`:
  `cc cpp cxx hpp hh hxx h`; `.h` is C++, the grammar parses C declarations too), `c` (prefix `c`:
  `c`), and `cmake` (prefix `cmake`, file name `CMakeLists.txt`).
- **extract**: tree-sitter-cpp facts, the memory plugin's extractor ported to `lang@1` facts:
  includes, namespaces, classes / structs / unions with fields and access, enums with enumerators,
  functions, methods (in-class and out-of-line `T::f`, attached to their class), prototypes,
  typedefs and aliases, `#define` constants and macros, docs, GoogleTest / Boost.Test / Catch2 /
  doctest tests, calls (scope qualifiers keep their `::`), instantiations, reads and writes,
  `throw`s, typed locals, `getenv` reads and programs run through `system` / `popen`.
  CMakeLists.txt (the CMake language: invocations, quoted / bracket / unquoted arguments, comments):
  `project` (`package`), `add_executable` / `add_library` targets as commands with the source files
  that exist under the extract `root`, `add_test` tests, `find_package` (REQUIRED -> `dependencies`,
  else `optional`) and `FetchContent_Declare` dependencies (source under `build/_deps/<name>-src`
  when present). `${...}` references are not evaluated.
- **Server**: Apple's clangd (`xcrun --find clangd`, then `/usr/bin/clangd`), then one on PATH,
  then the installed one, each accepted only when `--version` runs. Which compilation database to
  read is per workspace: the server of each root starts (the pack's `server_launch`) as
  `clangd --log=error --compile-commands-dir=<dir>` for the `compile_commands.json` in the root,
  else in its `build*` directories (`build` first). Without one, every `diagnostics` answer for
  that root gets one warning (`no-compile-commands`, line 1, source `lang-cpp`) on each C/C++ file
  it covers (the pack's `annotate_diagnostics`) saying so and how to create it
  (`cmake -S . -B build -DCMAKE_EXPORT_COMPILE_COMMANDS=ON` in a CMake project).
  `[plugin.config] server_path` names another clangd (it gets the same arguments).
- **install**: nothing when a system clangd runs (the log says which); otherwise clangd 23.1.0
  (github.com/clangd/clangd `clangd-mac-23.1.0.zip`, SHA-256 pinned) into `toolchains/clangd/23.1.0`.
- **Toolchain** (`describe`'s `path`): the installed clangd's `bin`, only when it was installed.
- **Verifiers**: with `CMakeLists.txt`: `cmake -S . -B build -DCMAKE_EXPORT_COMPILE_COMMANDS=ON &&
  cmake --build build` (build, requires `cmake`; it also writes the database clangd reads) and the
  same followed by `ctest --test-dir build --output-on-failure` (test, requires `cmake` and `ctest`),
  both through `sh -c`; with a `Makefile`: `make` (build, requires `make`).

## Build and run

```sh
cd backend && cargo build --release && cargo clippy --release --all-targets -- -D warnings
cortex-pack build plugins/lang-cpp
```
