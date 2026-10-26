# @cortex-ui/updater-band

Updates in the brand band: download progress, "Restart to update" when a newer frame is staged, a
short line when a plugin was updated, and the popup at the bottom right that asks about a plugin the
user changed (merge the release into the change, keep the user's version, or show the changes).

- **Plugin id:** `updater.band`
- **Slot:** `workbench.band@1`
- **Build:** `npm run build` → `dist/` (`index.js`, `style.css`, `manifest.json`)

React, ReactDOM and `@cortex/sdk` are peer dependencies the workbench provides.
