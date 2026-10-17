# Cortex theme (`theme-cortex`)

CortexCode's design language (`theme@1`): the tokens for both grounds (generated from `tokens/`), the bundled faces (Source Serif 4, Archivo), the base and component styles, and the derived CodeMirror and xterm themes. Swapping this plugin re-themes the running app.

One self-contained plugin folder, separable into its own repository (only `cc-protocol` and `@cortex/sdk` come from outside):

| Path | What |
|---|---|
| `plugin.toml` | identity, build commands and outputs, UI packages and their slots, shell modules, source location |
| `ui/` | one npm workspace (own `package-lock.json`, `@cortex/sdk` as a local dependency): `ui/theme` (theme@1); each package builds `dist/` with `manifest.json` |

## Build and run

```sh
cd ui && npm ci && npm run build && npm test     # after the SDK is built (cd ../../ui/sdk && npm run build)
cortex-pack build plugins/theme-cortex                  # -> plugins/theme-cortex/target/theme-cortex-<version>-<target>.ccplugin
```

`cortex-pack build plugins/theme-cortex --install` packs the folder into a `.ccplugin` that the frame installs (`frame.plugins.install`), which hot-swaps the UI package together; Settings > Plugins disables and enables the whole plugin.

## Verification

ui/verify/tokens.mjs and ui/verify/theme.mjs (in the UI core) check the tokens against the design reference and the live re-theme.
