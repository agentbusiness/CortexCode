# Settings host (`settings-host`)

The settings window (`settings.host@1`): every `settings.<name>@1` plugin is a panel.

One self-contained plugin folder, separable into its own repository (only `cc-protocol` and `@cortex/sdk` come from outside):

| Path | What |
|---|---|
| `plugin.toml` | identity, build commands and outputs, UI packages and their slots, shell modules, source location |
| `ui/` | one npm workspace (own `package-lock.json`, `@cortex/sdk` as a local dependency): `ui/host` (settings.host@1), which builds `dist/` with `manifest.json` |

## Build and run

```sh
cd ui && npm ci && npm run build && npm test     # after the SDK is built (cd ../../ui/sdk && npm run build)
cortex-pack build plugins/settings-host                  # -> plugins/settings-host/target/settings-host-<version>-<target>.ccplugin
```

`cortex-pack build plugins/settings-host --install` packs the folder into a `.ccplugin` that the frame installs (`frame.plugins.install`), which hot-swaps the UI packages together; Settings > Plugins disables and enables the whole plugin.
