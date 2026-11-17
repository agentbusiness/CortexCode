# Runner (`runner`)

A CortexCode that builds for a manager (CortexManage). The manager reaches it through `mesh.link`
and calls `runner.*`; the model never does. Design: `docs/RUNNER.md`; the manager's side:
CortexManage `docs/specs/01-WORK.md`.

A job is one worktree of the source's mirror and one chat session that carries out a finished
plan, one task after the other. The runner commits after each task that ended done, counts the
run's facts from the session's journal and the worktree, and sends them as `runner.beat`.

| Path | What |
|---|---|
| `plugin.toml` | identity, build command and output, data |
| `backend/` | Cargo workspace (depends on `cc-protocol` by path), binary `cc-runner`: category `runner`, contract `runner@1` |
| `backend/data/texts.toml` | the "now" line's words and the refusals (the manager reads them, never the model) |
| `backend/src/service.rs` | jobs, the journal, facts, beats, checkpoints, the job's methods |
| `backend/src/track.rs` | a session's journal, counted |
| `backend/src/work.rs` | diff, build, command, land, release, reads of the mirror, the probe |
| `backend/src/store.rs` | the records in the graph (`RunnerRepo`, `RunnerJob`, `RunnerTask`, `RunnerCheckpoint`) |
| `backend/src/process.rs` | running a build, a command or a release script |

## Folders

```
<CORTEX_HOME>/work/<repo>/
  mirror.git/     the source: its branches are refs/remotes/origin/*, the jobs' are refs/heads/*
  <job id>/       one worktree per job
  release/        the clean checkout releases run in (branch runner/release)
```

## Build

```sh
cd backend && cargo build --release && cargo clippy --all-targets -- -D warnings
cortex-pack build plugins/runner --install        # into the runner's home (CORTEX_HOME)
```

The category `runner` needs a frame that knows it (`CATEGORIES` in the protocol), a git plugin
with the runner's verbs (`vcs.mirror`, `vcs.worktree.*`, ...) and a chat flow with `plan.put` and
runner mode.
