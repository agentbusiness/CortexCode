// @ts-check
/**
 * Shell modules: code that must run in Electron's main process (today the browser) without being
 * wired into the shell. A shell module is a folder whose package.json says
 *
 *   "cortex": { "id": "browser.shell-module", "shellModule": { "entry": "index.js" } }
 *
 * and whose entry exports `async load(api)` returning `{ unload(), beforeQuit?() }`. The shell
 * finds modules in the directories named by CORTEX_SHELL_MODULES (path-list) and, unless
 * CORTEX_SHELL_MODULES_EXCLUSIVE=1 (the frame starts the shell so: it names exactly the modules of
 * enabled plugins), in the `shell` folder of every plugin folder (`<repo>/plugins/<plugin>/shell`).
 * When a module's entry file changes on disk, the shell unloads the running instance and loads the
 * new one (each generation is imported from a fresh snapshot of the module's folder, so the entry
 * and everything it imports relatively is new code). While it runs, the shell follows the frame's
 * `plugin.unit` events: a disabled plugin's modules are unloaded, an enabled or newly installed
 * plugin's modules are loaded ([`ShellModules.applyUnit`]).
 */

import fs from 'node:fs';
import path from 'node:path';
import { pathToFileURL } from 'node:url';

/**
 * @typedef {{ publish(topic: string, data: unknown): void, stop(): void }} ServedCategory
 * @typedef {{
 *   window: import('electron').BrowserWindow,
 *   home: string,
 *   version: string,
 *   log(line: string): void,
 *   serve(hello: Record<string, unknown>, handler: (method: string, params: any) => Promise<unknown>): ServedCategory,
 *   connect(): FrameClient,
 *   ipc: { on(channel: string, listener: (payload: unknown) => void): () => void },
 *   onOpenUrl(listener: (url: string) => void): () => void,
 *   focusWindow(): void,
 *   navigate(target: Record<string, unknown>): void,
 *   release?(): void,
 * }} ShellApi
 * @typedef {{
 *   call(method: string, params?: unknown): Promise<unknown>,
 *   on(prefix: string, listener: (topic: string, data: unknown) => void): () => void,
 *   subscribe(topics: string[], listener: (topic: string, data: unknown) => void): () => void,
 *   onOpen(listener: () => void): () => void,
 *   readonly connected: boolean,
 *   publish(topic: string, data: unknown): void,
 *   close(): void,
 * }} FrameClient
 * @typedef {{ unload(): Promise<void> | void, beforeQuit?(): Promise<void> | void }} LoadedModule
 */

/**
 * Folders that hold shell modules: CORTEX_SHELL_MODULES (a path list), then the `shell-module`
 * or `shell` folder of every plugin in `<ui>/plugins/` and in the repository's `plugins/`.
 * @param {string} uiDir
 */
export function moduleDirs(uiDir) {
  const dirs = [];
  for (const entry of (process.env.CORTEX_SHELL_MODULES ?? '').split(path.delimiter).filter(Boolean)) dirs.push(path.resolve(entry));
  if (process.env.CORTEX_SHELL_MODULES_EXCLUSIVE === '1') return [...new Set(dirs)];
  for (const plugins of [path.join(uiDir, 'plugins'), path.join(uiDir, '..', 'plugins')]) {
    if (!fs.existsSync(plugins)) continue;
    for (const plugin of fs.readdirSync(plugins).sort()) {
      for (const name of ['shell-module', 'shell']) {
        const candidate = path.join(plugins, plugin, name);
        if (!fs.existsSync(path.join(candidate, 'package.json'))) continue;
        try {
          if (JSON.parse(fs.readFileSync(path.join(candidate, 'package.json'), 'utf8')).cortex?.shellModule) dirs.push(candidate);
        } catch {
          // Not a shell module.
        }
      }
    }
  }
  return [...new Set(dirs.map((dir) => path.resolve(dir)))];
}

export class ShellModules {
  /**
   * @param {{ dirs: string[], api: (id: string) => ShellApi, log(line: string): void, cacheDir: string }} options
   */
  constructor(options) {
    this.dirs = options.dirs;
    this.cacheDir = options.cacheDir;
    this.makeApi = options.api;
    this.log = options.log;
    /** @type {Map<string, { dir: string, entry: string, loaded: LoadedModule | null, watcher: fs.FSWatcher | null, timer: ReturnType<typeof setTimeout> | null, generation: number, snapshot: string | null, release: (() => void) | null }>} */
    this.modules = new Map();
    /**
     * Last generation per module id, kept across unload and load: Node caches an imported module
     * by URL for the life of the process, so a snapshot path must never be used twice.
     * @type {Map<string, number>}
     */
    this.generations = new Map();
  }

  async start() {
    // Snapshots of earlier runs are stale.
    fs.rmSync(this.cacheDir, { recursive: true, force: true });
    fs.mkdirSync(this.cacheDir, { recursive: true });
    for (const dir of this.dirs) await this.add(dir);
  }

  /**
   * Load the module in `dir` (a module of the same id loaded from elsewhere is unloaded first).
   * @param {string} dir
   * @returns {Promise<string | null>} the module id, or null when `dir` holds no module
   */
  async add(dir) {
    try {
      const pkg = JSON.parse(fs.readFileSync(path.join(dir, 'package.json'), 'utf8'));
      const spec = pkg.cortex?.shellModule;
      const id = pkg.cortex?.id;
      if (!spec?.entry || typeof id !== 'string') return null;
      const existing = this.modules.get(id);
      if (existing && path.resolve(existing.dir) === path.resolve(dir)) return id;
      if (existing) await this.remove(id);
      const entry = path.resolve(dir, spec.entry);
      /** @type {{ dir: string, entry: string, loaded: LoadedModule | null, watcher: fs.FSWatcher | null, timer: ReturnType<typeof setTimeout> | null, generation: number, snapshot: string | null, release: (() => void) | null }} */
      const record = { dir, entry, loaded: null, watcher: null, timer: null, generation: 0, snapshot: null, release: null };
      this.modules.set(id, record);
      await this.load(id);
      record.watcher = fs.watch(dir, { recursive: true }, (_event, file) => {
        if (!file || !/\.(m?js|cjs|json)$/.test(String(file)) || String(file).split(path.sep)[0] === 'node_modules') return;
        if (record.timer) clearTimeout(record.timer);
        record.timer = setTimeout(() => void this.reload(id), 300);
      });
      return id;
    } catch (error) {
      this.log(`shell module in ${dir} not loaded: ${error instanceof Error ? error.stack : String(error)}`);
      return null;
    }
  }

  /**
   * Unload a module and forget it.
   * @param {string} id
   */
  async remove(id) {
    const record = this.modules.get(id);
    if (!record) return;
    this.modules.delete(id);
    record.watcher?.close();
    if (record.timer) clearTimeout(record.timer);
    try {
      await record.loaded?.unload();
    } catch (error) {
      this.log(`shell module ${id} unload failed: ${error}`);
    }
    record.release?.();
    if (record.snapshot) fs.rmSync(record.snapshot, { recursive: true, force: true });
    this.log(`shell module ${id} unloaded`);
  }

  /**
   * Follow a plugin's state (the frame's `plugin.unit` event): load the module folders it names in
   * `shell_dirs` (empty while it is disabled) and unload its modules that are not among them.
   * @param {{ id?: string, shell?: string[], shell_dirs?: string[] }} unit
   */
  async applyUnit(unit) {
    const wanted = (unit.shell_dirs ?? []).map((dir) => path.resolve(dir));
    for (const id of unit.shell ?? []) {
      const record = this.modules.get(id);
      if (record && !wanted.includes(path.resolve(record.dir))) await this.remove(id);
    }
    for (const dir of wanted) await this.add(dir);
  }

  /** @param {string} id */
  async load(id) {
    const record = this.modules.get(id);
    if (!record) return;
    record.generation = (this.generations.get(id) ?? 0) + 1;
    this.generations.set(id, record.generation);
    // Node keeps every imported ES module for the life of the process, keyed by URL. Each
    // generation is imported from its own snapshot of the module's folder, so the entry and every
    // file it imports relatively are new code. The module's own node_modules is linked in.
    const snapshot = path.join(this.cacheDir, `${id.replace(/[^\w.-]/g, '_')}-${record.generation}`);
    fs.rmSync(snapshot, { recursive: true, force: true });
    fs.cpSync(record.dir, snapshot, {
      recursive: true,
      filter: (file) => {
        const first = path.relative(record.dir, file).split(path.sep)[0];
        return first !== 'node_modules' && first !== '.git';
      },
    });
    if (fs.existsSync(path.join(record.dir, 'node_modules'))) fs.symlinkSync(path.join(record.dir, 'node_modules'), path.join(snapshot, 'node_modules'), 'dir');
    const module = await import(pathToFileURL(path.join(snapshot, path.relative(record.dir, record.entry))).href);
    if (typeof module.load !== 'function') throw new Error(`${record.entry} has no load(api)`);
    const api = this.makeApi(id);
    record.release = api.release ?? null;
    if (record.snapshot && record.snapshot !== snapshot) fs.rmSync(record.snapshot, { recursive: true, force: true });
    record.snapshot = snapshot;
    record.loaded = await module.load(api);
    this.log(`shell module ${id} loaded (generation ${record.generation})`);
  }

  /**
   * Swap a module for the code now on disk.
   * @param {string} id
   */
  async reload(id) {
    const record = this.modules.get(id);
    if (!record) return;
    try {
      await record.loaded?.unload();
    } catch (error) {
      this.log(`shell module ${id} unload failed: ${error}`);
    }
    // Frame connections the module opened through its api and did not close end with it.
    record.release?.();
    record.release = null;
    record.loaded = null;
    try {
      await this.load(id);
    } catch (error) {
      this.log(`shell module ${id} reload failed: ${error instanceof Error ? error.stack : String(error)}`);
    }
  }

  async beforeQuit() {
    for (const [id, record] of this.modules) {
      try {
        await record.loaded?.beforeQuit?.();
      } catch (error) {
        this.log(`shell module ${id} beforeQuit failed: ${error}`);
      }
    }
  }

  async stop() {
    for (const record of this.modules.values()) {
      record.watcher?.close();
      if (record.timer) clearTimeout(record.timer);
      try {
        await record.loaded?.unload();
      } catch {
        // Quitting anyway.
      }
      record.release?.();
    }
  }
}
