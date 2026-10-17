# @cortex/sdk

The one package a CortexCode UI plugin imports. It is served to the workbench once, as a shared ES
module (`/ui/vendor/cortex-sdk.js` through the import map), so the host and every plugin version
share one frame connection, one journal cache, one query cache and one durable state store.

- **Frame client** (`useClient`, `useQuery`, `useEvent`): CPP/1 over the frame's WebSocket.
- **Journal** (`useJournal`): a session materialized from its patches.
- **Durable slot state** (`useSlotState`, `useShared`): survives hot-swaps and restarts.
- **Host** (`useHost`): render other slots (`Slot`, `Slots`), the shell bridge, notifications.
- **Slot contracts** (`CORE_SLOTS`, `HOST_SLOTS`, `SharedState`, the code view's `code.*` state).
- **Design marks** (`Cross`, `Caret`, `Menu`, `DiffView`): the design's grammar.
- **Testing** (`installTestRuntime`, `TestHost`): a fake frame and host for plugin tests.
- **Build preset** (`@cortex/sdk/vite`): `export default pluginConfig(import.meta.dirname)` builds a
  plugin package into `dist/` with `manifest.json` `{id, version, slot, entry, css, build}`.
- **TypeScript preset**: `"extends": "@cortex/sdk/tsconfig.plugin.json"`.

A plugin package declares its identity in package.json:

```json
{ "name": "@cortex-ui/terminal-pane", "version": "0.1.0",
  "cortex": { "id": "terminal.pane", "slot": "pane.terminal@1" },
  "peerDependencies": { "react": "^19.2.0", "react-dom": "^19.2.0", "@cortex/sdk": "^0.1.0" } }
```

React, ReactDOM, the JSX runtime and this SDK are never bundled into a plugin: the workbench
provides them.
