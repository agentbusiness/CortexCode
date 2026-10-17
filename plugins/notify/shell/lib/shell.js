// @ts-check
/**
 * The notify shell module's behaviour, independent of Electron (index.js wires Electron in):
 *
 * - **Presence.** Holds a lease with cc-notify (`notify.deliverer`, renewed well before it runs
 *   out): while it holds, a click on a notification is handed to this window.
 * - **Activation.** Every `notify.activate` brings the window forward; the workbench opens the
 *   target. Notifications themselves are posted by the CortexCode notification app only.
 * - **Permission.** At start, reads the notification app's permission (`notify.status`): when macOS
 *   has notifications for CortexCode turned off, the window says so once and offers the setting.
 * - **Focus.** Publishes `notify.focus` — whether the window is focused and visible, and which
 *   session the workbench shows (from the workbench's own slot-state saves) — on every change and
 *   as a heartbeat, so cc-notify can stay quiet about the session the user is looking at, and
 *   marks sessions in the session list instead of asking macOS while the window is in front.
 */

export const LEASE_MS = 30_000;
export const RENEW_MS = 10_000;
export const FOCUS_TTL_MS = 30_000;

/**
 * @typedef {{ call(method: string, params: unknown, timeoutMs?: number): Promise<any>, publish(topic: string, data: unknown): boolean }} Link
 * @typedef {{ isFocused(): boolean, isVisible(): boolean, isMinimized(): boolean, on(event: string, listener: () => void): void,
 *   removeListener(event: string, listener: () => void): void, bringForward(): void }} WindowLike
 */

/**
 * What to tell the user about notification permission, from `notify.status`: `off` when macOS has
 * CortexCode's notifications turned off (the system no longer asks; only the setting changes it),
 * else null (allowed, or undecided: macOS shows its own prompt).
 * @param {any} status
 * @returns {'off' | null}
 */
export function permissionNotice(status) {
  return status?.native?.authorization === 'denied' ? 'off' : null;
}

export class NotifyShell {
  /**
   * @param {{
   *   link: Link,
   *   window: WindowLike | null,
   *   onPermissionOff?: () => void,
   *   delivererId: string,
   *   log?: (line: string) => void,
   *   now?: () => number,
   *   every?: (fn: () => void, ms: number) => any,
   *   stopEvery?: (handle: any) => void,
   * }} options
   */
  constructor(options) {
    this.link = options.link;
    this.onPermissionOff = options.onPermissionOff ?? (() => undefined);
    this.permissionChecked = false;
    this.window = options.window;
    this.delivererId = options.delivererId;
    this.log = options.log ?? (() => undefined);
    this.now = options.now ?? (() => Date.now());
    this.every = options.every ?? ((fn, ms) => setInterval(fn, ms));
    this.stopEvery = options.stopEvery ?? ((handle) => clearInterval(handle));
    /** @type {{ sessionId: string | null, projectId: string | null }} */
    this.selection = { sessionId: null, projectId: null };
    /** @type {string} */
    this.lastFocus = '';
    /** @type {any[]} */
    this.intervals = [];
    /** @type {Array<() => void>} */
    this.detachers = [];
    this.stopped = false;
  }

  start() {
    this.intervals.push(this.every(() => void this.renew(), RENEW_MS));
    this.intervals.push(this.every(() => this.publishFocus(true), RENEW_MS));
    const window = this.window;
    if (window) {
      const update = () => this.publishFocus(false);
      for (const event of ['focus', 'blur', 'show', 'hide', 'minimize', 'restore']) {
        window.on(event, update);
        this.detachers.push(() => window.removeListener(event, update));
      }
    }
  }

  /** The link (re)connected: take the lease, say where the user is, check the permission once. */
  connected() {
    void this.renew();
    this.publishFocus(true);
    void this.checkPermission();
  }

  /** Once per start: when macOS has CortexCode's notifications turned off, say so. */
  async checkPermission() {
    // Claimed before the call: connections that arrive meanwhile do not check (or ask) again.
    if (this.permissionChecked || this.stopped) return;
    this.permissionChecked = true;
    try {
      const status = await this.link.call('notify.status', {});
      if (permissionNotice(status) === 'off') this.onPermissionOff(status?.native?.bundle ?? '');
    } catch (error) {
      // cc-notify not running yet: the next connection checks again.
      this.permissionChecked = false;
      this.log(`notification permission not read: ${error instanceof Error ? error.message : String(error)}`);
    }
  }

  async renew() {
    if (this.stopped) return;
    try {
      await this.link.call('notify.deliverer', { deliverer_id: this.delivererId, kind: 'electron', ttl_ms: LEASE_MS, actions: true });
    } catch (error) {
      // cc-notify not running (yet): the next renewal tries again.
      this.log(`notification lease not renewed: ${error instanceof Error ? error.message : String(error)}`);
    }
  }

  /**
   * What the workbench shows: its slot state (the frame's `frame.ui_state`), or a `ui.focus`
   * report shaped like it (`{ workbench: { projectId, sessionId } }`); it carries the selection.
   * @param {unknown} document
   */
  stateSaved(document) {
    const workbench = /** @type {any} */ (document)?.workbench;
    if (!workbench || typeof workbench !== 'object') return;
    const sessionId = typeof workbench.sessionId === 'string' ? workbench.sessionId : null;
    const projectId = typeof workbench.projectId === 'string' ? workbench.projectId : null;
    if (sessionId === this.selection.sessionId && projectId === this.selection.projectId) return;
    this.selection = { sessionId, projectId };
    this.publishFocus(false);
  }

  focusState() {
    const window = this.window;
    return {
      source: 'electron-shell',
      window_focused: window ? window.isFocused() : false,
      visible: window ? window.isVisible() && !window.isMinimized() : false,
      session_id: this.selection.sessionId,
      project_id: this.selection.projectId,
      ts_ms: this.now(),
      ttl_ms: FOCUS_TTL_MS,
    };
  }

  /** @param {boolean} heartbeat publish even when nothing changed */
  publishFocus(heartbeat) {
    if (this.stopped) return;
    const state = this.focusState();
    const signature = JSON.stringify([state.window_focused, state.visible, state.session_id, state.project_id]);
    if (!heartbeat && signature === this.lastFocus) return;
    if (this.link.publish('notify.focus', state)) this.lastFocus = signature;
  }

  /**
   * @param {string} topic
   * @param {any} data
   */
  event(topic, data) {
    if (topic === 'notify.activate') {
      this.log(`notification ${data?.id} activated (${data?.via ?? 'unknown'}); bringing the window forward`);
      this.window?.bringForward();
    }
  }

  async stop() {
    this.stopped = true;
    for (const handle of this.intervals) this.stopEvery(handle);
    this.intervals = [];
    for (const detach of this.detachers) detach();
    this.detachers = [];
    try {
      await this.link.call('notify.deliverer', { deliverer_id: this.delivererId, detach: true }, 1000);
    } catch {
      // The frame or cc-notify is gone already; the lease runs out on its own.
    }
  }
}
