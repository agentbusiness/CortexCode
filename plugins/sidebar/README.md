# Sidebar (`sidebar`)

The workbench sidebar (`workbench.sidebar@1`): projects with their conversations and agents, live from the frame (a session with a notification CortexCode showed only in the app — its window was in front, or macOS did not show it — carries a mark until it is opened; read from `notify.history`, cleared by cc-notify on `notify.changed`), and the actions it hosts (`sidebar.actions@1`): Open project (the native folder dialog, then `frame.project.open`) and New chat.

One self-contained plugin folder, separable into its own repository (only `cc-protocol` and `@cortex/sdk` come from outside):

| Path | What |
|---|---|
| `plugin.toml` | identity, build commands and outputs, UI packages and their slots, shell modules, source location |
| `ui/` | one npm workspace (own `package-lock.json`, `@cortex/sdk` as a local dependency): `ui/new-chat` (sidebar.actions@1), `ui/open-project` (sidebar.actions@1), `ui/sidebar` (workbench.sidebar@1); each package builds `dist/` with `manifest.json` |

## Build and run

```sh
cd ui && npm ci && npm run build && npm test     # after the SDK is built (cd ../../ui/sdk && npm run build)
cortex-pack build plugins/sidebar                  # -> plugins/sidebar/target/sidebar-<version>-<target>.ccplugin
```

`cortex-pack build plugins/sidebar --install` packs the folder into a `.ccplugin` that the frame installs (`frame.plugins.install`), which hot-swaps the UI packages together; Settings > Plugins disables and enables the whole plugin.
