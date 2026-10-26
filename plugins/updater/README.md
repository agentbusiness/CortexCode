# Updates (`cc-updater`, contract `update@1`)

Updates from the official releases (`docs/RELEASES.md`).

- **The catalog.** Every hour (and 30 s after the start) the updater downloads `catalog.json` from
  the release tagged `catalog` of the official repository. Nothing else decides; every downloaded
  file is checked against the size and SHA-256 the catalog gives.
- **Plugins the user did not change** (their folder in the code folder matches the shipped source,
  `source.release.status`) install by themselves when no run is going (`chatflow.running`): the
  release's source is merged into the code folder first (`source.release.merge`, plugins read their
  live data there), then the package is installed (`frame.plugins.install_many`: all of them or
  none; one by one when the set fails). A plugin new to this machine is installed on its own.
- **Plugins the user changed** ask first: the popup at the bottom right (`updater.band`). *Merge
  update* merges the release into the user's version; when the user changed its code it is built
  here, otherwise the release package is installed. *Keep mine* stays on the user's version until
  the plugin's next release. A merge that clashes changes nothing and says which files.
- **The frame.** A newer frame (with its window code, and Electron when that changed) is downloaded
  and staged (`frame.update.stage`); the band says *Restart to update* (`frame.update.restart`).
  The frame's source (everything outside `plugins/`) is merged into the code folder.
- A plugin built with a newer frame than the one running (`min_frame`) waits for the frame update.

## Configuration

```toml
[plugin.updater.config]
enabled = true                      # false: never look
repo = "agentbusiness/CortexCode"   # or catalog_url = "https://..."
check_every_s = 3600
```

## Methods (`update@1`)

`update.status`, `update.check`, `update.decide {plugin, version, choice: merge|keep}`,
`update.restart`; topic `update.changed`. Types: `protocol/src/contracts/update.rs`.

| Path | What |
|---|---|
| `backend/` | Cargo workspace, binary `cc-updater` |
| `ui/band` | the band line and the popup (`workbench.band@1`) |
