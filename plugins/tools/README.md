# Tools (`tools`)

The model's hands: `read` / `edit` / `write` with hashline anchors (`hashedit`, `textedit`), search,
URI reads (`graph://`, `task://`, `knowledge://`, … through the memory plugin; the project's
knowledge bound to a symbol or a file rides along with its read, one line per item), a persistent shell with background
jobs and output offloading to artifacts, the step log (what every step changed, for undo), one
tree for every session (file reservations, checks on a throw-away copy), the UI file
tree (`tools.files`, `tools.save`), tool-call repair for the shapes local models actually emit
(`repair/`: tolerant JSON, XML-style calls, other harnesses' tool names), and the ask-mode gate
(`tools.approve`). Agent commands run under `sandbox-exec` with a write allowlist (the workspace,
CORTEX_HOME, temp, package caches) and, for Python projects without their own environment, an
automatic per-workspace `uv` venv under `CORTEX_HOME/venvs` (`pyenv`), so nothing reaches the
system Python or Homebrew.

A user's request to commit or push authorizes the ordinary Git steps, including staging; there is
no additional approval prompt in Auto or Ask mode. Destructive Git operations (discarding work,
deleting refs, rewriting history, force pushes) still need a one-time confirmation in either mode.
`approval::git_destructive` inspects every command in a shell script before any of it runs. Plain
pushes use `vcs.push`, including the GitHub connection from Settings. This is a source change for
the next tools release after 0.1.18; it requires installing that release to affect the running app.

One self-contained plugin folder, separable into its own repository (only `cc-protocol` comes from
outside):

| Path | What |
|---|---|
| `plugin.toml` | identity, build command and output, source location, verification commands |
| `backend/` | Cargo workspace (depends on `cc-protocol` by path), binary `cc-tools`: category `tools`, contract `tools@1` |

The file tree and editor are the code-view plugin's, which call `tools.files` and `tools.save`. The
plugin's own UI packages are two rows of the project card: `card.timeline@1` (the step log, below)
and `card.jobs@1` (the running background jobs, under the model: command, age, memory, Stop and
Kill; the job's output then ends saying the user stopped it).

## Build and run

```sh
cd backend && cargo build --release && cargo clippy --all-targets -- -D warnings
cortex-pack build plugins/tools        # -> plugins/tools/target/tools-<version>-<target>.ccplugin
```

The frame runs it once installed (`cortex-pack build plugins/tools --install`). The backend
workspace also builds `cx` (`backend/cx`), the command line the model's shells use, which ships
beside `cc-tools` and is put on the shells' PATH (`[plugin.config] cx_path` or `CORTEX_CX_BIN` name
another one when the package has none). The methods `cx` may call from a shell are each plugin's
`commands` in its hello; this plugin's are its session's background jobs.

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

The scratch folder is `tmp/` at the workspace root (`$TMPDIR` in its shells' commands, not exported: the programs they start use the system's temp folder, so their temp files never sit inside the project), listed in the project's
`.gitignore` while it exists. When a turn ends the chat flow calls `tools.cleanup {workspace,
session_id, since_ms}` (`cleanup.rs`): `tmp/` goes, the added `.gitignore` line is taken back, and
new untracked files the turn's commands made (build output, logs) are removed; files `edit` wrote,
ignored files, older files and `.cortex-*` stay, and nothing else is removed when the user let a
command past the sandbox in the session. A file `edit` created in the running turn may be written
whole again (the model's own draft); the list is cleared when the turn ends. A `.cortex-env` file (`KEY=VALUE` lines, `$NAME`
expanded) in the workspace or above it is loaded into its shells and jobs: a toolchain the machine
does not have on its PATH.

## The machine guard

`guard.rs` measures every run's processes every two seconds (each session shell's process group,
each background job's group, and their descendants, also ones that left the group) and stops a run
that would take the machine down or that nobody runs any more; it never stops work for how long it
runs. A stopped run's result ends with the host's note (why, then what to fix), so the model fixes
the code instead of running it again:

- **Memory runaway:** a run that holds more memory than the machine has (it lives only in swap); one
  that holds more than half of it while it grows or while macOS reports memory pressure; one
  that grows and holds the most while macOS reports memory pressure; at critical pressure, the run
  that holds the most. Growing: up by 5% (at least 256 MB) over 30 s.
- **Hung job:** a background job that printed nothing, used no CPU and served no TCP port for an
  hour.
- **Repeat:** starting the same command again (same session and folder; `command_head` compares
  the first real step without its redirections) stops the earlier background job; the new result
  says so.
- **Left behind:** what a job's command left running when it ended (`server &` inside a job) is
  stopped with its note.

A timeout names what the command was really running when it was killed (the test binary, not the
build), so a hanging test does not read as a slow build.

## The step log and undo

Every step that changes files is a record in the workspace's step log (`undo.rs`), kept until the
next commit: the files it changed, their contents before and after (compressed blobs, shared
between records), who did it (`user`, `session:<id>`, `host`) and what for. Records come from the
edit tool and the editor's save (with the text in hand), from every command's end, the turn's
cleanup and a quiet moment after a file-system event (`git status` compared with what the log
knows, so a `sed`, a formatter or an edit by hand is a record too), and from the log's opening (what
changed while the app was off). On `vcs.changed` with a new HEAD, every path whose content equals
HEAD leaves the log, so a commit empties it and a partial commit keeps what stays uncommitted.
Undo and redo are records too: `tools.undo {workspace, seqs | to_seq, cascade}` puts the
before-contents back (a step whose files a later step touched is refused with `blocked_by` unless
`cascade`), `tools.redo` undoes the newest active undo; `tools.timeline` lists the records (newest
first, filtered by session or time; it records what changed on disk first), `tools.timeline.diff`
gives one path's change over a range of steps. The event `tools.timeline {workspace, seq}` says the
log changed. The card's Timeline section (`ui/card-timeline`) and the chat's Changed card read it;
the model uses `cx changed`, `cx undo`, `cx redo`. A workspace that is not a repository is walked
with the ignore rules instead and has its files recorded once when the log opens.

## One tree, many hands

Every session, chat or agent, edits the project's own folder (`tree.rs`). A path the edit tool
writes is reserved for that session until its turn ends (`tools.cleanup` releases it); another
session's edit of it is refused with who holds it and when it frees, nothing written. A command
that is one of the project's checks (its first words are a verifier command the language plugins
declare) runs on a `clonefile` copy of the tree when another session holds a reservation or runs a
command in it: the copy drops what git ignores, links the caches the language plugins declare
(`Description.caches`: `target`, `node_modules`, `.venv`) to the project's own, hosts the command
with the session's shell and sandbox (the copy added as a writable grant), and goes when the
command ends; the result says so and that files the command changed were not kept. Alone in the
tree, a check runs in the tree.

A host `tools.read` of `diagnostics://` may pass `wait_ms` (0: what the servers have now, nothing
started or waited for, marked partial; the chat flow's reads) and `shown` (at most that many listed
per server, errors first, the rest counted). A server that cannot answer is a line of the result.
