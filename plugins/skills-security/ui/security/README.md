# @cortex-ui/security-pane

The Security tab: the last audit, the findings by priority, one finding's trace, fix and history,
and the moves on it.

- **Plugin id:** `skills-security.pane`
- **Slot:** `pane.security@1`
- **Build:** `npm run build` → `dist/` (`index.js`, `style.css`, `manifest.json`)

React, ReactDOM and `@cortex/sdk` are peer dependencies the workbench provides.

## Slot contract

- **Fills:** `pane.security@1` as `skills-security.pane`
- **Props:** `{ active: boolean }` (whether this view is the right pane's open tab)
- **Hosts:** no slots
- **Workbench state read:** `agents.defaultMode`, `projectId`, `sessionId`, `security.selected`, `security.severity`, `security.showFixed`
- **Workbench state written:** `chat.stick`, `code.open`, `pane.tab`, `layout.paneCollapsed`, `card.open`, `overlay.open` (through `openInCode`), `projectId`, `sessionId`, `security.selected`, `security.severity`, `security.showFixed`
- **Own durable state (survives hot swaps):** none
- **Frame methods called:** `chatflow.turn`, `frame.session.create`, `memory.security.finding`, `memory.security.finding.update`, `memory.security.findings`, `memory.security.forget`, `memory.security.summary`
- **Topics followed:** `memory.security.changed`

Styles use the theme's tokens and primitives only. Nothing is imported from the renderer or from
another plugin: the package builds alone.

## What it shows

1. The head bar: the last audit (when, units, lines judged) and "Audit now", which creates a chat
   session titled "Security audit" and sends `security audit of this codebase` as its first message
   (the composer's `chatflow.turn` with the person's default mode), then selects that session.
2. The severity chips (critical, high, medium, low, lead, note) with the counts of open and fixing
   findings; a chip filters the list. "Show fixed" adds fixed, verified and gone findings.
3. The findings, highest priority first: severity, title, `sink_path:sink_line`, class, status and
   a "quick fix" mark when the host spelled the fix out. Accepted and closed findings are greyed.
4. A row opens the finding under itself: who can do what because of which control and with what
   impact; the trace, each step a link that opens the file at its line in the code view; the fix
   (where, change, how to prove it; the quick fix's commands); the evidence; the priority's reason;
   a lead's missing fact; the fix session and commit; the history.
5. The moves: "Fix in a session" and "Quick fix" create a session titled `Fix: <title>` whose first
   message is built from the finding, then mark it `fixing` with that session; "Accept risk" and
   "Not an issue" take a note inline (required) and mark it `accepted` or `gone`; "Reopen" marks an
   accepted, fixing or closed finding `open`; "Forget" deletes a closed finding after a second click.
   A refused move shows the store's answer under the actions.
