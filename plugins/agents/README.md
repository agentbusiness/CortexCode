# Agents (`agents`)

Delegation of a goal into a task tree with contracts (planned by the model over the code graph), named agents in copy-on-write workspaces, a scheduler whose agent concurrency follows measured throughput (no fixed ceiling) with foreground priority, contract verification by the host, the project board and the merge steward. Stages, texts, prompts and schemas are data (`backend/data/`). The UI packages are the board pane and the card's agents, tasks and board sections, plus the agents settings panel.

One self-contained plugin folder, separable into its own repository (only `cc-protocol` and `@cortex/sdk` come from outside):

| Path | What |
|---|---|
| `plugin.toml` | identity, build commands and outputs, UI packages and their slots, shell modules, source location |
| `backend/` | Cargo workspace (depends on `cc-protocol` by path), binary `cc-agents`: category `agents`, contract `agents@1` |
| `ui/` | one npm workspace (own `package-lock.json`, `@cortex/sdk` as a local dependency): `ui/board` (pane.board@1), `ui/card-agents` (card.agents@1), `ui/card-board` (card.board@1), `ui/card-tasks` (card.tasks@1), `ui/settings-agents` (settings.agents@1); each package builds `dist/` with `manifest.json` |
| `verify/` | gate scripts, run against the real frame |
| `fixtures/` | fixture repositories for the verify scripts |

## Build and run

```sh
cd backend && cargo build --release && cargo test --release && cargo clippy --all-targets -- -D warnings
cd ui && npm ci && npm run build && npm test     # after the SDK is built (cd ../../ui/sdk && npm run build)
cortex-pack build plugins/agents                  # -> plugins/agents/target/agents-<version>-<target>.ccplugin
```

`cortex-pack build plugins/agents --install` packs the folder into a `.ccplugin` that the frame installs (`frame.plugins.install`), which hot-swaps the backend and the UI packages together; Settings > Plugins disables and enables the whole plugin.

## Contracts

- A task's contract is frozen when an agent claims it (`agents.spawn` stores its SHA-256 on the task). Memory refuses any later change that is not an accepted revision.
- The worker may only propose a revision (`agents.contract.propose`), from its baseline or after a failure. The host refuses it mechanically when a check changes directory or names a path outside the root, or when nothing machine-checkable is left. Otherwise the planner judges it in a fresh context (REVIEW stage). An accepted revision is recorded on the task with old hash, new hash, reason, evidence, proposer and acceptor (`foreman`).
- Command checks run from the workspace root. Paths are compared by file identity, not spelling, so firmlinked and symlinked aliases of the root count as the root. Manual or unknown check kinds are INDETERMINATE, never passed, and leave the task `indeterminate` for a person.
- Agents run in the mode they were spawned with (`auto` or `ask`); dependents inherit it. Commands are confined to the workspace by the tools plugin's write sandbox.

## Verification

verify/mechanics.mjs (gate 1.6 G3, deterministic peers), verify/real_do.mjs and verify/real_delegate.mjs (G4/G5, with the real model). `fixtures/` holds their fixture repositories.
