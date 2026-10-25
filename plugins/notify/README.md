# notify — CortexCode notifications

macOS notifications when something needs the user: a run finished or failed, a command waits for
approval, an agent is blocked, a contract verified or failed, a merge conflicted, a long background
job ended, a plugin hot-swap failed or rolled back. One self-contained plugin folder:

| Part | What it is |
|---|---|
| `backend/` | `cc-notify`, the `notify@1` plugin process (Rust, own Cargo workspace), plus `CortexCodeNotify.app`, a tiny notification app built from `backend/helper/` by `build.rs` |
| `shell/` | the Electron main-process module: brings the window forward on a notification click, publishes the window's focus, and says once per start when macOS has CortexCode's notifications turned off |
| `ui/` | the settings panel (`settings.notify@1`: whether macOS shows them, which kinds, the focus rule), its own npm package built to `ui/dist` |
| `plugin.toml` | what to build, ship and plug in: the frame loads the folder from it (`[[plugin]] dir = "plugins/notify"`), `cortex-pack build plugins/notify` packs it into a `.ccplugin` (the app bundle travels beside `cc-notify`, where the backend finds it) |

## How it works

```
journal.<session> (status, tool_result needs_approval, approval and change pending, verification,
                   agent_spawned, note purpose=merge)
agents.changed (blocked) · plugin.swapped (ok=false / rolled_back) · tools.job (ended)
        │ structural trigger (field values only, no text interpretation)
        ▼
names: frame.session.get, frame.project.get, memory.task.get, agents.list, tools.job.status,
       frame.plugins.list (a change's plugins)
        ▼
notification {kind, title, subtitle, body, key, target}
        ▼
gate: preference for the kind (memory.settings `notify.kinds.<kind>`), focus rule
        ▼
history: same key within the dedup window -> update the item (same id), not a new one
        ▼
delivery, decided at delivery time from the latest fresh focus report:
          a CortexCode window in front (focused and visible) and the notification has a session
            -> no macOS notification; delivery `ui`, method `window`: the session's row in the
               session list is marked until that session is opened
          otherwise -> the CortexCode notification app when macOS allows it; without macOS's
            permission it is recorded `failed` with the reason and marks its session the same way
```

- **Kinds**: `done`, `failed`, `cancelled`, `approval`, `blocked`, `verified`, `verify_failed`,
  `conflict`, `spawned`, `job`, `swap_failed`, `report`, `info`. Titles say the outcome and
  the task (or session, or agent); the subtitle is `project · agent`; the body is the reason
  (status detail, the approval text, failing checks with the last output line, the swap error, a
  job's last output line, a code change's summary). The target says what activation opens
  (`session` at an entry, `approval` with the call and its classes or a change's `proposal`,
  `task`, `agent`, `job`, `plugins`). A pending approval or change offers Approve, which answers
  it at once (`chatflow.approve`, or `source.decide` for a change).
- **Dedup keys**: `run:<session>:<run>`, `approval:<session>:<call>`, `change:<proposal>`, `verify:<task>`,
  `blocked:<project>:<agent>`, `conflict:<session>`, `spawned:<session>`, `job:<job>`,
  `swap:<category>:<version>`. A repeat within the window (default 10 min, sliding) updates the
  item; it is shown again only when its text changed, replacing the shown one where the delivery
  path can (the notification app can).
- **Preferences** live in the main settings database (`memory.settings.*`, section `notify`):
  `notify.kinds.<kind>`, `notify.only_when_away`, `notify.long_job_ms`, `notify.dedup_window_ms`.
  cc-notify follows `memory.settings.changed`. When the database is unavailable the built-in
  defaults apply and every notification's detail and `notify.status` say so. A suppressed
  notification is still recorded, with the reason.
- **Focus**: the shell module publishes `notify.focus` (`window_focused`, `visible`, the
  workbench's selected session from its own slot-state saves) on change and every 10 s; it is stale
  after 30 s. With "only when not looking" on, a notification about the session on screen in the
  focused window is suppressed.
- **Session marks**: the session list (plugins/sidebar) reads `notify.history` and marks a
  session whose newest unseen notification reached the person only inside CortexCode (`ui` by
  `window`, or `failed`), with the notification's kind ("Finished", "Failed", "Needs you", ...).
  When a focus report shows that session in the focused, visible window, cc-notify marks those
  items seen (`activated_action: "viewed"`) and publishes `notify.changed`, which clears the mark.
- **Activation**: a click comes back as `notify.activated` (from the shell or the notification
  app); cc-notify publishes `notify.activate {id, action, target, ...}` for the UI to open the
  target, and the shell module brings the window forward. A click while no UI is connected opens
  the UI (`frame.ui.open`) and is handed to it once its deliverer attaches; a click while
  CortexCode is not running is spooled by the notification app and routed at the next start.

## Delivering to macOS

UNUserNotificationCenter only serves a process running from an app bundle with a bundle
identifier, after the user allowed that app. `cc-notify` is a bare executable started by the
frame, so it drives a bundled helper: `build.rs` compiles `helper/CortexNotify.swift` into
`CortexCodeNotify.app` (bundle id `dev.cortexcode.notify`, named "CortexCode", the C2 icon,
windowless), ad-hoc signed and rebuilt only when its sources or the compiler change, so its
signature stays stable. That app is the only path to macOS (owner): no `osascript` (macOS shows
those as Script Editor), no Electron notifications.

**Permission, like any app:** when CortexCode starts, cc-notify asks macOS for permission if it
has never been answered, and macOS shows its standard prompt. When it was turned off, macOS never
asks again; the window then says so once per start (shell module) and offers to open System
Settings at CortexCode's entry. Without permission every notification still appears in CortexCode's
own list, with the reason it was not shown.

## Configuration (`[plugin.config]`)

| Key | Default | Meaning |
|---|---|---|
| `helper_app` | beside the binary, else where build.rs put it | the notification app bundle |
| `request_authorization` | true | ask macOS for the notification app's permission when CortexCode starts |

## Contract

`protocol/src/contracts/notify.rs` (`notify@1`): methods `notify.send`, `notify.history`,
`notify.activated`, `notify.deliverer`, `notify.prefs.get`,
`notify.prefs.set`, `notify.status`; topics `notify.posted`, `notify.activate`,
`notify.focus`, `notify.changed`.

## Shell module interface

`shell/index.js` exports `load(api)` returning `{ unload() }` (the shell's module interface, see
the UI's `shell/README.md`). It uses `api.window`, `api.home` and `api.log`, and keeps its own
WebSocket link to the frame (the shell's `api.serve` is for serving a category; this module is a
client); the selection it reports in `notify.focus` comes from the frame's slot state
(`frame.ui_state.get`) and the workbench's `ui.focus` events. The frame hands the shell the module
folders of enabled plugins (`[[shell]]` in plugin.toml); disabling the plugin unloads it.

## Build

```sh
cd backend && cargo build --release            # cc-notify + CortexCodeNotify.app
cortex-pack build plugins/notify --install
```

## Absent in v0.1

- `report` notifications are posted by whoever produces a report (`notify.send`); no event derives
  them yet.
