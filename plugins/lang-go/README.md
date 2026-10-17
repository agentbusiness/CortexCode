# Go (`lang-go`)

Everything CortexCode knows about Go, as one `lang@1` plugin (the `lang` family, see
[docs/LANGUAGE-PLUGINS.md](../../docs/LANGUAGE-PLUGINS.md)): the code graph's facts of `.go` files
and `go.mod` manifests, gopls for diagnostics, definition, references and hover, the go verifiers,
Go / gopls / delve installed into CortexCode's toolchains directory, and the Go skills. A
`LanguagePack` served by `cc-lang-kit`, built like [lang-rust](../lang-rust/README.md).

| Path | What |
|---|---|
| `plugin.toml` | identity, build command and output, `data/` as a backend asset, source location, verification commands |
| `backend/` | Cargo workspace (depends on `cc-lang-kit` by path), binary `cc-lang-go`: category `lang-go`, contract `lang@1` |
| `data/skills/` | the skills (`<name>.md`: line 1 the one-sentence summary, a blank line, the Markdown body) |
| `verify/` | gate script, run against the real frame |

## What it serves

Methods are called on the category: `lang-go.describe`, `lang-go.extract`, ...

- **Languages**: `go` (symbol-id prefix `go`, extension `go`; the memory plugin's names, so code-graph
  ids stay the same) and `go.mod` (prefix `gomod`, file name `go.mod`).
- **extract**: tree-sitter-go facts, the memory plugin's extractor ported to `lang@1` facts: the
  package clause, imports with their bound names, functions and methods (attached to their receiver
  type), structs with fields, interfaces with methods, consts and vars, docs (go/doc rules,
  directives such as `//go:noinline` excluded), `go test` tests / benchmarks / examples in
  `_test.go` files and `t.Run` subtests, calls, instantiations, reads and writes of package-level
  names and receiver members, receiver-typed locals, `os.Getenv` reads, programs run through
  `exec.Command`, and error values returned. go.mod: the module path (`package`), `require`s
  (`// indirect` ones in section `indirect`) with the module-cache directory
  (`$GOMODCACHE`/`$GOPATH/pkg/mod`, upper case escaped as `!x`) when present, `replace`s (a local
  directory resolved against the extract `root`, another module looked up in the cache), and `tool`
  directives as commands (`go tool <name>`).
- **install**: Go 1.27.2 (the go.dev/dl darwin archive, SHA-256 pinned) into
  `toolchains/go/1.27.2/go`, then with it `go install golang.org/x/tools/gopls@v0.23.0` and
  `go install github.com/go-delve/delve/cmd/dlv@v1.27.2` into `toolchains/gopls/0.23.0/gopls` and
  `toolchains/dlv/1.27.2/dlv` (GOBIN a staging directory renamed into place; GOPATH, module cache
  and build cache in `toolchains/gopath`; `GOTOOLCHAIN=local`; `GOFLAGS=-modcacherw`). Nothing is
  installed system-wide.
- **Server**: the installed gopls, else one on PATH, in `$GOBIN`, `$GOPATH/bin` or `~/go/bin`,
  accepted only when `gopls version` runs. gopls runs `go list`, so a `go` is required too (on PATH,
  else the installed one, which the server's PATH ends with); without one the server is reported
  unavailable, with the reason. gopls runs with `GOTOOLCHAIN=local` (a go.mod `toolchain` line never
  downloads another Go) and `linksInHover: false`. `.go` files are opened in it by extension,
  `go.mod` and `go.work` by file name (other `*.mod` files are not gopls's). `[plugin.config]
  server_path` names another executable.
- **Toolchain** (`describe`'s `path` and `env`, which the tools prepend to every command's PATH and
  set): the installed Go's `bin` with `GOTOOLCHAIN=local` when no runnable `go` is on PATH, and the
  gopls and delve directories when installed. `GOROOT` is not set (the go command derives it from
  its own location; a stale GOROOT is a classic breakage) and `GOFLAGS=-mod=mod` is not set (it
  would let builds and tests rewrite go.mod / go.sum instead of reporting a missing requirement).
- **Verifiers** (in a module with `go.mod`, requiring `go`): `go build ./...` (build),
  `go vet ./...` (lint), `go test ./...` (test).

## Build and run

```sh
cd backend && cargo build --release && cargo test --release && cargo clippy --release --all-targets -- -D warnings
cortex-pack build plugins/lang-go     # -> plugins/lang-go/target/lang-go-<version>-<target>.ccplugin
```

The frame runs it once installed (`cortex-pack build plugins/lang-go --install`). The probe extracts a sample
through the real `extract`, reads every skill and reports whether gopls can run (an absent server
does not fail the probe).

## Verification

`verify/lang_go.mjs` loads the folder in a real frame and checks every method against truth computed
independently: definition lines and call sites found in the fixture text, the compiler's errors from
`go build ./...` (run with the described toolchain path) before and after a fix, definition,
references and hover positions of a function, skills read back with their summaries, the toolchain
directories, the `lang-go.server` event, and refusals. gopls and Go must be runnable;
`node verify/lang_go.mjs --install` lets `lang-go.install` put Go, gopls and delve into the run's
temporary CORTEX_HOME first.
