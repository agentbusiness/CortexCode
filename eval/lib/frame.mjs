// Headless frame lifecycle and a CPP/1 WebSocket client (one JSON envelope per text message,
// see protocol/src/envelope.rs).
import fs from 'node:fs';
import fsp from 'node:fs/promises';
import os from 'node:os';
import path from 'node:path';
import { execFileSync, spawn } from 'node:child_process';
import { DEV_CONFIG, FRAME_BIN, REPO_ROOT } from './paths.mjs';
import { parseToml, stringifyToml } from './toml.mjs';
import { ensureDir, killGroup, log, readJson, sleep, writeText } from './util.mjs';

export class RpcError extends Error {
  constructor(method, error) {
    super(`${method}: ${error?.code ?? 'error'}: ${error?.message ?? JSON.stringify(error)}`);
    this.code = error?.code;
    this.retryable = !!error?.retryable;
    this.data = error?.data;
  }
}

export class FrameClient {
  constructor(url) {
    this.url = url;
    this.nextId = 1;
    this.pending = new Map();
    this.listeners = [];
    this.closed = false;
  }

  async connect(timeoutMs = 30000) {
    this.ws = new WebSocket(this.url);
    await new Promise((resolve, reject) => {
      const timer = setTimeout(() => reject(new Error(`websocket connect timeout ${this.url}`)), timeoutMs);
      this.ws.addEventListener('open', () => { clearTimeout(timer); resolve(); }, { once: true });
      this.ws.addEventListener('error', (e) => { clearTimeout(timer); reject(new Error(`websocket error: ${e.message ?? 'connect failed'}`)); }, { once: true });
    });
    this.ws.addEventListener('message', (ev) => this.onMessage(ev.data));
    this.ws.addEventListener('close', () => {
      this.closed = true;
      for (const [, p] of this.pending) p.reject(new RpcError(p.method, { code: 'connection_closed', message: 'connection closed', retryable: true }));
      this.pending.clear();
    });
    return this;
  }

  onMessage(raw) {
    let msg;
    try { msg = JSON.parse(typeof raw === 'string' ? raw : Buffer.from(raw).toString('utf8')); } catch { return; }
    if (msg.t === 'res') {
      const p = this.pending.get(msg.id);
      if (!p) return;
      this.pending.delete(msg.id);
      clearTimeout(p.timer);
      if (msg.ok) p.resolve(msg.result ?? null);
      else p.reject(new RpcError(p.method, msg.error));
    } else if (msg.t === 'item') {
      this.pending.get(msg.id)?.onItem?.(msg.data);
    } else if (msg.t === 'evt') {
      for (const l of this.listeners) {
        if (msg.topic.startsWith(l.prefix)) {
          try { l.fn(msg.topic, msg.data); } catch (error) { log('event listener error', error); }
        }
      }
    } else if (msg.t === 'req') {
      // This client serves no category; answer so the frame never waits on us.
      this.send({ t: 'res', id: msg.id, ok: false, error: { code: 'unknown_method', message: `eval client serves no methods (${msg.method})`, retryable: false } });
    }
  }

  send(obj) {
    if (this.closed) throw new Error('websocket closed');
    this.ws.send(JSON.stringify(obj));
  }

  request(method, params = {}, { timeoutMs = 600000, onItem } = {}) {
    const id = this.nextId++;
    return new Promise((resolve, reject) => {
      const timer = timeoutMs ? setTimeout(() => {
        this.pending.delete(id);
        try { this.send({ t: 'cancel', id }); } catch { /* closed */ }
        reject(new RpcError(method, { code: 'timeout', message: `no response in ${timeoutMs} ms` }));
      }, timeoutMs) : null;
      this.pending.set(id, { resolve, reject, onItem, timer, method });
      try { this.send({ t: 'req', id, method, params }); } catch (error) {
        clearTimeout(timer);
        this.pending.delete(id);
        reject(error);
      }
    });
  }

  on(prefix, fn) {
    const l = { prefix, fn };
    this.listeners.push(l);
    return () => { this.listeners = this.listeners.filter((x) => x !== l); };
  }

  close() {
    try { this.ws?.close(); } catch { /* ignore */ }
  }
}

// plugin.toml is read by cortex-pack, the same validating parser the frame uses (`cortex-pack info`).
const PACK = path.join(REPO_ROOT, 'packer', 'target', 'release', 'cortex-pack');

function readPluginToml(dir) {
  if (!fs.existsSync(PACK)) throw new Error(`cortex-pack missing (${PACK}): build it with cargo build --release in packer/`);
  const out = execFileSync(PACK, ['info', dir], { encoding: 'utf8' });
  return JSON.parse(out.slice(out.indexOf('{')));
}

/** Attach a value the harness reads but the frame config must not contain (not serialized). */
function hidden(target, key, value) {
  Object.defineProperty(target, key, { value, enumerable: false, configurable: true, writable: true });
}

/** The app's home, whose installed packages hidden frames run (`CORTEX_HOME`, else ~/.cortexcode). */
export function appHome() {
  return process.env.CORTEX_HOME || path.join(os.homedir(), '.cortexcode');
}

/**
 * The plugins installed in `home` (`<home>/state/plugins.json`, `<home>/plugins/<id>/<version>/`):
 * `{ id, version, dir, category, dataName, config }`, where `dataName` is the plugin's data folder
 * under `plugins-data` (the packed executable's name) and `config` its `[backend.config]` defaults.
 */
export function installedPlugins(home = appHome()) {
  const state = readJsonSync(path.join(home, 'state', 'plugins.json')) ?? {};
  return Object.entries(state.installed ?? {}).flatMap(([id, record]) => {
    const dir = path.join(home, 'plugins', id, record.current);
    const manifest = readJsonSync(path.join(dir, 'manifest.json'));
    if (!manifest) return [];
    const backend = manifest.backend ?? null;
    return [{ id, version: record.current, dir, category: backend?.category ?? null, dataName: backend ? path.basename(backend.exec) : null, config: backend?.config ?? null }];
  });
}

function readJsonSync(file) {
  try { return JSON.parse(fs.readFileSync(file, 'utf8')); } catch { return null; }
}

/**
 * Build a headless frame config from the repo's dev config: the app's installed plugins (seeded
 * into the private home by `Frame.start`), a private CORTEX_HOME, no UI. `pluginConfig` merges
 * extra keys into `[plugin.<id>.config]` per category (e.g. the model variant); `categories`
 * restricts the plugins to those backend categories. The installed plugins are attached as the
 * hidden field `installed`.
 */
export function buildFrameConfig({ home, pluginConfig = {}, categories = null, devConfigPath = DEV_CONFIG }) {
  const dev = parseToml(fs.readFileSync(devConfigPath, 'utf8'));
  const config = { frame: { ...(dev.frame ?? {}), home, port: 0, ui: false } };
  if ('ui_dir' in config.frame) delete config.frame.ui_dir;
  config.plugin = { ...(dev.plugin ?? {}) };
  let installed = installedPlugins();
  if (categories) installed = installed.filter((p) => categories.includes(p.category));
  for (const p of installed) {
    const extra = p.category ? pluginConfig[p.category] : null;
    if (extra) config.plugin[p.id] = { ...(config.plugin[p.id] ?? {}), config: { ...(config.plugin[p.id]?.config ?? {}), ...extra } };
  }
  hidden(config, 'installed', installed);
  // Nothing overridden: the frame runs the product's own config file as it is (owner: the test
  // setup is the normal one), only with a private home and headless.
  if (!Object.keys(pluginConfig).length) hidden(config, 'asIs', devConfigPath);
  return config;
}

/** The value a plugin folder's plugin.toml gives `[backend.config] <key>` (e.g. the model's variant). */
export function folderDefault(dir, key) {
  return readPluginToml(path.resolve(REPO_ROOT, dir)).backend?.config?.[key];
}

/** Hard-link copy of a directory tree (packages are never written in place, so links are safe). */
function linkTree(from, to) {
  fs.mkdirSync(to, { recursive: true });
  for (const entry of fs.readdirSync(from, { withFileTypes: true })) {
    const source = path.join(from, entry.name);
    const target = path.join(to, entry.name);
    if (entry.isDirectory()) linkTree(source, target);
    else if (entry.isFile() && !fs.existsSync(target)) fs.linkSync(source, target);
  }
}

/** Give a private home the installed plugins: their package versions and a plugins.json. */
export function seedPlugins(home, installed) {
  if (!installed.length) throw new Error(`no plugins are installed in ${appHome()}: start ./cortexcode once (it installs them), or cortex-pack build plugins/<name> --install`);
  for (const p of installed) linkTree(p.dir, path.join(home, 'plugins', p.id, p.version));
  const state = { disabled: [], installed: Object.fromEntries(installed.map((p) => [p.id, { current: p.version }])) };
  fs.mkdirSync(path.join(home, 'state'), { recursive: true });
  fs.writeFileSync(path.join(home, 'state', 'plugins.json'), JSON.stringify(state, null, 2));
}

export class Frame {
  /**
   * @param {object} o
   * @param {string} o.runDir  directory for config, logs and the frame home
   * @param {object} o.config  config object from buildFrameConfig
   * @param {string|null} o.sandboxProfile  profile file, or null to run unconfined
   * @param {object} o.env  extra environment for the frame (inherited by plugins)
   */
  constructor({ runDir, config, sandboxProfile, env = {} }) {
    this.runDir = runDir;
    this.config = config;
    this.home = config.frame.home;
    this.sandboxProfile = sandboxProfile;
    this.env = env;
  }

  async start({ readyTimeoutMs = 1800000 } = {}) {
    if (!fs.existsSync(FRAME_BIN)) throw new Error(`frame binary missing: ${FRAME_BIN}`);
    await ensureDir(this.runDir);
    await ensureDir(this.home);
    seedPlugins(this.home, this.config.installed ?? []);
    this.configPath = this.config.asIs ?? path.join(this.runDir, 'frame.toml');
    if (!this.config.asIs) await writeText(this.configPath, stringifyToml(this.config));
    this.logPath = path.join(this.runDir, 'frame.log');
    const logFd = fs.openSync(this.logPath, 'a');
    const args = ['--headless', '--config', this.configPath, ...(this.config.asIs ? ['--home', this.home] : [])];
    const env = { ...process.env, ...this.env, CORTEX_HOME: this.home };
    let cmd = FRAME_BIN;
    let cmdArgs = args;
    if (this.sandboxProfile) {
      cmd = '/usr/bin/sandbox-exec';
      cmdArgs = ['-f', this.sandboxProfile, FRAME_BIN, ...args];
    }
    this.startedMs = Date.now();
    this.proc = spawn(cmd, cmdArgs, { cwd: REPO_ROOT, env, detached: true, stdio: ['ignore', logFd, logFd] });
    fs.closeSync(logFd);
    this.exited = null;
    this.proc.on('exit', (code, signal) => { this.exited = { code, signal }; });
    const cleanup = () => this.killNow();
    this.cleanupHook = cleanup;
    process.on('exit', cleanup);

    const infoPath = path.join(this.home, 'run', 'frame.json');
    const deadline = Date.now() + 120000;
    let info = null;
    while (Date.now() < deadline) {
      if (this.exited) throw new Error(`frame exited during startup (${JSON.stringify(this.exited)}); log: ${this.logPath}\n${await this.logTail()}`);
      info = await readJson(infoPath, null);
      if (info?.port && info?.token) {
        try {
          const r = await fetch(`http://127.0.0.1:${info.port}/health`);
          if (r.ok) break;
        } catch { /* not listening yet */ }
      }
      await sleep(250);
    }
    if (!info?.port) throw new Error(`frame did not write ${infoPath}; log: ${this.logPath}\n${await this.logTail()}`);
    this.info = info;
    this.client = await new FrameClient(`ws://127.0.0.1:${info.port}/ws?token=${encodeURIComponent(info.token)}`).connect();
    await this.waitPluginsActive(readyTimeoutMs);
    return this;
  }

  async logTail(n = 3000) {
    try { const t = await fsp.readFile(this.logPath, 'utf8'); return t.slice(-n); } catch { return ''; }
  }

  async plugins() {
    const r = await this.client.request('frame.plugins.list', {});
    return Array.isArray(r) ? r : (r?.plugins ?? []);
  }

  /** Wait until every configured category has an active instance (model weights load here). */
  async waitPluginsActive(timeoutMs) {
    // UI-only plugins have no backend process: only backend categories become active.
    const want = new Set((this.config.installed ?? []).map((p) => p.category).filter(Boolean));
    const deadline = Date.now() + timeoutMs;
    let last = [];
    while (Date.now() < deadline) {
      if (this.exited) throw new Error(`frame exited while plugins started; log tail:\n${await this.logTail()}`);
      try {
        last = await this.plugins();
        const active = new Set(last.filter((p) => p.state === 'active').map((p) => p.category));
        if ([...want].every((c) => active.has(c))) {
          this.pluginInfo = last;
          return last;
        }
      } catch (error) {
        if (this.client.closed) throw error;
      }
      await sleep(1000);
    }
    throw new Error(`plugins not active after ${timeoutMs} ms: ${JSON.stringify(last.map((p) => [p.category, p.state]))}`);
  }

  async stop() {
    try { this.client?.close(); } catch { /* ignore */ }
    if (this.proc && !this.exited) {
      killGroup(this.proc.pid, 'SIGTERM');
      const deadline = Date.now() + 20000;
      while (!this.exited && Date.now() < deadline) await sleep(200);
    }
    this.killNow();
    if (this.cleanupHook) process.off('exit', this.cleanupHook);
  }

  killNow() {
    if (this.proc) killGroup(this.proc.pid, 'SIGKILL');
    for (const p of this.pluginInfo ?? []) {
      if (p.pid) { try { process.kill(p.pid, 'SIGKILL'); } catch { /* gone */ } }
    }
  }
}

