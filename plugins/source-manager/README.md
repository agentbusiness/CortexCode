# Source manager (`cc-source`, contract `source@1`)

Every plugin's data (prompts, texts, stages, schemas, skills), live and editable, as one git
repository in the live folder (`~/.cortexcode/live` by default; `[frame] live` in the frame config).
CortexCode's code is not here and is never edited by the app: it changes only with a new package.
Design: `docs/LIVE-EDITING.md`.

- **Made from the installed packages.** A package carries its data folders (`[data] dirs` in its
  plugin.toml, e.g. `data`, `backend/data`) at the same paths; the live folder holds them as
  `plugins/<id>/<dir>/...`, the layout of CortexCode's repository. At every start, and whenever a
  plugin is installed, rolled back or switched (`plugin.unit`), the installed packages' data is
  merged in, keeping the user's edits; a conflict leaves the folder as it was and shows the files
  in Settings > History.
- **Releases.** The updater merges a release package's data the same way before it installs it
  (`source.release.merge`), and asks `source.release.status` whether the user edited a plugin's
  data (then the update asks first). A reset (`source.release.reset`, from Settings > Plugins >
  Reset all plugins) puts one plugin's folder back to its package's data, dropping the user's edits
  in it, committed or not; the rest of the live folder, uncommitted edits included, stays.
- **Data is live.** Every plugin reads its data from its folder in the live folder (the frame
  passes `CORTEX_PLUGIN_LIVE`; safe mode leaves it out, so the packaged copy serves). An edited
  file is in use at once; once it has settled it is committed, so History lists it and it can be
  reverted. The chat flow's safety policy (`prompts/policy.md`) is read from its package only.
- **The record is the graph** (`docs/GRAPH-MODEL.md`, `backend/src/record.rs`): every change of the
  branch puts the files it changed as `(:DataFile {key: "<plugin>/<path>", plugin, path,
  shipped_hash, body*, hash*, origin*})-[:OF]->(:Plugin)-[:ON]->(:Machine)`, the temporal values
  sampled at the change's time, and appends one record to the stream `source.changes` (`{at,
  commit, origin, summary, plugins, files, reverts}`, kept 10 years). The watch loop records each
  commit right after it makes it (in the background: a memory plugin that does not serve never
  holds the loop), and every other change (an update, a reset, a revert, a restore) at its next
  look; `LiveFolder.recorded_commit` says how far the record goes. At the first start of this
  version the branch's whole first-parent history is replayed, oldest first (the shipped root
  commit, dated 1970, just before the first change after it: samples must be newer than the graph's
  10-year history retention and must only grow). History (`source.history`) reads the stream;
  `source.file.history` lists a file's versions (`HISTORY f.hash`, `f.origin`) and
  `source.file.restore` writes a file back as the graph had it at a time (`AT TIME … f.body`;
  a binary file's content comes from git's objects by its hash), committed as a `restore`.
- **Git** stays in the folder as the transport to GitHub (backup and sharing are git pushes and
  pull requests) and as the three-way merge of releases, resets and reverts; the record of what
  changed and every version's text is the graph.
- **History, backup, sharing.** Every change can be reverted; the folder is pushed to a repository
  the user picked on GitHub (branch `cortexcode`) and restored from it on a new Mac; picked changes
  are sent to the official repository as a pull request after a privacy check. The GitHub connection
  is the vcs plugin's (`vcs.github.*`).
- **The code folder of earlier versions** (`~/.cortexcode/code`, CortexCode's full source): at the
  first start of this version the user's data edits in it are carried into the live folder (one
  commit, "Carried over your edits from the code folder"), then it is removed. If that fails it
  stays as it was.

Git runs as a library (`git2`, libgit2 compiled in): no git program is needed on the user's Mac.

| Path | What |
|---|---|
| `backend/` | Cargo workspace, binary `cc-source` (category `source`, contract `source@1`) |
| `ui/settings-*` | Settings > History, Backup & Sync, Share, Storage |

## Configuration

```toml
[plugin.source-manager.config]
# official_repo = "agentbusiness/CortexCode"         # where improvements are shared
```

## Methods (`source@1`)

`source.status`, `source.update`, `source.history`, `source.revert`, `source.diff`,
`source.backup.status|set|push|restore`, `source.share.candidates|send|status`, `source.storage`,
`source.release.status|merge|reset`, `source.file.history|restore`; topic `source.changed`. The contract and its types are in
`protocol/src/contracts/source.rs`.

## Run

```sh
packer/target/release/cortex-pack build plugins/source-manager --install
```

The probe creates a live folder from a small package in a temporary folder, commits an edit,
merges a newer package keeping the edit, and reverts the edit.
