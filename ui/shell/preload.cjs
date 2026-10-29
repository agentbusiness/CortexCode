// The renderer's only door into the shell. The workbench runs with no Node integration and with
// context isolation; this exposes these calls and nothing else:
//   setBrowserBounds      - where the browser pane is (the browser shell module lays the page there)
//   pickFolder / pickFile - the native dialogs (Open project; a plugin executable to swap in)
//   pickPaths             - one dialog for files and folders together (attachments)
//   pathForFile           - the local path of a dropped or picked File (empty when it has none)
//   saveAttachment        - write pasted bytes where the chat flow's attachment store takes them in
//   reveal                - show a project file in Finder
//   openExternal          - an https page in the user's own browser (GitHub's sign-in), where
//                           every other link opens in the agent browser
//   loadState             - the slot state an earlier shell kept itself (the frame keeps it now:
//                           frame.ui_state), read once to move it into the frame
//   onNavigate            - a shell module asks the workbench to show a target (a session, a view)
const { contextBridge, ipcRenderer, webUtils } = require('electron');

contextBridge.exposeInMainWorld('cortexShell', {
  /** @param {{ x: number, y: number, width: number, height: number, visible: boolean }} bounds */
  setBrowserBounds(bounds) {
    if (!bounds || typeof bounds !== 'object') return;
    ipcRenderer.send('cortex:browser-bounds', {
      x: Number(bounds.x) || 0,
      y: Number(bounds.y) || 0,
      width: Number(bounds.width) || 0,
      height: Number(bounds.height) || 0,
      visible: Boolean(bounds.visible),
    });
  },
  /** @param {string} [title] */
  pickFolder(title) {
    return ipcRenderer.invoke('cortex:pick-folder', typeof title === 'string' ? title : '');
  },
  /** @param {string} [title] */
  pickPaths(title) {
    return ipcRenderer.invoke('cortex:pick-paths', typeof title === 'string' ? title : '');
  },
  /** @param {File} file */
  pathForFile(file) {
    try {
      return webUtils.getPathForFile(file) || '';
    } catch {
      return '';
    }
  },
  /** @param {Uint8Array} bytes @param {string} name */
  saveAttachment(bytes, name) {
    return ipcRenderer.invoke('cortex:save-attachment', bytes, String(name ?? ''));
  },
  /** @param {string} title */
  pickFile(title) {
    return ipcRenderer.invoke('cortex:pick-file', String(title ?? ''));
  },
  /** @param {(target: unknown) => void} listener */
  onNavigate(listener) {
    if (typeof listener !== 'function') return;
    ipcRenderer.on('cortex:navigate', (_event, target) => listener(target));
  },
  /** @param {string} path */
  reveal(path) {
    return ipcRenderer.invoke('cortex:reveal', String(path));
  },
  /** @param {string} url */
  openExternal(url) {
    return ipcRenderer.invoke('cortex:open-external', String(url));
  },
  loadState() {
    return ipcRenderer.invoke('cortex:state-load');
  },
});
