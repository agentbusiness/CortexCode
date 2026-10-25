# Rust (`lang-rust`)

Everything CortexCode knows about Rust, as one `lang@1` plugin (the `lang` family, see
[docs/LANGUAGE-PLUGINS.md](../../docs/LANGUAGE-PLUGINS.md)): the code graph's facts of `.rs` files
and `Cargo.toml` manifests, rust-analyzer for diagnostics, definition, references and hover, the
cargo verifiers, and the Rust skills. It is the reference language plugin: a `LanguagePack` served by
`cc-lang-kit`.

One self-contained plugin folder, separable into its own repository (only `cc-protocol` and
`cc-lang-kit` come from outside):

| Path | What |
|---|---|
| `plugin.toml` | identity, build command and output, `data/` as a backend asset, source location, verification commands |
| `backend/` | Cargo workspace (depends on `cc-lang-kit` by path), binary `cc-lang-rust`: category `lang-rust`, contract `lang@1` |
| `data/skills/` | the skills (`<name>.md`: line 1 the one-sentence summary, a blank line, the Markdown body) |

## What it serves

Methods are called on the category: `lang-rust.describe`, `lang-rust.extract`, ...

- **Languages**: `rust` (symbol-id prefix `rs`, extension `rs`) and `cargo.toml` (prefix `cargo`,
  file name `Cargo.toml`).
- **extract**: tree-sitter-rust facts (items with qualified names, signatures, docs, attributes,
  parameters and types; calls with argument text, instantiations, field and const reads and writes,
  macro calls and the calls inside their arguments, `use` imports, `mod x;` declarations,
  `impl Trait for Type`, receiver-typed locals, `env::var` reads, programs run through
  `Command`, errors returned, axum / actix routes and Tauri commands) and Cargo.toml facts (package,
  dependencies by section, binaries, the build script, `[package.metadata.scripts]`). The extractors
  are the memory plugin's, ported to `lang@1` facts. Manifest facts that need the disk (Cargo.lock
  versions, registry sources, which binary and build-script files exist) come from `extract`'s `root`;
  without a root they are left out, never guessed.
- **Server**: rust-analyzer of the workspace's own toolchain (`rustup which rust-analyzer` run in
  the root, so its `rust-toolchain.toml` decides), else one on PATH or in `~/.cargo/bin`, each
  accepted only when `--version` runs. It runs with its own target directory
  (`target/rust-analyzer`), so its on-save `cargo check` never holds the build-directory lock the
  agent's builds need. rustup always runs with `RUSTUP_AUTO_INSTALL=0`.
  `[plugin.config] server_path` names another executable.
- **install**: `rustup component add rust-analyzer` for the active toolchain, when rustup exists;
  the log says which toolchain. Nothing else is installed or touched.
- **Verifiers** (in a project with `Cargo.toml`, requiring `cargo`): `cargo check --message-format=short` (build),
  `cargo test` (test), `cargo clippy -- -D warnings` (lint).

## Build and run

```sh
cd backend && cargo build --release && cargo clippy --all-targets -- -D warnings
cortex-pack build plugins/lang-rust     # -> plugins/lang-rust/target/lang-rust-<version>-<target>.ccplugin
```

The frame runs it once installed (`cortex-pack build plugins/lang-rust --install`). The probe extracts a sample
through the real `extract`, reads every skill and reports whether rust-analyzer can run (an absent
server does not fail the probe: extraction and skills work without it).
