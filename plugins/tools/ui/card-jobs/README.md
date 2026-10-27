# @cortex-ui/tools-card-jobs

The running background jobs of the project in the card, under the model: each job's command, how
long it runs and how much memory its processes hold, with Stop (SIGTERM) and Kill (SIGKILL).
Nothing running: nothing shown.

- **Plugin id:** `tools.card-jobs`
- **Slot:** `card.jobs@1`
- **Build:** `npm run build` → `dist/` (`index.js`, `style.css`, `manifest.json`)

React, ReactDOM and `@cortex/sdk` are peer dependencies the workbench provides.
