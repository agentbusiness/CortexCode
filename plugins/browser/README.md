# Browser (`browser`)

The agent browser (`browser@1`). Its backend is the Electron shell module in `shell/`: it serves `browser@1` over its own frame connection from Electron's main process, with WebContentsView tabs driven through the Chrome DevTools Protocol on a persistent profile in CORTEX_HOME, laid over the right pane's Browser view (`ui/pane`, `pane.browser@1`).

One self-contained plugin folder, separable into its own repository (only `cc-protocol` and `@cortex/sdk` come from outside):

## Rendered evidence for workers

`browser.view` captures fresh pixels of a command session's own tab, optionally restricted to a visible element. The worker-facing `view_page` MCP tool returns the image itself; `browser.screenshot` continues to return a saved PNG file. View images are JPEGs bounded to 1536 pixels per side and 2 MiB, with graph-linked artifact metadata and the original crop coordinates. The operation accepts no input file path and checks document/viewport/element movement around capture. Offscreen elements require normal browser commands and a fresh snapshot.

Source added 21 November 2026; not built, installed or run by this work. See [the worker guide](../../docs/WORKER-VISUAL-EVIDENCE.md) for ownership, client requirements and validation limits.

## Files

| Path | What |
|---|---|
| `plugin.toml` | identity, build commands and outputs, UI packages and their slots, shell modules, source location |
| `ui/` | one npm workspace (own `package-lock.json`, `@cortex/sdk` as a local dependency): `ui/pane` (pane.browser@1); each package builds `dist/` with `manifest.json` |
| `shell/` | Electron main-process module `browser.shell-module` (serves `browser`); no build step |

## Build and run

```sh
cd ui && npm ci && npm run build && npm test     # after the SDK is built (cd ../../ui/sdk && npm run build)
cortex-pack build plugins/browser                  # -> plugins/browser/target/browser-<version>-<target>.ccplugin
```

`cortex-pack build plugins/browser --install` packs the folder into a `.ccplugin` that the frame installs (`frame.plugins.install`), which hot-swaps the UI package and shell module together; Settings > Plugins disables and enables the whole plugin.
