# Updates (`cc-updater`, contract `update@1`)

Updates from the official releases (`docs/RELEASES.md`).

- **The catalog.** Every hour (and 30 s after the start) the updater downloads `catalog.json` from
  the release tagged `catalog` of the official repository. Nothing else decides; every downloaded
  file is checked against the size and SHA-256 the catalog gives.
- **Plugins the user did not change** (their folder in the live folder matches their package's
  data, `source.release.status`) install by themselves when no run is going (`chatflow.running`):
  the package's data is merged into the live folder first (`source.release.merge`, plugins read
  their data there), then the package is installed (`frame.plugins.install_many`: all of them or
  none; one by one when the set fails). A plugin new to this machine is installed on its own.
- **No catalog** (none published yet, offline): the packages the app itself came with stand in
  (`frame.plugins.bundled`), so every plugin still goes to the latest build by itself.
- **Plugins the user changed** ask first: the popup at the bottom right (`updater.band`), in plain
  words. *Use the newer version* drops the user's edits to the plugin's data
  (`source.release.reset`) and installs the package. *Keep my version* stays until the plugin's
  next version. (`update.decide` also takes `merge`: the release's data merged into the user's
  edits; a clash changes nothing and says which files.)
- **A version that cannot start** (the frame's check refused it; the previous version keeps
  running) is recorded (`failed`), not tried again until a newer version comes, and shown with its
  reason in the popup until seen (`update.dismiss`) and in Settings > Plugins.
- **The user's own builds** (`+dev.` versions from `cortex-pack build --install`) are left alone;
  Settings > Plugins names them, and the reset replaces them.
- **The frame.** A newer frame (with its window code, and Electron when that changed) is downloaded
  and staged (`frame.update.stage`); the band says *Restart to update* (`frame.update.restart`).
- A plugin built with a newer frame than the one running (`min_frame`) waits for the frame update.
- **Reset all plugins** (Settings > Plugins, after a confirmation that lists what it does to each
  plugin): every installed plugin back to its official release. The user's edits to the plugin's
  data in the live folder are dropped (`source.release.reset`), then the release package is
  installed (`frame.plugins.install_many`); a plugin already running the release only loses the
  changes. When the official releases cannot be read (no catalog published, offline), the packages
  the app itself carries stand in (`frame.plugins.bundled`): the reset then goes back to the build
  the app came with. Plugins in neither, and releases that need a newer frame, are left as they
  are; the frame is not touched. Refused while a run is going. `update.reset_plan` is the list the
  confirmation shows; `update.reset` does it.

## Configuration

```toml
[plugin.updater.config]
enabled = true                      # false: never look
repo = "agentbusiness/CortexCode"   # or catalog_url = "https://..."
check_every_s = 3600
```

## Methods (`update@1`)

`update.status`, `update.check`, `update.decide {plugin, version, choice: merge|keep}`,
`update.restart`, `update.dismiss {plugin}`, `update.reset_plan`, `update.reset`; topic `update.changed`. Types: `protocol/src/contracts/update.rs`.

| Path | What |
|---|---|
| `backend/` | Cargo workspace, binary `cc-updater` |
| `ui/band` | the band line and the popup (`workbench.band@1`) |
