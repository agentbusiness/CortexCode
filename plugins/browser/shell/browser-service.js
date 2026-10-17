// @ts-check
/**
 * `browser@1`, served by the shell: Chromium tabs as Electron `WebContentsView`s, each driven
 * through `webContents.debugger` (the full Chrome DevTools Protocol). The views are shown in the
 * renderer's browser pane, whose bounds the renderer reports; the profile lives in a persistent
 * session partition so cookies and storage survive restarts.
 */

import { WebContentsView, session } from 'electron';
import { randomUUID } from 'node:crypto';
import fs from 'node:fs/promises';
import path from 'node:path';
import { formatAxTree, parseTarget } from './ax-snapshot.js';
import { keyEvents } from './keys.js';
import { pngSize } from './png.js';
import { normalizeUrl } from './url.js';

/** A protocol error with a CPP/1 code (the shell's frame link sends `code` with the response). */
export class RpcError extends Error {
  /**
   * @param {string} code
   * @param {string} message
   */
  constructor(code, message) {
    super(message);
    this.code = code;
  }
}

export const PARTITION = 'persist:cortex-browser';

/** Size a tab lays out at before the pane has ever been shown. */
const DEFAULT_BOUNDS = { x: 0, y: 0, width: 1280, height: 800 };

/**
 * @typedef {{ level: string, text: string, ts_ms: number, source?: string | null }} ConsoleMessage
 * @typedef {{ url: string, method: string, status?: number | null, mime?: string | null,
 *   duration_ms?: number | null, failed?: string | null, started?: number, ts_ms: number }} NetworkEntry
 */

class Tab {
  /**
   * @param {string} id
   * @param {WebContentsView} view
   * @param {{ sessionId: string | null, internal: boolean }} options
   */
  constructor(id, view, options) {
    this.id = id;
    this.view = view;
    this.wc = view.webContents;
    this.sessionId = options.sessionId;
    this.internal = options.internal;
    /** @type {ConsoleMessage[]} */
    this.console = [];
    /** @type {Map<string, NetworkEntry>} */
    this.requests = new Map();
    /** @type {NetworkEntry[]} */
    this.network = [];
    /** @type {Map<number, number>} */
    this.refs = new Map();
    this.loadError = /** @type {string | null} */ (null);
    this.createdMs = Date.now();
  }

  /**
   * @param {string} method
   * @param {Record<string, unknown>} [params]
   */
  cdp(method, params = {}) {
    return this.wc.debugger.sendCommand(method, params);
  }

  info() {
    return {
      tab_id: this.id,
      url: this.wc.getURL(),
      title: this.wc.getTitle(),
      loading: this.wc.isLoading(),
      session_id: this.sessionId,
      load_error: this.loadError,
    };
  }

  /**
   * One CDP event.
   * @param {string} method
   * @param {any} params
   */
  onEvent(method, params) {
    switch (method) {
      case 'Runtime.consoleAPICalled': {
        const text = (params.args ?? []).map(remoteText).join(' ');
        this.console.push({ level: consoleLevel(params.type), text, ts_ms: Date.now(), source: 'console-api' });
        return;
      }
      case 'Runtime.exceptionThrown': {
        const details = params.exceptionDetails ?? {};
        const text = details.exception?.description ?? details.text ?? 'uncaught exception';
        this.console.push({ level: 'error', text, ts_ms: Date.now(), source: 'exception' });
        return;
      }
      case 'Log.entryAdded': {
        const entry = params.entry ?? {};
        this.console.push({ level: entry.level ?? 'info', text: entry.text ?? '', ts_ms: Date.now(), source: entry.source ?? 'log' });
        return;
      }
      case 'Network.requestWillBeSent': {
        const previous = this.requests.get(params.requestId);
        if (previous && params.redirectResponse) {
          // A redirect ends the previous hop under the same request id.
          previous.status = params.redirectResponse.status;
          previous.mime = params.redirectResponse.mimeType ?? null;
          previous.duration_ms = previous.started !== undefined ? round((params.timestamp - previous.started) * 1000) : null;
        }
        /** @type {NetworkEntry} */
        const entry = { url: params.request?.url ?? '', method: params.request?.method ?? 'GET', status: null, mime: null, duration_ms: null, failed: null, started: params.timestamp, ts_ms: Date.now() };
        this.requests.set(params.requestId, entry);
        this.network.push(entry);
        return;
      }
      case 'Network.responseReceived': {
        const entry = this.requests.get(params.requestId);
        if (!entry) return;
        entry.status = params.response?.status ?? null;
        entry.mime = params.response?.mimeType ?? null;
        return;
      }
      case 'Network.loadingFinished': {
        const entry = this.requests.get(params.requestId);
        if (entry && entry.started !== undefined) entry.duration_ms = round((params.timestamp - entry.started) * 1000);
        return;
      }
      case 'Network.loadingFailed': {
        const entry = this.requests.get(params.requestId);
        if (!entry) return;
        entry.failed = params.errorText ?? 'failed';
        if (entry.started !== undefined) entry.duration_ms = round((params.timestamp - entry.started) * 1000);
        return;
      }
      default:
        return;
    }
  }
}

/** @param {number} value */
function round(value) {
  return Math.round(value * 100) / 100;
}

/** @param {string} type */
function consoleLevel(type) {
  if (type === 'warning') return 'warn';
  if (type === 'error' || type === 'assert') return 'error';
  if (type === 'debug') return 'debug';
  return type === 'log' ? 'log' : 'info';
}

/** @param {any} arg */
function remoteText(arg) {
  if (arg.type === 'string') return arg.value;
  if ('value' in arg) return typeof arg.value === 'object' ? JSON.stringify(arg.value) : String(arg.value);
  if (arg.unserializableValue) return arg.unserializableValue;
  return arg.description ?? arg.type ?? '';
}

export class BrowserService {
  /**
   * @param {{ window: import('electron').BrowserWindow, home: string,
   *   publish: (topic: string, data: unknown) => void, log: (line: string) => void }} options
   */
  constructor(options) {
    this.window = options.window;
    this.home = options.home;
    this.publish = options.publish;
    this.log = options.log;
    /** @type {Map<string, Tab>} */
    this.tabs = new Map();
    /** @type {string | null} */
    this.activeId = null;
    this.bounds = { ...DEFAULT_BOUNDS };
    this.visible = false;
    /** @type {ReturnType<typeof setTimeout> | null} */
    this.publishTimer = null;
    const profile = session.fromPartition(PARTITION);
    // Pages the agent visits ask for nothing on this machine's behalf.
    profile.setPermissionRequestHandler((_wc, _permission, callback) => callback(false));
    profile.on('will-download', (_event, item) => {
      item.setSavePath(path.join(this.home, 'artifacts', 'browser', 'downloads', `${Date.now()}-${item.getFilename()}`));
    });
  }

  /** The renderer's browser pane moved, resized, appeared or went away. */
  setPane(/** @type {{ x: number, y: number, width: number, height: number, visible: boolean }} */ pane) {
    const width = Math.max(0, Math.round(pane.width));
    const height = Math.max(0, Math.round(pane.height));
    if (width > 0 && height > 0) this.bounds = { x: Math.round(pane.x), y: Math.round(pane.y), width, height };
    this.visible = Boolean(pane.visible) && width > 0 && height > 0;
    this.layout();
  }

  layout() {
    for (const tab of this.tabs.values()) {
      tab.view.setBounds(this.bounds);
      tab.view.setVisible(this.visible && tab.id === this.activeId && !tab.internal);
    }
  }

  /** Close every tab (the module is being unloaded or swapped). */
  closeAll() {
    for (const tab of [...this.tabs.values()]) this.closeTab(tab);
  }

  /** Flush cookies and storage to disk (before quitting). */
  async flush() {
    const profile = session.fromPartition(PARTITION);
    await profile.cookies.flushStore();
    await profile.flushStorageData?.();
  }

  /**
   * Serve one browser.* request.
   * @param {string} method
   * @param {any} params
   */
  async handle(method, params) {
    switch (method) {
      case 'plugin.probe':
        return this.probe();
      case 'plugin.shutdown':
        for (const tab of [...this.tabs.values()]) this.closeTab(tab);
        return null;
      case 'browser.open': {
        // A background tab (research reading a search page) is neither shown nor listed.
        const background = params.background === true;
        const tab = await this.openTab(requireString(params, 'url'), { sessionId: params.session_id ?? null, activate: !background, internal: background });
        return tab.info();
      }
      case 'browser.navigate': {
        const tab = this.tab(params);
        await this.load(tab, requireString(params, 'url'));
        return tab.info();
      }
      case 'browser.tabs':
        return { tabs: this.visibleTabs().map((tab) => tab.info()), active: this.activeId };
      case 'browser.select': {
        const tab = this.tab(params);
        this.activeId = tab.id;
        this.layout();
        this.changed();
        return tab.info();
      }
      case 'browser.close': {
        this.closeTab(this.tab(params));
        return null;
      }
      case 'browser.back':
      case 'browser.forward':
      case 'browser.reload': {
        const tab = this.tab(params);
        const history = tab.wc.navigationHistory;
        if (method === 'browser.back' && history.canGoBack()) history.goBack();
        else if (method === 'browser.forward' && history.canGoForward()) history.goForward();
        else if (method === 'browser.reload') tab.wc.reload();
        await waitForLoad(tab.wc);
        return tab.info();
      }
      case 'browser.snapshot': {
        const tab = this.tab(params);
        await tab.cdp('Accessibility.enable');
        const { nodes } = /** @type {{ nodes: any[] }} */ (await tab.cdp('Accessibility.getFullAXTree'));
        const { tree, refs } = formatAxTree(nodes);
        tab.refs = refs;
        return { url: tab.wc.getURL(), title: tab.wc.getTitle(), tree };
      }
      case 'browser.screenshot': {
        const tab = this.tab(params);
        const { data } = /** @type {{ data: string }} */ (await tab.cdp('Page.captureScreenshot', { format: 'png', fromSurface: true }));
        const bytes = Buffer.from(data, 'base64');
        const { width, height } = pngSize(bytes);
        const dir = path.join(this.home, 'artifacts', 'browser');
        await fs.mkdir(dir, { recursive: true });
        const file = path.join(dir, `${tab.id}-${Date.now()}.png`);
        await fs.writeFile(file, bytes);
        return { path: file, width, height };
      }
      case 'browser.console': {
        const tab = this.tab(params);
        const since = typeof params.since_ms === 'number' ? params.since_ms : 0;
        return { messages: tab.console.filter((message) => message.ts_ms >= since) };
      }
      case 'browser.network': {
        const tab = this.tab(params);
        return {
          entries: tab.network.map((entry) => ({
            url: entry.url, method: entry.method, status: entry.status ?? null, mime: entry.mime ?? null,
            duration_ms: entry.duration_ms ?? null, failed: entry.failed ?? null,
          })),
        };
      }
      case 'browser.eval': {
        const tab = this.tab(params);
        return this.evaluate(tab, requireString(params, 'expression'));
      }
      case 'browser.click': {
        const tab = this.tab(params);
        await this.click(tab, requireString(params, 'target'));
        return { ok: true };
      }
      case 'browser.type': {
        const tab = this.tab(params);
        const text = requireString(params, 'text');
        const node = await this.resolve(tab, requireString(params, 'target'));
        await tab.cdp('DOM.focus', { backendNodeId: node });
        await tab.cdp('Input.insertText', { text });
        return { ok: true };
      }
      case 'browser.press': {
        const tab = this.tab(params);
        let events;
        try {
          events = keyEvents(requireString(params, 'key'));
        } catch (error) {
          throw new RpcError('invalid_params', error instanceof Error ? error.message : String(error));
        }
        await tab.cdp('Input.dispatchKeyEvent', events.down);
        await tab.cdp('Input.dispatchKeyEvent', events.up);
        return { ok: true };
      }
      case 'browser.metrics': {
        const tab = this.tab(params);
        await tab.cdp('Performance.enable');
        const { metrics } = /** @type {{ metrics: Array<{ name: string, value: number }> }} */ (await tab.cdp('Performance.getMetrics'));
        /** @type {Record<string, number>} */
        const out = {};
        for (const metric of metrics) out[metric.name] = metric.value;
        return { metrics: out };
      }
      case 'browser.cdp': {
        const tab = this.tab(params);
        return tab.cdp(requireString(params, 'method'), params.params && typeof params.params === 'object' ? params.params : {});
      }
      default:
        throw new RpcError('unknown_method', `unknown method \`${method}\``);
    }
  }

  /**
   * @param {string} url
   * @param {{ sessionId: string | null, activate: boolean, internal: boolean }} options
   */
  async openTab(url, options) {
    const view = new WebContentsView({
      webPreferences: { partition: PARTITION, sandbox: true, contextIsolation: true, nodeIntegration: false, backgroundThrottling: false },
    });
    view.setBackgroundColor('#FFFFFF');
    const id = randomUUID().slice(0, 8);
    const tab = new Tab(id, view, { sessionId: options.sessionId, internal: options.internal });
    this.tabs.set(id, tab);
    this.window.contentView.addChildView(view);
    view.setBounds(this.bounds);
    view.setVisible(false);
    const wc = tab.wc;
    // A fresh view has no renderer until something loads; DevTools commands sent before that
    // wait forever. An empty page first gives the debugger a target from the very first request.
    await wc.loadURL('about:blank');
    wc.debugger.attach('1.3');
    wc.debugger.on('message', (_event, method, params) => tab.onEvent(method, params));
    wc.debugger.on('detach', (_event, reason) => this.log(`browser tab ${id} debugger detached: ${reason}`));
    await tab.cdp('Runtime.enable');
    await tab.cdp('Log.enable');
    await tab.cdp('Network.enable');
    await tab.cdp('Page.enable');
    for (const event of ['page-title-updated', 'did-start-loading', 'did-stop-loading', 'did-navigate', 'did-navigate-in-page']) {
      wc.on(/** @type {any} */ (event), () => this.changed());
    }
    // A page that opens a window gets a new tab instead.
    wc.setWindowOpenHandler(({ url: next }) => {
      void this.openTab(next, { sessionId: tab.sessionId, activate: true, internal: false }).catch((error) => this.log(`popup failed: ${error}`));
      return { action: 'deny' };
    });
    wc.on('destroyed', () => {
      if (this.tabs.get(id) === tab) this.forget(tab);
    });
    if (options.activate) this.activeId = id;
    this.layout();
    this.changed();
    await this.load(tab, url);
    return tab;
  }

  /**
   * @param {Tab} tab
   * @param {string} url
   */
  async load(tab, url) {
    tab.loadError = null;
    tab.refs = new Map();
    try {
      await tab.wc.loadURL(normalizeUrl(url));
    } catch (error) {
      // The tab stays, showing Chromium's error page; the caller learns why.
      tab.loadError = error instanceof Error ? error.message : String(error);
    }
    this.changed();
  }

  /** @param {Tab} tab */
  closeTab(tab) {
    this.forget(tab);
    if (!tab.wc.isDestroyed()) {
      try {
        tab.wc.debugger.detach();
      } catch {
        // Already detached.
      }
      this.window.contentView.removeChildView(tab.view);
      tab.wc.close();
    }
  }

  /** @param {Tab} tab */
  forget(tab) {
    this.tabs.delete(tab.id);
    if (this.activeId === tab.id) this.activeId = this.visibleTabs().at(-1)?.id ?? null;
    this.layout();
    this.changed();
  }

  visibleTabs() {
    return [...this.tabs.values()].filter((tab) => !tab.internal);
  }

  /** @param {any} params */
  tab(params) {
    const id = requireString(params, 'tab_id');
    const tab = this.tabs.get(id);
    if (!tab || tab.wc.isDestroyed()) throw new RpcError('not_found', `no browser tab ${id}`);
    return tab;
  }

  /**
   * Resolve `ref=N` or a CSS selector to a backend DOM node id.
   * @param {Tab} tab
   * @param {string} target
   */
  async resolve(tab, target) {
    const parsed = parseTarget(target);
    if ('ref' in parsed) {
      const node = tab.refs.get(parsed.ref);
      if (node === undefined) throw new RpcError('not_found', `no element ref=${parsed.ref} in the last snapshot; take a new snapshot`);
      return node;
    }
    const { root } = /** @type {{ root: { nodeId: number } }} */ (await tab.cdp('DOM.getDocument', { depth: 0 }));
    const { nodeId } = /** @type {{ nodeId: number }} */ (await tab.cdp('DOM.querySelector', { nodeId: root.nodeId, selector: parsed.selector }));
    if (!nodeId) throw new RpcError('not_found', `no element matches ${parsed.selector}`);
    const { node } = /** @type {{ node: { backendNodeId: number } }} */ (await tab.cdp('DOM.describeNode', { nodeId }));
    return node.backendNodeId;
  }

  /**
   * A real click: scroll the element into view and press the mouse at its centre.
   * @param {Tab} tab
   * @param {string} target
   */
  async click(tab, target) {
    const backendNodeId = await this.resolve(tab, target);
    await tab.cdp('DOM.scrollIntoViewIfNeeded', { backendNodeId });
    const { quads } = /** @type {{ quads: number[][] }} */ (await tab.cdp('DOM.getContentQuads', { backendNodeId }));
    const quad = quads?.[0];
    if (!quad) throw new RpcError('not_found', `${target} has no visible box to click`);
    const x = (quad[0] + quad[2] + quad[4] + quad[6]) / 4;
    const y = (quad[1] + quad[3] + quad[5] + quad[7]) / 4;
    await tab.cdp('Input.dispatchMouseEvent', { type: 'mouseMoved', x, y });
    await tab.cdp('Input.dispatchMouseEvent', { type: 'mousePressed', x, y, button: 'left', clickCount: 1 });
    await tab.cdp('Input.dispatchMouseEvent', { type: 'mouseReleased', x, y, button: 'left', clickCount: 1 });
  }

  /**
   * @param {Tab} tab
   * @param {string} expression
   */
  async evaluate(tab, expression) {
    const response = /** @type {any} */ (await tab.cdp('Runtime.evaluate', { expression, returnByValue: true, awaitPromise: true, userGesture: true }));
    if (response.exceptionDetails) {
      const details = response.exceptionDetails;
      throw new RpcError('eval_error', details.exception?.description ?? details.text ?? 'evaluation threw');
    }
    const result = response.result ?? {};
    return { value: 'value' in result ? result.value : null, type: result.type ?? 'undefined' };
  }

  /** Conformance self-test, run for real on a hidden tab. */
  async probe() {
    /** @type {Array<{ name: string, ok: boolean, detail: string, ms: number }>} */
    const checks = [];
    const timed = async (/** @type {string} */ name, /** @type {() => Promise<string>} */ run) => {
      const started = Date.now();
      try {
        checks.push({ name, ok: true, detail: await run(), ms: Date.now() - started });
      } catch (error) {
        checks.push({ name, ok: false, detail: error instanceof Error ? error.message : String(error), ms: Date.now() - started });
      }
    };
    const page = 'data:text/html,' + encodeURIComponent('<title>probe</title><button id="b" onclick="this.textContent=\'pressed\'">press</button><script>console.log("probe-ready")</script>');
    /** @type {Tab | null} */
    let tab = null;
    await timed('open', async () => {
      tab = await this.openTab(page, { sessionId: null, activate: false, internal: true });
      return tab.wc.getTitle();
    });
    if (tab) {
      const probeTab = /** @type {Tab} */ (tab);
      await timed('snapshot', async () => {
        const result = /** @type {{ tree: string }} */ (await this.handle('browser.snapshot', { tab_id: probeTab.id }));
        if (!/button "press" \[ref=\d+\]/.test(result.tree)) throw new Error(`snapshot lacks the button: ${result.tree}`);
        return 'button found with a ref';
      });
      await timed('eval', async () => {
        const result = /** @type {{ value: unknown }} */ (await this.evaluate(probeTab, '6 * 7'));
        if (result.value !== 42) throw new Error(`6 * 7 evaluated to ${JSON.stringify(result.value)}`);
        return '42';
      });
      await timed('console', async () => {
        if (!probeTab.console.some((message) => message.text === 'probe-ready')) throw new Error('console message not captured');
        return 'captured';
      });
      this.closeTab(probeTab);
    }
    return { ok: checks.length > 0 && checks.every((check) => check.ok), checks };
  }

  changed() {
    if (this.publishTimer) return;
    this.publishTimer = setTimeout(() => {
      this.publishTimer = null;
      this.publish('browser.tabs', { tabs: this.visibleTabs().map((tab) => tab.info()), active: this.activeId });
    }, 50);
  }
}

/**
 * @param {any} params
 * @param {string} field
 */
function requireString(params, field) {
  const value = params?.[field];
  if (typeof value !== 'string' || value === '') throw new RpcError('invalid_params', `\`${field}\` must be a non-empty string`);
  return value;
}

/**
 * Resolve once a navigation that may just have started has finished.
 * @param {import('electron').WebContents} wc
 * @returns {Promise<void>}
 */
async function waitForLoad(wc) {
  // Back, forward and reload start loading asynchronously; give the navigation a moment to begin.
  if (!wc.isLoading()) await new Promise((resolve) => setTimeout(resolve, 50));
  if (!wc.isLoading()) return;
  await new Promise((resolve) => wc.once('did-stop-loading', () => resolve(undefined)));
}
