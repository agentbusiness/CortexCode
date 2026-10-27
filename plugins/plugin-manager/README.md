# Plugin manager (`plugin-manager`)

Settings > Plugins (`settings.plugins@1`): every installed plugin as one unit (from `frame.plugins.list` `units`) with its package version (shown without its build tag, which says where it came from: a release, the app's build, or a build of yours), Disable / Enable (`frame.plugins.disable` / `enable`) and, after an install, Roll back (`frame.plugins.rollback {plugin}`); every service with Restart (`frame.plugins.swap`, the same version) and the swap's steps; every UI package with Reload; at the top, Reset all plugins (`update.reset_plan` lists what it does to each plugin in a confirmation sheet, `update.reset` does it: every plugin back to its official release, the user's changes dropped; the panel's own plugin is reinstalled too, so the sheet keeps its start time in the slot's state and reads the outcome back from `update.status`). Also, in the brand band (`workbench.band@1`), a service that is not running (crashed, restarting, missing; a disabled plugin's service is left out): nothing while every service runs, so a problem is never silent and the band stays empty when all is well; pressing it opens Settings > Plugins.

One self-contained plugin folder, separable into its own repository (only `cc-protocol` and `@cortex/sdk` come from outside):

| Path | What |
|---|---|
| `plugin.toml` | identity, build commands and outputs, UI packages and their slots, shell modules, source location |
| `ui/` | one npm workspace (own `package-lock.json`, `@cortex/sdk` as a local dependency): `ui/band-status` (workbench.band@1), `ui/settings-plugins` (settings.plugins@1); each package builds `dist/` with `manifest.json` |

## Build and run

```sh
cd ui && npm ci && npm run build && npm test     # after the SDK is built (cd ../../ui/sdk && npm run build)
cortex-pack build plugins/plugin-manager --install       # a dev build, installed into the running app
```

The frame runs plugins only from installed packages; `--install` (or `cx plugin install <file>`) hot-swaps the UI packages together; Settings > Plugins disables and enables the whole plugin.
