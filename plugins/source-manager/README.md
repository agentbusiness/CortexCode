# Source manager (`cc-source`, contract `source@1`)

CortexCode's own source, live and editable, as one git repository in the code folder
(`~/.cortexcode/code` by default; `[frame] code` in the frame config). Design: `docs/LIVE-EDITING.md`.

- **Created and updated from the shipped source.** At the first start the folder is made from the
  source the app ships (`shipped_source`; in development, this repository). At every start the
  shipped source is merged in, keeping the user's changes; a conflict leaves the folder as it was
  and shows the files in Settings > History.
- **Data is live.** Every plugin reads its data (skills, guides, prompts, texts) from its folder in
  the code folder (the frame passes `CORTEX_PLUGIN_SOURCE`). An edited data file is in use at once;
  once it has settled it is committed, so History lists it and it can be reverted. The chat flow's
  safety policy is the exception: it is code (`paths::PROTECTED`).
- **Code needs approval.** Changed code is held in a draft commit and shown as a card with a
  plain-English summary (written by the local model): in the conversation that made it, else under
  "Waiting for you" in the top-right card. Approve: the change goes on the branch, every plugin it
  affects is built and packed (`cortex-pack`) and installed into the running frame, which probes
  the new version and rolls back a crash. A failed build takes the change back off the branch; the
  draft stays.
- **What can be built here.** A plugin's `[build] needs` (`rust`, `apple-clt`, `metal`, `node`)
  is checked on this Mac; a plugin whose needs are missing is data-only (Settings > Plugins says what
  is missing).
- **History, backup, sharing.** Every change can be reverted; the folder is pushed to a repository
  the user picked on GitHub (branch `cortexcode`) and restored from it on a new Mac; picked changes
  are sent to the official repository as a pull request after a privacy check. The GitHub connection
  is the vcs plugin's (`vcs.github.*`).

Git runs as a library (`git2`, libgit2 compiled in): no git program is needed on the user's Mac.

| Path | What |
|---|---|
| `backend/` | Cargo workspace, binary `cc-source` (category `source`, contract `source@1`) |
| `backend/data/summary.md` | instructions for the change summary (data: live-editable) |
| `ui/card-waiting` | "Waiting for you" in the top-right card (`card.waiting@1`) |
| `ui/settings-*` | Settings > History, Backup & Sync, Share, Storage |

## Configuration

```toml
[plugin.source-manager.config]
shipped_source = "."                                 # the source the code folder is made from
packer = "packer/target/release/cortex-pack"          # builds and packs approved changes
# official_repo = "agentbusiness/CortexCode"         # where improvements are shared
```

Relative paths are relative to the frame config's folder.

## Methods (`source@1`)

`source.status`, `source.update`, `source.history`, `source.revert`, `source.diff`,
`source.proposals`, `source.propose`, `source.decide`, `source.backup.status|set|push|restore`,
`source.share.candidates|send|status`, `source.storage`; topics `source.changed` and
`source.progress`. The contract and its types are in `protocol/src/contracts/source.rs`.

## Run

```sh
packer/target/release/cortex-pack build plugins/source-manager --install
```

The probe creates a code folder from a small shipped source in a temporary folder, commits a data
edit, merges an update of the shipped source and reverts the edit.
