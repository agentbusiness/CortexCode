# @cortex-ui/memory-irongraph-card-knowledge

What the project knows (docs/PROJECT-KNOWLEDGE-PLAN.md P6): the project's knowledge items from
`memory.knowledge.list`, by kind (Concepts, Facts, Decisions, Theories, Questions), each with its
source, date, confidence, the code it binds to and its evidence; Withdraw for any live item, mark a
theory supported or refuted, answer a question, and Add an item by hand. Refreshes on
`memory.knowledge.changed`.

- **Plugin id:** `memory-irongraph.card-knowledge`
- **Slot:** `card.knowledge@1`
- **Build:** `npm run build` → `dist/` (`index.js`, `style.css`, `manifest.json`)
- **Test:** `npm test`

React, ReactDOM and `@cortex/sdk` are peer dependencies the workbench provides.

## Learned procedures

The foldout keeps the existing knowledge tabs and adds review of project-scoped procedure proposals. Owners can permit a trial, review exact-revision uses, approve regular use after sufficient helpful non-origin tasks, reject or withdraw a revision. Completion alone is not evidence of help. See the [source feature guide](../../../../docs/VALUE-TRACING-AND-PROCEDURES.md). This addition has not been built or visually verified in the app.
