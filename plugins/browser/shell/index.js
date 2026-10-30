// @ts-check
/**
 * The browser shell module: `browser@1` served from Electron's main process. The shell loads it
 * through its shell-extension interface (ui/shell/README.md): `load(api)` starts it, and the
 * returned `unload()` stops it, so the module can be rebuilt and swapped while the app runs.
 *
 * It serves the category over its own frame connection (`api.serve`), lays its tabs over the
 * renderer's browser pane (`cortex:browser-bounds` from the browser pane plugin), and opens the
 * links the workbench hands it.
 */

import { BrowserService } from './browser-service.js';

const PROVIDES = [
  'browser.open', 'browser.navigate', 'browser.tabs', 'browser.close', 'browser.snapshot',
  'browser.screenshot', 'browser.console', 'browser.network', 'browser.eval', 'browser.click',
  'browser.type', 'browser.press', 'browser.metrics', 'browser.cdp', 'browser.select',
  'browser.back', 'browser.forward', 'browser.reload',
];
// What a command in the model's shells may call (`cx browser ...`): all but the raw `browser.cdp`.
const COMMANDS = PROVIDES.filter((method) => method !== 'browser.cdp');

/**
 * The shell-extension interface this module needs (documented with the shell).
 * @typedef {{
 *   window: import('electron').BrowserWindow,
 *   home: string,
 *   version: string,
 *   log(line: string): void,
 *   serve(hello: Record<string, unknown>, handler: (method: string, params: any) => Promise<unknown>): { publish(topic: string, data: unknown): void, stop(): void },
 *   connect(): { call(method: string, params?: unknown): Promise<unknown>, close(): void },
 *   ipc: { on(channel: string, listener: (payload: unknown) => void): () => void },
 *   onOpenUrl(listener: (url: string) => void): () => void,
 * }} ShellApi
 */

/** @param {ShellApi} api */
export async function load(api) {
  /** @type {{ publish(topic: string, data: unknown): void, stop(): void } | null} */
  let link = null;
  const store = api.connect();
  const service = new BrowserService({
    window: api.window,
    home: api.home,
    publish: (topic, data) => link?.publish(topic, data),
    log: api.log,
    record: (file) => void recordArtifact(store, file).catch((error) => api.log(`artifact ${file.path} not recorded: ${error}`)),
  });
  link = api.serve(
    { plugin_id: 'cortex-ui-browser', version: api.version, category: 'browser', contract: 'browser@1', provides: PROVIDES, commands: COMMANDS, subscribes: [], swap: 'blue_green' },
    (method, params) => service.handle(method, params),
  );
  const offBounds = api.ipc.on('cortex:browser-bounds', (bounds) => service.setPane(/** @type {any} */ (bounds)));
  const offUrl = api.onOpenUrl((url) => {
    void service.openTab(url, { sessionId: null, activate: true, internal: false }).catch((error) => api.log(`open ${url}: ${error}`));
  });
  return {
    async unload() {
      offBounds();
      offUrl();
      link?.stop();
      store.close();
      service.closeAll();
    },
    async beforeQuit() {
      await service.flush();
    },
  };
}

/** This machine's root node key, asked once. @type {string | null} */
let machineKey = null;

/**
 * A file the browser wrote, as `(:Artifact {key: path, kind, bytes, mime, url, …})` linked to the
 * session whose tab made it, else to this machine (`docs/GRAPH-MODEL.md`).
 * @param {{ call(method: string, params?: unknown): Promise<unknown> }} store
 * @param {{ path: string, kind: string, bytes: number, mime: string, url: string, sessionId: string | null, width?: number, height?: number }} file
 */
async function recordArtifact(store, file) {
  if (!machineKey) machineKey = String(/** @type {{ key: string }} */ (await store.call('memory.graph.machine', {})).key);
  const props = { path: file.path, kind: file.kind, bytes: file.bytes, mime: file.mime, url: file.url, created_ms: Date.now() };
  if (file.width) Object.assign(props, { width: file.width, height: file.height });
  const node = { label: 'Artifact', key: file.path, props };
  const to = file.sessionId ? { label: 'Session', key: file.sessionId } : { label: 'Machine', key: machineKey };
  const rel = file.sessionId ? 'FROM' : 'ON';
  await store.call('memory.graph.put', { nodes: [node], edges: [{ from: { label: 'Artifact', key: file.path }, rel, to }] });
}
