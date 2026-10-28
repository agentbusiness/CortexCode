# @cortex-ui/updater-band

Updates: "Restart to update" in the brand band when a newer frame is staged (first of the band's items, so the
ones to its right never move); at the bottom right the
round (every update of a check, one row per plugin with its progress) and the popup that asks about a plugin the
user changed (merge the release into the change, keep the user's version, or show the changes).

- **Plugin id:** `updater.band`
- **Slot:** `workbench.band@1`
- **Build:** `npm run build` → `dist/` (`index.js`, `style.css`, `manifest.json`)

React, ReactDOM and `@cortex/sdk` are peer dependencies the workbench provides.
