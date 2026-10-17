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

/**
 * The shell-extension interface this module needs (documented with the shell).
 * @typedef {{
 *   window: import('electron').BrowserWindow,
 *   home: string,
 *   version: string,
 *   log(line: string): void,
 *   serve(hello: Record<string, unknown>, handler: (method: string, params: any) => Promise<unknown>): { publish(topic: string, data: unknown): void, stop(): void },
 *   ipc: { on(channel: string, listener: (payload: unknown) => void): () => void },
 *   onOpenUrl(listener: (url: string) => void): () => void,
 * }} ShellApi
 */

/** @param {ShellApi} api */
export async function load(api) {
  /** @type {{ publish(topic: string, data: unknown): void, stop(): void } | null} */
  let link = null;
  const service = new BrowserService({
    window: api.window,
    home: api.home,
    publish: (topic, data) => link?.publish(topic, data),
    log: api.log,
  });
  link = api.serve(
    { plugin_id: 'cortex-ui-browser', version: api.version, category: 'browser', contract: 'browser@1', provides: PROVIDES, subscribes: [], swap: 'blue_green' },
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
      service.closeAll();
    },
    async beforeQuit() {
      await service.flush();
    },
  };
}
