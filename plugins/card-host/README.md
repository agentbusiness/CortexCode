# Card host (`card-host`)

The card, at the right of the brand band (`workbench.band@1`): a toggle with a live summary (and Open pane while the right pane is collapsed) and the card it opens, which hosts the card header (`card.header@1`, the project and its menu, also in this plugin), every `card.<name>@1` section other plugins add, and the settings window host (`settings.host@1`).

One self-contained plugin folder, separable into its own repository (only `cc-protocol` and `@cortex/sdk` come from outside):

| Path | What |
|---|---|
| `plugin.toml` | identity, build commands and outputs, UI packages and their slots, shell modules, source location |
| `ui/` | one npm workspace (own `package-lock.json`, `@cortex/sdk` as a local dependency): `ui/card` (workbench.band@1), `ui/project` (card.header@1); each package builds `dist/` with `manifest.json` |

## Build and run

```sh
cd ui && npm ci && npm run build && npm test     # after the SDK is built (cd ../../ui/sdk && npm run build)
cortex-pack build plugins/card-host                  # -> plugins/card-host/target/card-host-<version>-<target>.ccplugin
```

`cortex-pack build plugins/card-host --install` packs the folder into a `.ccplugin` that the frame installs (`frame.plugins.install`), which hot-swaps the UI packages together; Settings > Plugins disables and enables the whole plugin.
