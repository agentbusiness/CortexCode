# Schedule (`scheduler`)

Routines: recurring and timed work. A routine wakes a session (`wake`; an agent check-in when
`manage`), hands a brief to an agent in a session of its own (`handoff`), or runs a command and
wakes someone only when it fails (`command`), on a clock: `every 5m`, `every mon,thu at 09:00`,
`once at 2026-11-02 09:00`, `in 20m`, `cron 0 9 * * 1`. Routines belong to a project or are global.
The model makes them with `cx schedule ...` and reads them with `read schedule://`; the user sees
them on the card and in the Schedule pane, where they are edited, paused, run or removed. Design:
`docs/SCHEDULES-PLAN.md`.

| Path | What |
|---|---|
| `plugin.toml` | identity, build commands and outputs, UI packages and their slots, data |
| `backend/` | Cargo workspace (depends on `cc-protocol` by path), binary `cc-scheduler`: category `schedule`, contract `schedule@1` |
| `backend/data/texts.toml` | every model-facing text (the answers `cx` prints, what a run reads) |
| `ui/` | one npm workspace: `ui/card` (card.schedule@1), `ui/pane` (pane.schedule@1) |

## How it works

- Records are memory's (`memory.routine.*`, the scope's store); the plugin keeps a copy for the
  clock, loaded on activation, so it can be hot-swapped at any time.
- The clock compares wall-clock time with each active routine's next time at least every 15 s. A
  due routine is claimed (written with its next time) before it fires; what was missed fires once.
- A run that still runs is not started again (`skipped`). Three failed runs in a row pause the
  routine. Handoffs go through the agents queue.
- Culling, from structural facts only: a routine whose watched tasks are finished ends; two
  routines one model run made for the same thing keep the newer; a run the user stopped leaves
  its not-yet-fired routines paused; a wake whose session is deleted ends; a deleted project's
  routines are removed; `until` and `max_runs` end a routine.

## Build and run

```sh
cd backend && cargo build --release && cargo clippy --all-targets -- -D warnings
cd ui && npm ci && npm run build           # after the SDK is built (cd ../../ui/sdk && npm run build)
cortex-pack build plugins/scheduler --install
```

The category `schedule` needs a frame that knows it (`CATEGORIES` in the protocol).
