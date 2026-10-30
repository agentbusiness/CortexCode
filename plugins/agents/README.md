# Agents (`agents`)

Delegation of a goal into a task tree with contracts (planned by the model over the code graph), named agents working in the project's own folder (one tree for every session; the tools plugin reserves files under edit and runs checks on a throw-away copy while others are active), a scheduler whose agent concurrency follows measured throughput (no fixed ceiling) with foreground priority, contract verification by the host, the project board and the steward that verifies a finished task in the project. Stages, texts, prompts and schemas are data (`backend/data/`). The UI packages are the board pane and the card's agents, tasks and board sections, plus the agents settings panel.

One self-contained plugin folder, separable into its own repository (only `cc-protocol` and `@cortex/sdk` come from outside):

| Path | What |
|---|---|
| `plugin.toml` | identity, build commands and outputs, UI packages and their slots, shell modules, source location |
| `backend/` | Cargo workspace (depends on `cc-protocol` by path), binary `cc-agents`: category `agents`, contract `agents@1` |
| `ui/` | one npm workspace (own `package-lock.json`, `@cortex/sdk` as a local dependency): `ui/board` (pane.board@1), `ui/card-agents` (card.agents@1), `ui/card-board` (card.board@1), `ui/card-tasks` (card.tasks@1), `ui/settings-agents` (settings.agents@1); each package builds `dist/` with `manifest.json` |

## Build and run

```sh
cd backend && cargo build --release && cargo clippy --all-targets -- -D warnings
cd ui && npm ci && npm run build && npm test     # after the SDK is built (cd ../../ui/sdk && npm run build)
cortex-pack build plugins/agents                  # -> plugins/agents/target/agents-<version>-<target>.ccplugin
```

`cortex-pack build plugins/agents --install` packs the folder into a `.ccplugin` that the frame installs (`frame.plugins.install`), which hot-swaps the backend and the UI packages together; Settings > Plugins disables and enables the whole plugin.

## Contracts

- A task's contract is frozen when an agent claims it (`agents.spawn` stores its SHA-256 on the task). Memory refuses any later change that is not an accepted revision.
- The worker may only propose a revision (`agents.contract.propose`), from its baseline or after a failure. The host refuses it mechanically when a check changes directory or names a path outside the root, or when nothing machine-checkable is left. Otherwise the planner judges it in a fresh context (REVIEW stage). An accepted revision is recorded on the task with old hash, new hash, reason, evidence, proposer and acceptor (`foreman`).
- An agent's work is in the project's tree as it happens; `done` means its frozen contract passes there (the steward verifies, nothing is merged). An abandoned agent's steps are undone through the step log (`tools.undo`). Command checks run from the project root. Paths are compared by file identity, not spelling, so firmlinked and symlinked aliases of the root count as the root. Manual or unknown check kinds are INDETERMINATE, never passed, and leave the task `indeterminate` for a person.
## Hand-offs between flows

`agents.handoff {from_session, project?, flow?, brief, title?}` (and `cx agent handoff <brief>
[--in <project>] [--flow <flow>]` from a session's shell) hands a piece of work to a flow in a
project, this one or another, among the flows the project takes (`Project.flows`): a task there
with the brief and no contract, and an agent (role `handoff`) of that flow whose session is the
asking session's child. The asker keeps going; when the work is done or ends unfinished, its result
comes back into the asking session as a note (`purpose: handoff_result`). Every chat flow call goes
to the session's own flow (`chatflow-<flow>.<verb>`), and the session's flow is the tag of the
models it may use.

## Control, supervision and the team channel

- The session that delegated owns its agents. `agents.stop`, `agents.finish` (the session takes the task as done as it stands: the run ends now, without its own checks, and the merge steward judges the work; a pass starts the tasks waiting for it), `agents.restart`, `agents.cancel` (a task with its subtasks and the tasks waiting for it; their agents stop, their changes stay in the files), `agents.steer`, `agents.list` and `agents.scheduler` are in the hello's `commands`, so a session's shell reaches them through `cx` (`cx agent stop|finish|restart|send|list`, `cx task cancel`). A stop cancels the tasks waiting for the stopped one; a restart reopens them. An agent that answers its session's message in words and ends its turn stops there too: its run ends done and the steward judges it, so a message saying "stop" is never overridden by the run's own checks.
- Supervision (`agents.supervise`, a `Supervise` event to the delegating session): an agent blocked (run ended badly, contract fails in the project, an error), a board post of kind `need` addressed to `lead`, and the whole tree done. The chat flow tells the session and, when it is idle, wakes it into MANAGE. Progress lines (`agent_progress` notes: done, cancelled, restarted) go to that session's journal for its card.
- Board posts carry the work: a done post names the changed files; a cancel tells the team not to wait. The chat flow delivers to each agent only what bears on its task (addressed to it; from its team, asks and decisions and posts about a related task; from elsewhere, a claim on code it touches).

- Agents run in the mode they were spawned with (`auto` or `ask`); dependents inherit it. Commands are confined to the workspace by the tools plugin's write sandbox.
