# Terminal (`terminal`)

PTY terminals running the user's login shell: `terminal.open`, `terminal.write`, `terminal.resize`, `terminal.close`, `terminal.list`, `terminal.snapshot` (VT-parsed screen text) and `terminal.output` events. The UI package is the right pane's Terminal tab (xterm.js with WebGL).

## Terminal application compatibility (source only)

The backend now uses one pinned Ghostty terminal parser. Its optional vector fast paths and image processing
are disabled; speed and transitive license completeness remain unverified. An authorized build must
collect the pinned Unicode dependency notices into `data/licenses/GHOSTTY-THIRD-PARTY.txt` before
packaging can proceed. The generated notice stays visible to the packer and release digest.
Future authorized builds need Python 3,
Zig 0.17.0 and the existing Rust toolchain. The manifest runs `backend/native/build.py`, which
checks the source digest, builds a static library and then builds Rust. Direct Cargo commands
do not download or compile Ghostty. See [terminal-core implementation and validation limits](../../docs/TERMINAL-CORE.md).

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
cd backend && python3 native/build.py
cd ui && npm ci && npm run build && npm test     # after the SDK is built (cd ../../ui/sdk && npm run build)
cortex-pack build plugins/terminal                  # -> plugins/terminal/target/terminal-<version>-<target>.ccplugin
```

`cortex-pack build plugins/terminal --install` packs the folder into a `.ccplugin` that the frame installs (`frame.plugins.install`), which hot-swaps the backend and the UI package together; Settings > Plugins disables and enables the whole plugin.
