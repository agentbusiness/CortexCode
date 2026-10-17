# Workbench (`workbench`)

The workbench's right pane host (`workbench.rightPane@1`: one view at a time from every `pane.<name>@1` plugin, as tabs) the brand band's connection mark (`workbench.band@1`: nothing while connected; "Connecting to the frame" or "Frame unreachable" in rubric otherwise), and Settings > Appearance (`settings.appearance@1`: the ground, Plate, Auto or Page).

One self-contained plugin folder, separable into its own repository (only `cc-protocol` and `@cortex/sdk` come from outside):

| Path | What |
|---|---|
| `plugin.toml` | identity, build commands and outputs, UI packages and their slots, shell modules, source location |
| `ui/` | one npm workspace (own `package-lock.json`, `@cortex/sdk` as a local dependency): `ui/band-connection` (workbench.band@1), `ui/right-pane` (workbench.rightPane@1), `ui/settings-appearance` (settings.appearance@1); each package builds `dist/` with `manifest.json` |

## Build and run

```sh
cd ui && npm ci && npm run build && npm test     # after the SDK is built (cd ../../ui/sdk && npm run build)
cortex-pack build plugins/workbench                  # -> plugins/workbench/target/workbench-<version>-<target>.ccplugin
```

`cortex-pack build plugins/workbench --install` packs the folder into a `.ccplugin` that the frame installs (`frame.plugins.install`), which hot-swaps the UI packages together; Settings > Plugins disables and enables the whole plugin.
