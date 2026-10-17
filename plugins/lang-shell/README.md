# Shell (`lang-shell`)

Everything CortexCode knows about shell scripts (sh, bash, zsh, dash, ksh), as one `lang@1` plugin
(the `lang` family, see [docs/LANGUAGE-PLUGINS.md](../../docs/LANGUAGE-PLUGINS.md)): the code graph's
facts of scripts, bash-language-server with ShellCheck for diagnostics, definition, references and
hover, the shellcheck / `bash -n` verifiers, and the shell skills. A `LanguagePack` served by
`cc-lang-kit`, built like the reference plugin `lang-rust`.

One self-contained plugin folder, separable into its own repository (only `cc-protocol` and
`cc-lang-kit` come from outside):

| Path | What |
|---|---|
| `plugin.toml` | identity, build command and output, `data/` as a backend asset, source location, verification commands |
| `backend/` | Cargo workspace (depends on `cc-lang-kit` by path), binary `cc-lang-shell`: category `lang-shell`, contract `lang@1` |
| `data/skills/` | the skills (`<name>.md`: line 1 the one-sentence summary, a blank line, the Markdown body) |
| `verify/` | gate script, run against the real frame |

## What it serves

Methods are called on the category: `lang-shell.describe`, `lang-shell.extract`, ...

- **Languages**: `shell` (symbol-id prefix `sh`, extensions `sh`, `bash`, `zsh`, `#!` interpreters
  `sh`, `bash`, `zsh`, `dash`, `ksh`): the memory plugin's name and prefix, so the code graph's ids
  stay the same.
- **extract**: tree-sitter-bash facts (zsh, dash and ksh are parsed with the bash grammar):
  functions with their comment docs and the positional parameters they use, top-level variables
  (exported or not, with their values), every command run with its arguments, `source` / `.`
  imports, scripts run through an interpreter (`bash x.sh`, `python3 tools/x.py`) or by path
  (`./scripts/x.sh`), and environment reads (upper-case variables the script never assigns). The
  extractor is the memory plugin's, ported to `lang@1` facts.
- **Server**: bash-language-server (npm, pinned 5.8.1) run as `bash-language-server start`: the one
  installed in `$CORTEX_HOME/toolchains/bash-language-server/5.8.1/`, else one on PATH, each accepted
  only when `--version` runs. Its diagnostics are ShellCheck's: it is pointed at ShellCheck through
  `bashIde.shellcheckPath` (and `SHELLCHECK_PATH` for the time before the configuration arrives);
  explainshell is off. Without a runnable ShellCheck the server would publish no diagnostics, so it
  counts as available only with one.
- **ShellCheck**: the official static release 0.11.0 for macOS (`darwin.aarch64` / `darwin.x86_64`
  `.tar.gz` from github.com/koalaman/shellcheck, SHA-256 pinned), unpacked into
  `$CORTEX_HOME/toolchains/shellcheck/0.11.0/shellcheck-v0.11.0/shellcheck`, else one on PATH.
- **install**: ShellCheck (download, checksum, unpack, staged and renamed into place), then
  `npm install --prefix $CORTEX_HOME/toolchains/bash-language-server/5.8.1 bash-language-server@5.8.1`
  (never global; Node.js 20+ and npm must be present). The log says every step.
- **Toolchain for commands** (`describe`'s `path`): the installed ShellCheck's directory, so
  `shellcheck` runs in commands.
- **Verifiers** (no project marker: they apply to the scripts being checked; `{file}` is one script,
  `{files}` every script checked): `shellcheck -x {files}` (lint, requires `shellcheck`, which the
  toolchain puts on commands' PATH once installed), `bash -n {file}` (build: the syntax check,
  requires `bash`).

## Build and run

```sh
cd backend && cargo build --release && cargo test --release && cargo clippy --release --all-targets -- -D warnings
cortex-pack build plugins/lang-shell     # -> plugins/lang-shell/target/lang-shell-<version>-<target>.ccplugin
```

The frame runs it once installed (`cortex-pack build plugins/lang-shell --install`). The probe extracts a sample
through the real `extract`, reads every skill and reports whether bash-language-server and ShellCheck
can run (an absent server does not fail the probe: extraction and skills work without it).

## Verification

`verify/lang_shell.mjs` loads the folder in a real frame and checks every method against truth
computed independently: definition lines and call sites found in the fixture text, ShellCheck's own
findings (`shellcheck -f json`, run here) before and after a fix, definition, references and hover
positions of a function, skills read back with their summaries, the `lang-shell.server` event, and
refusals (another language, a relative root, a missing file). bash-language-server and ShellCheck
must be runnable; `node verify/lang_shell.mjs --install` lets `lang-shell.install` add them first.
