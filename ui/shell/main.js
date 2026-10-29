// @ts-check
/**
 * The Electron shell: one window that loads the workbench the frame serves, plus the agent
 * browser (`browser@1`), which the shell serves to the frame over its own WebSocket.
 *
 * The frame starts it as `ui/node_modules/.bin/electron ui` with CORTEX_FRAME_URL and
 * CORTEX_FRAME_TOKEN; without them it reads <CORTEX_HOME>/run/frame.json. No application logic
 * lives here: the workbench is the renderer's, the agents are the frame's.
 */

import { app, BrowserWindow, dialog, ipcMain, shell as electronShell } from 'electron';
import crypto from 'node:crypto';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { FrameLink } from './frame-link.js';
import { ShellModules, moduleDirs } from './modules.js';

const here = path.dirname(fileURLToPath(import.meta.url));
const pkg = JSON.parse(fs.readFileSync(path.join(here, '..', 'package.json'), 'utf8'));
const home = process.env.CORTEX_HOME || path.join(os.homedir(), '.cortexcode');
const dataDir = path.join(home, 'plugins-data', 'ui');
const stateFile = path.join(dataDir, 'slot-state.json');
/** The CortexCode icon (docs/brand/final): the window's and the Dock's in development. */
const ICON = path.join(here, 'assets', 'cortexcode-512.png');

// The shell's profile (window state, the agent browser's cookies) lives in CORTEX_HOME, so a test
// home is a separate profile and a real one keeps the agent's sign-ins.
fs.mkdirSync(dataDir, { recursive: true });
app.setName('CortexCode');
app.setPath('userData', path.join(dataDir, 'electron'));

/** @param {string} line */
function log(line) {
  process.stdout.write(`[cortex-ui] ${line}\n`);
}

/** @returns {{ url: string, token: string } | null} */
function frameAddress() {
  if (process.env.CORTEX_FRAME_URL && process.env.CORTEX_FRAME_TOKEN) {
    return { url: process.env.CORTEX_FRAME_URL.replace(/\/+$/, ''), token: process.env.CORTEX_FRAME_TOKEN };
  }
  try {
    const info = JSON.parse(fs.readFileSync(path.join(home, 'run', 'frame.json'), 'utf8'));
    if (info && info.port && info.token) return { url: `http://127.0.0.1:${info.port}`, token: String(info.token) };
  } catch {
    // Not running yet.
  }
  return null;
}

/** The mark (docs/brand/final/cortexcode.svg), drawn on the waiting page before the frame serves any. */
const MARK = '<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 64 64" width="76" height="76"><rect width="64" height="64" fill="#14171A"/><path d="M17 16 7 32l10 16M47 16l10 16-10 16" fill="none" stroke="#E9EBEC" stroke-width="3.5" stroke-linecap="square"/><path d="M23 23H41V41H23Z M23 23 41 41 M41 23 23 41" fill="none" stroke="#6E9BB5" stroke-width="1.6"/><rect x="19.5" y="19.5" width="7" height="7" fill="#E9EBEC"/><rect x="37.5" y="19.5" width="7" height="7" fill="#E9EBEC"/><rect x="19.5" y="37.5" width="7" height="7" fill="#E9EBEC"/><rect x="37.5" y="37.5" width="7" height="7" fill="#E2603F"/></svg>';

/** The start-up splash's look (plugins/workbench/ui/window/src/splash.css) while the frame is not reachable: the
 * mark, the word mark and the step, with the bar at its start. */
function waitingPage(/** @type {string} */ text) {
  const html = `<!doctype html><meta charset="utf-8"><title>CortexCode</title><style>
body{margin:0;height:100vh;background:#14171A;color:#E9EBEC;display:flex;flex-direction:column;align-items:center;justify-content:center;-webkit-app-region:drag;user-select:none;cursor:default}
.w{display:flex;align-items:baseline;gap:10px;margin-top:22px}
.w b{font:700 13px "Helvetica Neue",Arial,sans-serif;letter-spacing:.26em;text-transform:uppercase}
.w svg{width:9px;height:9px;color:#E2603F;align-self:center}
.w i{font:italic 15.5px "Iowan Old Style",Georgia,serif;color:#A8AFB3}
.b{margin-top:44px;width:264px;height:2px;background:#262B2F}
.b span{display:block;height:100%;width:4%;background:#E2603F}
p{margin:14px 0 0;font:italic 13px/18px "Iowan Old Style",Georgia,serif;color:#828A8F}
</style><body>${MARK}<div class="w"><b>CortexCode</b><svg viewBox="0 0 9 9"><path d="M4.5 0v9M0 4.5h9" stroke="currentColor" stroke-width="1.1"/></svg><i>IronGraph</i></div><div class="b"><span></span></div><p>${text}</p></body>`;
  return `data:text/html;charset=utf-8,${encodeURIComponent(html)}`;
}

/** @type {BrowserWindow | null} */
let win = null;
/** @type {ShellModules | null} */
let modules = null;

const single = app.requestSingleInstanceLock();
if (!single) {
  app.quit();
} else {
  app.on('second-instance', () => {
    if (!win) return;
    if (win.isMinimized()) win.restore();
    win.focus();
  });
  app.whenReady().then(start).catch((error) => {
    log(`startup failed: ${error instanceof Error ? error.stack : String(error)}`);
    app.exit(1);
  });
}

async function start() {
  // A development run is Electron's own bundle: give the Dock the CortexCode icon. Its label (and
  // the Force Quit entry) come from the bundle's Info.plist, which scripts/brand-electron.mjs sets
  // after install; app.setName cannot change them.
  if (process.platform === 'darwin' && app.dock && fs.existsSync(ICON)) app.dock.setIcon(ICON);
  win = new BrowserWindow({
    icon: ICON,
    width: 1680,
    height: 1050,
    minWidth: 1100,
    minHeight: 680,
    title: 'CortexCode',
    backgroundColor: '#14171A',
    show: true,
    webPreferences: {
      preload: path.join(here, 'preload.cjs'),
      contextIsolation: true,
      nodeIntegration: false,
      sandbox: true,
      spellcheck: false,
    },
  });
  const window = win;

  let address = frameAddress();
  if (!address) {
    await window.loadURL(waitingPage('Starting CortexCode'));
    while (!address) {
      await new Promise((resolve) => setTimeout(resolve, 500));
      address = frameAddress();
    }
  }
  const frame = address;
  const origin = new URL(frame.url).origin;
  const workbenchUrl = `${frame.url}/ui/?token=${encodeURIComponent(frame.token)}`;

  /** IPC is answered only for the workbench page the frame served. */
  const fromWorkbench = (/** @type {Electron.IpcMainEvent | Electron.IpcMainInvokeEvent} */ event) => {
    const url = event.senderFrame?.url ?? '';
    return event.sender === window.webContents && url.startsWith(`${origin}/ui/`);
  };
  /** @type {Set<(url: string) => void>} */
  const urlListeners = new Set();
  const openUrl = (/** @type {string} */ url) => {
    if (urlListeners.size === 0) log(`no shell module opens links; dropped ${url}`);
    for (const listener of urlListeners) listener(url);
  };

  ipcMain.handle('cortex:pick-folder', async (event, title) => {
    if (!fromWorkbench(event)) return null;
    const result = await dialog.showOpenDialog(window, {
      title: typeof title === 'string' && title ? title : 'Open a project folder',
      properties: ['openDirectory', 'createDirectory'],
    });
    return result.canceled ? null : result.filePaths[0] ?? null;
  });
  // Bytes that never were a file (a pasted screenshot) are written where the chat flow's
  // attachment store takes them in (`chatflow.attachment.put` moves them into the store), so they
  // never travel base64 over the frame protocol. Bounded like the store itself.
  const incoming = path.join(home, 'attachments', 'incoming');
  const MAX_ATTACHMENT_BYTES = 20 * 1024 * 1024;
  ipcMain.handle('cortex:save-attachment', (event, bytes, name) => {
    if (!fromWorkbench(event)) return { error: 'not allowed' };
    if (!(bytes instanceof Uint8Array)) return { error: 'no bytes' };
    if (bytes.byteLength > MAX_ATTACHMENT_BYTES) return { error: `too large (${bytes.byteLength} bytes)` };
    const extension = path.extname(typeof name === 'string' ? name : '').toLowerCase().replace(/[^.a-z0-9]/g, '').slice(0, 11) || '.bin';
    fs.mkdirSync(incoming, { recursive: true });
    const file = path.join(incoming, `${crypto.randomUUID()}${extension}`);
    fs.writeFileSync(file, bytes);
    return { path: file };
  });
  // Files and folders in one dialog (attachments): every chosen path, none when cancelled.
  ipcMain.handle('cortex:pick-paths', async (event, title) => {
    if (!fromWorkbench(event)) return [];
    const result = await dialog.showOpenDialog(window, {
      title: typeof title === 'string' && title ? title : 'Attach files or folders',
      properties: ['openFile', 'openDirectory', 'multiSelections'],
    });
    return result.canceled ? [] : result.filePaths;
  });
  ipcMain.handle('cortex:pick-file', async (event, title) => {
    if (!fromWorkbench(event)) return null;
    const result = await dialog.showOpenDialog(window, { title: typeof title === 'string' ? title : 'Choose a file', properties: ['openFile'] });
    return result.canceled ? null : result.filePaths[0] ?? null;
  });
  ipcMain.handle('cortex:reveal', (event, file) => {
    if (!fromWorkbench(event) || typeof file !== 'string' || !path.isAbsolute(file)) return 'not allowed';
    if (!fs.existsSync(file)) return `${file} does not exist`;
    electronShell.showItemInFolder(file);
    return '';
  });
  // Pages that belong in the user's own browser (GitHub's sign-in, with their own session there);
  // https only.
  ipcMain.handle('cortex:open-external', async (event, url) => {
    if (!fromWorkbench(event) || typeof url !== 'string' || !URL.canParse(url) || new URL(url).protocol !== 'https:') return 'not allowed';
    try {
      await electronShell.openExternal(url);
      return '';
    } catch (error) {
      return error instanceof Error ? error.message : String(error);
    }
  });
  ipcMain.handle('cortex:state-load', (event) => {
    if (!fromWorkbench(event)) return {};
    try {
      return JSON.parse(fs.readFileSync(stateFile, 'utf8'));
    } catch {
      return {};
    }
  });

  // The workbench never navigates away from itself; links open in the agent browser instead.
  window.webContents.on('will-navigate', (event, url) => {
    if (url.startsWith(`${origin}/ui/`)) return;
    event.preventDefault();
    if (/^https?:/i.test(url)) openUrl(url);
  });
  window.webContents.setWindowOpenHandler(({ url }) => {
    if (/^https?:/i.test(url)) openUrl(url);
    return { action: 'deny' };
  });
  window.webContents.on('did-fail-load', (_event, code, description, url, isMainFrame) => {
    if (!isMainFrame || !url.startsWith(origin)) return;
    log(`workbench did not load (${code} ${description}); retrying`);
    setTimeout(() => void window.loadURL(workbenchUrl).catch(() => undefined), 1000);
  });
  window.webContents.on('render-process-gone', (_event, details) => {
    log(`workbench renderer gone (${details.reason}); reloading`);
    setTimeout(() => void window.loadURL(workbenchUrl).catch(() => undefined), 500);
  });

  // Shell modules (the browser, …) serve their categories over their own frame connections.
  modules = new ShellModules({
    dirs: moduleDirs(path.join(here, '..')),
    log,
    api: (id) => {
      // Every frame connection the module opens through this api; closed when the module unloads.
      /** @type {Set<FrameLink>} */
      const links = new Set();
      return {
      window,
      home,
      version: pkg.version,
      log: (line) => log(`[${id}] ${line}`),
      serve(hello, handler) {
        const link = new FrameLink({
          url: frame.url,
          token: frame.token,
          log: (line) => log(`[${id}] ${line}`),
          hello: { ...hello, instance: process.env.CORTEX_PLUGIN_INSTANCE ?? `ui-${id}-${process.pid}` },
          handler,
        });
        links.add(link);
        link.start();
        return { publish: (topic, data) => link.publish(topic, data), stop: () => { links.delete(link); link.stop(); } };
      },
      connect() {
        /** @type {Set<() => void>} */
        const openListeners = new Set();
        const client = new FrameLink({
          url: frame.url,
          token: frame.token,
          log: (line) => log(`[${id}] ${line}`),
          hello: null,
          onHello: () => { for (const listener of openListeners) listener(); },
        });
        links.add(client);
        client.start();
        return {
          call: (method, params) => client.call(method, params ?? {}),
          on: (prefix, listener) => client.on(prefix, listener),
          subscribe: (topics, listener) => {
            const offs = topics.map((topic) => client.on(topic, listener));
            return () => offs.forEach((off) => off());
          },
          onOpen: (listener) => {
            openListeners.add(listener);
            if (client.accepted) listener();
            return () => openListeners.delete(listener);
          },
          get connected() { return client.accepted; },
          publish: (topic, data) => client.publish(topic, data),
          close: () => { links.delete(client); client.stop(); },
        };
      },
      release() {
        for (const link of links) link.stop();
        links.clear();
      },
      focusWindow() {
        if (window.isMinimized()) window.restore();
        window.show();
        window.focus();
        app.focus({ steal: true });
      },
      navigate(target) {
        window.webContents.send('cortex:navigate', target);
      },
      ipc: {
        on(channel, listener) {
          const wrapped = (/** @type {Electron.IpcMainEvent} */ event, /** @type {unknown} */ payload) => {
            if (fromWorkbench(event)) listener(payload);
          };
          ipcMain.on(channel, wrapped);
          return () => ipcMain.removeListener(channel, wrapped);
        },
      },
      onOpenUrl(listener) {
        urlListeners.add(listener);
        return () => urlListeners.delete(listener);
      },
      };
    },
    cacheDir: path.join(home, 'plugins-data', 'ui', 'shell-modules'),
  });
  await modules.start();
  // Plugins enabled, disabled or installed while the shell runs: their shell modules follow.
  const running = modules;
  const units = new FrameLink({ url: frame.url, token: frame.token, log: (line) => log(`[plugin units] ${line}`), hello: null });
  units.on('plugin.unit', (_topic, data) => {
    void running.applyUnit(/** @type {{ id?: string, shell?: string[], shell_dirs?: string[] }} */ (data ?? {})).catch((error) => log(`plugin.unit: ${error}`));
  });
  units.start();

  await window.loadURL(workbenchUrl).catch((error) => log(`workbench load: ${error instanceof Error ? error.message : String(error)}`));
}

let flushed = false;
app.on('will-quit', (event) => {
  if (flushed || !modules) return;
  // Modules flush what must survive (the browser's cookies and storage) before the process ends.
  event.preventDefault();
  flushed = true;
  const running = modules;
  // A module that never settles (its frame gone mid-flush) must not keep the process alive: the
  // shell exits after a bounded wait either way.
  const deadline = setTimeout(() => {
    log('shell modules did not finish within 8 s; exiting');
    app.exit(0);
  }, 8000);
  running.beforeQuit().finally(() => running.stop().finally(() => {
    clearTimeout(deadline);
    app.quit();
  }));
});
app.on('window-all-closed', () => app.quit());
for (const signal of /** @type {const} */ (['SIGTERM', 'SIGINT'])) {
  process.on(signal, () => app.quit());
}
