# @cortex-ui/browser-shell-module

`browser@1` served by the CortexCode shell (Electron main process). It is a **shell module**: the
shell finds it by its package.json (`cortex.shellModule.entry`), imports the entry, and calls
`load(api)`; the returned object's `unload()` stops it. When the entry file changes the shell
unloads the running module and loads the new one (see `ui/shell/README.md`).

- Tabs are Electron `WebContentsView`s in the persistent session partition `persist:cortex-browser`
  (cookies and storage survive restarts), each driven through `webContents.debugger` (CDP 1.3).
- Methods: open, navigate, tabs, close, select, back, forward, reload, snapshot (accessibility
  tree with `[ref=N]`), screenshot (PNG file), console, network, eval, click / type (by ref or CSS
  selector), press, metrics (`Performance.getMetrics`), cdp (raw). Topic: `browser.tabs`.
- `open` with `background: true` makes a hidden tab (never shown, not in `browser.tabs`), used by
  plugins that read pages (research's web search); the caller closes it by `tab_id`.
- The browser pane plugin (`pane.browser@1`) reports where the page goes on the IPC channel
  `cortex:browser-bounds`; this module lays the active tab over that region.

Tests: `npm test` (node:test; the pure parts: snapshot formatting, keys, PNG, URLs).
