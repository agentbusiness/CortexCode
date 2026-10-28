# The CortexCode shell (Electron main process)

The frame starts it as `ui/node_modules/.bin/electron ui` with `CORTEX_FRAME_URL` and
`CORTEX_FRAME_TOKEN` (without them it reads `<CORTEX_HOME>/run/frame.json`). It opens one window
on `${CORTEX_FRAME_URL}/ui/?token=…`, which the frame serves from the window package (`window@1`,
the workbench plugin's `ui/window`).

- `main.js`: the window, the preload's IPC (`cortex:pick-folder` (with an optional title), `cortex:pick-file`,
  `cortex:save-attachment` (pasted bytes into `<CORTEX_HOME>/attachments/incoming/`, where
  `chatflow.attachment.put` takes them into the store),
  `cortex:state-load` (the slot state an earlier shell kept;
  the frame keeps it now, `frame.ui_state`)), navigation
  guards, and the shell modules.
- `preload.cjs`: the renderer's only door (context isolation on, no Node integration).
- `frame-link.js`: a CPP/1 peer over the frame's WebSocket that says `frame.hello` as a category.
- `modules.js`: the shell-extension loader.

In development the shell runs inside Electron's own bundle; `npm install` (postinstall, or
`npm run brand:electron`) renames that bundle and its executable to CortexCode and sets the Info.plist
name, display name and icon, so the Dock, Force Quit and Activity Monitor show the app, not Electron.

## Shell modules (the shell-extension interface)

Code that must run in Electron's main process lives in a shell module, never in `main.js`. A
module is a folder with a package.json:

```json
{ "cortex": { "id": "browser.shell-module", "shellModule": { "entry": "index.js" } } }
```

Its entry exports `async function load(api)` and returns `{ unload(), beforeQuit?() }`:

| `api.` | What it is |
|---|---|
| `window` | the workbench `BrowserWindow` |
| `home`, `version` | `CORTEX_HOME`, the shell's version |
| `log(line)` | the shell's log |
| `serve(hello, handler)` | say `frame.hello` (with `hello`) on a new frame connection and answer requests with `handler(method, params)`; returns `{ publish(topic, data), stop() }` |
| `connect()` | a plain frame client (no hello): `{ call(method, params), on(prefix, listener), subscribe(topics, listener), onOpen(listener), connected, publish(topic, data), close() }`. `on`/`subscribe` call `frame.subscribe` and deliver every matching `evt` to the listener; subscriptions are renewed on every reconnect, and `onOpen` listeners run on each (re)connection (and at once if already connected) |
| `ipc.on(channel, listener)` | messages the workbench page sends on `channel` (only from the workbench) |
| `onOpenUrl(listener)` | web links the workbench hands over instead of navigating |
| `focusWindow()` | bring the workbench window to the front |
| `navigate(target)` | ask the workbench to show a target (below) |

Every connection a module opens through `serve` or `connect` is closed when the module unloads,
whether or not the module closed it.

### Targets

`navigate(target)` and every `notify.activate { target }` event open a target in the workbench
(`openTarget` in `@cortex/sdk`). `target.open` says what it is:

| `open` | What the workbench does |
|---|---|
| `session`, `approval` | selects `project_id` (looked up from the session when absent) and `session_id`, scrolls the conversation to `seq` (else its end) and puts the caret in the composer |
| `task`, `agent` | selects the project and opens the board on the task (`task_id`) or agent (`agent`) |
| `job` | selects the job's conversation and brings the terminal forward |
| `plugins`, `settings` | opens the settings window (`panel`, else the plugins panel for `plugins`) |
| absent | selects `project_id`/`session_id`, brings `view` to the front of the right pane, scrolls to `seq` |

### Focus

The workbench publishes `{ project_id, session_id, window_focused, visible }` on `ui.focus` and,
with `source: "workbench"` and `at` (ms), on `notify.focus`, whenever the selection, window focus
or visibility changes and as a heartbeat every 10 s (a listener may treat a report older than 30 s
as stale).

Discovery: every directory in `CORTEX_SHELL_MODULES` (a path list); unless
`CORTEX_SHELL_MODULES_EXCLUSIVE=1`, also the `shell` (or `shell-module`) folder of each plugin
folder in the repository's `plugins/` (and `ui/plugins/`) whose package.json declares
`cortex.shellModule`. The frame starts the shell with exactly the shell modules of its enabled
plugins (from each plugin.toml's `[[shell]]`) and the exclusive flag. While the shell runs it
follows the frame's `plugin.unit` events: a disabled plugin's modules are unloaded, an enabled or
newly installed plugin's modules are loaded (an installed package's from its store folder).
Hot-swap: when a `.js`, `.mjs`, `.cjs` or `.json` file of a module changes (outside its
`node_modules`), the shell calls `unload()`, releases the module's frame connections, copies the
module's folder to a fresh snapshot under `<CORTEX_HOME>/plugins-data/ui/shell-modules/` (its
`node_modules`, if any, linked in), imports the entry from there and calls `load(api)`. Node never
forgets an imported ES module, so the snapshot is what makes every relatively imported file new
code too. A module resolves its bare imports (other than `electron` and Node's own) from its own
`node_modules`. `beforeQuit()` runs before the app exits (the browser flushes its cookies there).
