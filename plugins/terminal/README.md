# Terminal (`terminal`)

PTY terminals running the user's login shell: `terminal.open`, `terminal.write`, `terminal.resize`, `terminal.close`, `terminal.list`, `terminal.snapshot` (VT-parsed screen text) and `terminal.output` events. The UI package is the right pane's Terminal tab (xterm.js with WebGL).

## Terminal application compatibility (source only)

The shared `terminal@1` additions provide binary input and formatted screen restoration with byte
offsets. Both apps' terminal panes use one SDK transport for ordered input, mouse bytes, resize and
reconnect. See [shared contract and release targets](../../docs/SHARED-TERMINAL.md).
No build, install or live app verification was performed for this change.

One self-contained plugin folder, separable into its own repository (only `cc-protocol` and `@cortex/sdk` come from outside):

| Path | What |
|---|---|
| `plugin.toml` | identity, build commands and outputs, UI packages and their slots, shell modules, source location |
| `backend/` | Cargo workspace (depends on `cc-protocol` by path), binary `cc-terminal`: category `terminal`, contract `terminal@1` |
| `ui/` | one npm workspace (own `package-lock.json`, `@cortex/sdk` as a local dependency): `ui/pane` (pane.terminal@1); each package builds `dist/` with `manifest.json` |

## Build and run

```sh
cd backend && cargo build --release && cargo clippy --all-targets -- -D warnings
cd ui && npm ci && npm run build && npm test     # after the SDK is built (cd ../../ui/sdk && npm run build)
cortex-pack build plugins/terminal                  # -> plugins/terminal/target/terminal-<version>-<target>.ccplugin
```

`cortex-pack build plugins/terminal --install` packs the folder into a `.ccplugin` that the frame installs (`frame.plugins.install`), which hot-swaps the backend and the UI package together; Settings > Plugins disables and enables the whole plugin.
