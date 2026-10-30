# Git (`vcs-git`)

git for the workbench and the agents: status, diff with hunks (a file whose change exceeds 2 MB comes back with `truncated` and its counts only, so a generated or data file never makes an answer larger than one message may be), log, branches, per-hunk stage/unstage/revert, commit, merge with a conflict report, shadow checkpoints that never touch the user's history, push, and GitHub status through `gh` only when asked. The GitHub connection (`vcs.github.*`): GitHub's device sign-in (needs `github_client_id` in `[backend.config]`) or a personal access token, kept in the macOS Keychain through `/usr/bin/security` (service "CortexCode GitHub", account "token"; on Linux the Secret Service through `secret-tool`, same attributes); the account and the dev folder are a `GitHubAccount` node in the app's graph, and checkpoints are `Checkpoint` nodes of their `Workspace` plus the `checkpoints` stream; the user's repositories, new repositories, forks, pull requests, and `vcs.clone` through libgit2 into the dev folder. The UI packages are the card's Git section and Settings > GitHub.

One self-contained plugin folder, separable into its own repository (only `cc-protocol` and `@cortex/sdk` come from outside):

| Path | What |
|---|---|
| `plugin.toml` | identity, build commands and outputs, UI packages and their slots, shell modules, source location |
| `backend/` | Cargo workspace (depends on `cc-protocol` by path), binary `cc-vcs`: category `vcs`, contract `vcs@1` |
| `ui/` | one npm workspace (own `package-lock.json`, `@cortex/sdk` as a local dependency): `ui/card-git` (card.git@1), `ui/settings-github` (settings.github@1); each package builds `dist/` with `manifest.json` |

## Build and run

```sh
cd backend && cargo build --release && cargo clippy --all-targets -- -D warnings
cd ui && npm ci && npm run build && npm test     # after the SDK is built (cd ../../ui/sdk && npm run build)
cortex-pack build plugins/vcs-git                  # -> plugins/vcs-git/target/vcs-git-<version>-<target>.ccplugin
```

`cortex-pack build plugins/vcs-git --install` packs the folder into a `.ccplugin` that the frame installs (`frame.plugins.install`), which hot-swaps the backend and the UI package together; Settings > Plugins disables and enables the whole plugin.
