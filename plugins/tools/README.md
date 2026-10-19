# Tools (`tools`)

The model's hands: `read` / `edit` / `write` with hashline anchors (`hashedit`, `textedit`), search,
URI reads (`graph://`, `task://`, `knowledge://`, … through the memory plugin; the project's
knowledge bound to a symbol or a file rides along with its read, one line per item), a persistent shell with background
jobs and output offloading to artifacts, copy-on-write workspaces for parallel agents, the UI file
tree (`tools.files`, `tools.save`), tool-call repair for the shapes local models actually emit
(`repair/`: tolerant JSON, XML-style calls, other harnesses' tool names), and the ask-mode gate
(`tools.approve`). Agent commands run under `sandbox-exec` with a write allowlist (the workspace,
CORTEX_HOME, temp, package caches) and, for Python projects without their own environment, an
automatic per-workspace `uv` venv under `CORTEX_HOME/venvs` (`pyenv`), so nothing reaches the
system Python or Homebrew.

One self-contained plugin folder, separable into its own repository (only `cc-protocol` comes from
outside):

| Path | What |
|---|---|
| `plugin.toml` | identity, build command and output, source location, verification commands |
| `backend/` | Cargo workspace (depends on `cc-protocol` by path), binary `cc-tools`: category `tools`, contract `tools@1` |
| `verify/` | gate scripts, run against the real frame (plus fixtures and their results) |

The plugin has no UI package of its own: the file tree and editor are the code-view plugin's, which
call `tools.files` and `tools.save`.

## Build and run

```sh
cd backend && cargo build --release && cargo test --release && cargo clippy --all-targets -- -D warnings
cortex-pack build plugins/tools        # -> plugins/tools/target/tools-<version>-<target>.ccplugin
```

The frame runs it once installed (`cortex-pack build plugins/tools --install`); the frame config's
`[plugin.config] cx_path` names the `cx` CLI that agent commands call.

Reads of another project (`graph://<project>/...`, `symbol://<project>/<name>`,
`outline://<project>/<path>`; `src/xproject.rs`) wait for that project's index at most
`[plugin.config] reference_wait_ms` (default 5000); a project still being indexed after that answers
at once with its progress (`<name> is still being indexed (N of M files). Its files can be read now: <name>:<path>.`, from
`memory.project.status`) instead of a query that would wait for the whole index. Language-server
reads (`definition://`, `references://`, `hover://`, `diagnostics://<project>:<path>`) and files
(`<project>:<path>`) do not need the index.

## Skills by diagnostic code

Plugins that own skills declare the exact diagnostic codes each skill covers
(`data/skills/codes.toml`, served as `SkillInfo.codes` by `describe`). `src/codeskills.rs` attaches
the skill to a tool result that shows one of them, so the model gets the borrow-checker skill when
it hits E0382 instead of having to pick it:

- **after an edit** and on a **`diagnostics://` read**: the structured `code` of each error or
  warning listed, reported by the language plugin that covers the file;
- on a **`run`** whose script contains a simple command that is one of the language plugins'
  verifier commands (parsed with tree-sitter-bash; the verifier's words as a prefix, `{file}` one
  word, `{files}` one or more; redirections, pipes and lists around it do not matter): a declared code
  of that plugin appearing as a whole token of the full output (`error[E0382]`, `TS2322:`,
  `[-Wsign-compare]`). Other commands' output is not scanned.

A code is looked up in the reporting plugin's skills, then in the skills plugins'. The first time in
a session the skill's text comes inline, before the result, between `[skill for E0382]` and
`[end of skill]` (its URI goes to the result's `display.skills`); later results carry nothing for it.
At most one skill's text per result (others get a pointer, `[skill for E0502: skill://...]`, and keep
their first inline for later). The set of skills a session got lives with the per-session read state
(`readstate.rs`) and is handed to a successor on a hot swap. A skill whose text reached the session
another way (the chat flow's injected skills) is recorded there with `tools.skills.shown {session_id,
uris}`, so a code that leads to it later brings nothing.

The scratch folder is `tmp/` at the workspace root (`$TMPDIR` of its shells), listed in the project's
`.gitignore` while it exists. When a turn ends the chat flow calls `tools.cleanup {workspace,
session_id, since_ms}` (`cleanup.rs`): `tmp/` goes, the added `.gitignore` line is taken back, and
new untracked files the turn's commands made (build output, logs) are removed; files `edit` wrote,
ignored files, older files and `.cortex-*` stay, and nothing else is removed when the user let a
command past the sandbox in the session. A file `edit` created in the running turn may be written
whole again (the model's own draft); the list is cleared when the turn ends. A `.cortex-env` file (`KEY=VALUE` lines, `$NAME`
expanded) in the workspace or above it is loaded into its shells and jobs: a toolchain the machine
does not have on its PATH.

A host `tools.read` of `diagnostics://` may pass `wait_ms` (0: what the servers have now, nothing
started or waited for, marked partial; the chat flow's reads) and `shown` (at most that many listed
per server, errors first, the rest counted). A server that cannot answer is a line of the result.

## Verification

Each `[verify]` entry of `plugin.toml`, run from this folder (binaries come from `CORTEX_FRAME_BIN`,
`CORTEX_TOOLS_BIN`, `CORTEX_CX_BIN`, `CORTEX_MEMORY_BIN`, else the repository layout):

- `verify/hashline_suite.mjs`: anchored edits against an independent hashline implementation.
- `verify/repair_corpus.mjs`: real malformed calls recorded from Qwen models are repaired or refused without side effects.
- `verify/run.mjs`: shell, background jobs, output offloading, cancellation.
- `verify/workspace.mjs`: copy-on-write workspaces.
- `verify/uris_cx.mjs`: URI reads and the `cx` CLI against the real memory plugin.
- `../../scripts/verify-sandbox.mjs`: the write sandbox and the per-workspace venv.
