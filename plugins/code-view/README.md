# Code view (`code-view`)

The right pane's code view: the host (`pane.code@1`) lays out the toolbar (tree choice, scope, compare base, totals), the CodeMirror 6 editor with the inline unified diff, attributions, per-hunk accept/reject and the code graph, and the file tree with change counts.

One self-contained plugin folder, separable into its own repository (only `cc-protocol` and `@cortex/sdk` come from outside):

| Path | What |
|---|---|
| `plugin.toml` | identity, build commands and outputs, UI packages and their slots, shell modules, source location |
| `ui/` | one npm workspace (own `package-lock.json`, `@cortex/sdk` as a local dependency): `ui/editor` (code.editor@1), `ui/host` (pane.code@1), `ui/toolbar` (code.toolbar@1), `ui/tree` (code.tree@1); each package builds `dist/` with `manifest.json` |

## Build and run

```sh
cd ui && npm ci && npm run build && npm test     # after the SDK is built (cd ../../ui/sdk && npm run build)
cortex-pack build plugins/code-view                  # -> plugins/code-view/target/code-view-<version>-<target>.ccplugin
```

`cortex-pack build plugins/code-view --install` packs the folder into a `.ccplugin` that the frame installs (`frame.plugins.install`), which hot-swaps the UI packages together; Settings > Plugins disables and enables the whole plugin.
