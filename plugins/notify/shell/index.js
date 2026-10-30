// @ts-check
/**
 * The notify plugin's shell module: runs in the Electron main process (loaded by the shell's
 * module loader, see ui/shell/modules.js). It brings the window forward when a notification is
 * clicked, publishes where the user is looking, and at start says so once when macOS has
 * CortexCode's notifications turned off (with a button to the setting).
 *
 * Interface (the shell-extension contract): `export async function load(api)` returning
 * `{ unload() }`. From `api` it uses `window` (the workbench BrowserWindow), `home` (CORTEX_HOME)
 * and `log(line)`. It talks to the frame over its own WebSocket link, which also tells it what the
 * workbench shows: the slot state the frame keeps (`frame.ui_state.get`, read on every connection)
 * and the workbench's `ui.focus` events (the selected project and session as they change).
 */

import { app, dialog, nativeImage, shell as electronShell } from 'electron';
import { execFileSync } from 'node:child_process';
import fs from 'node:fs';
import path from 'node:path';
import crypto from 'node:crypto';
import { fileURLToPath } from 'node:url';
import { FrameClient } from './lib/link.js';
import { NotifyShell } from './lib/shell.js';

/** The CortexCode icon, shown on the permission dialog. */
const ICON = path.join(path.dirname(fileURLToPath(import.meta.url)), 'assets', 'cortexcode-256.png');
/** macOS's Notifications settings, opened at one app's entry (`?id=<bundle id>`). */
const NOTIFICATION_SETTINGS = 'x-apple.systempreferences:com.apple.Notifications-Settings.extension';
/** The CortexCode notification app's bundle id (its own Info.plist). */
const NOTIFY_BUNDLE = 'dev.cortexcode.notify';

/**
 * The frame's API token from the macOS Keychain (the frame writes it there at every start; on
 * other systems frame.json carries it). `/usr/bin/security` made the item, so nothing asks.
 * @param {string} home
 * @returns {string | null}
 */
function keychainToken(home) {
  try {
    const out = execFileSync('/usr/bin/security', ['find-generic-password', '-s', 'CortexCode frame token', '-a', home, '-w'], { encoding: 'utf8', stdio: ['ignore', 'pipe', 'ignore'] });
    return out.replace(/\n$/, '') || null;
  } catch {
    return null;
  }
}

/**
 * Where the frame is: the shell's environment, else <home>/run/frame.json (read on every attempt,
 * since a restarted frame has a new port and token).
 * @param {string} home
 */
function frameAddress(home) {
  return () => {
    if (process.env.CORTEX_FRAME_URL && process.env.CORTEX_FRAME_TOKEN) {
      return { url: process.env.CORTEX_FRAME_URL, token: process.env.CORTEX_FRAME_TOKEN };
    }
    try {
      const info = JSON.parse(fs.readFileSync(path.join(home, 'run', 'frame.json'), 'utf8'));
      const token = info?.token || keychainToken(home);
      if (info?.port && token) return { url: `http://127.0.0.1:${info.port}`, token: String(token) };
    } catch {
      // The frame is not up yet.
    }
    return null;
  };
}

/**
 * The part of the shell API this module uses (see the shell's README, "Shell modules").
 * @typedef {{
 *   window: import('electron').BrowserWindow,
 *   home: string,
 *   log(line: string): void,
 * }} ShellApi
 */

/** @param {ShellApi} api */
export async function load(api) {
  const window = api.window;
  const windowLike = {
    isFocused: () => !window.isDestroyed() && window.isFocused(),
    isVisible: () => !window.isDestroyed() && window.isVisible(),
    isMinimized: () => !window.isDestroyed() && window.isMinimized(),
    on: (/** @type {any} */ event, /** @type {() => void} */ listener) => void window.on(event, listener),
    removeListener: (/** @type {any} */ event, /** @type {() => void} */ listener) => void window.removeListener(event, listener),
    bringForward: () => {
      if (window.isDestroyed()) return;
      if (window.isMinimized()) window.restore();
      window.show();
      window.focus();
      app.focus({ steal: true });
    },
  };
  /** @type {NotifyShell | null} */
  let shell = null;
  const link = new FrameClient({
    address: frameAddress(api.home),
    topics: ['notify.posted', 'notify.activate', 'ui.focus'],
    log: api.log,
    onOpen: () => {
      shell?.connected();
      // The selection the workbench restores (and shows until it changes) is in the frame's slot state.
      link
        .call('frame.ui_state.get', {})
        .then((state) => shell?.stateSaved(/** @type {{ document?: unknown }} */ (state)?.document))
        .catch((error) => api.log(`slot state not read: ${error instanceof Error ? error.message : String(error)}`));
    },
    onEvent: (topic, data) => {
      if (topic === 'ui.focus') {
        // The workbench's own focus report carries the selection as it changes.
        const focus = /** @type {{ project_id?: unknown, session_id?: unknown }} */ (data ?? {});
        shell?.stateSaved({ workbench: { projectId: focus.project_id ?? null, sessionId: focus.session_id ?? null } });
        return;
      }
      shell?.event(topic, data);
    },
  });
  shell = new NotifyShell({
    link,
    window: windowLike,
    delivererId: `electron-${process.pid}-${crypto.randomBytes(4).toString('hex')}`,
    log: api.log,
    onPermissionOff: () => {
      if (window.isDestroyed()) return;
      void dialog
        .showMessageBox(window, {
          type: 'info',
          icon: nativeImage.createFromPath(ICON),
          message: 'Notifications are turned off for CortexCode',
          detail: 'CortexCode tells you when a run finishes, needs your approval or fails. Turn on notifications for CortexCode in System Settings to get them.',
          buttons: ['Open Notification Settings', 'Not now'],
          defaultId: 0,
          cancelId: 1,
        })
        .then(({ response }) => {
          if (response === 0) void electronShell.openExternal(`${NOTIFICATION_SETTINGS}?id=${NOTIFY_BUNDLE}`);
        });
    },
  });
  shell.start();
  link.start();
  api.log(`notification presence as ${shell.delivererId}`);
  return {
    async unload() {
      await shell?.stop();
      link.stop();
      shell = null;
    },
  };
}
