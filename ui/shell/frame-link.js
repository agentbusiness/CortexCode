// @ts-check
/**
 * The shell's own connection to the frame: a CPP/1 peer over the frame's WebSocket that says
 * `frame.hello` as a category (the shell serves `browser@1`) and answers the frame's requests,
 * including the standard plugin lifecycle (health, probe, activate, drain, shutdown, state).
 *
 * It reconnects on its own and says hello again, so a frame restart re-attaches the browser.
 */

import WebSocket from 'ws';

/**
 * @typedef {{ item: (data: unknown) => void, signal: AbortSignal }} CallContext
 * @typedef {(method: string, params: any, context: CallContext) => Promise<unknown>} Handler
 * @typedef {{ code: string, message: string, retryable?: boolean, data?: unknown }} WireError
 */

export class RpcError extends Error {
  /**
   * @param {string} code
   * @param {string} message
   * @param {boolean} [retryable]
   */
  constructor(code, message, retryable = false) {
    super(message);
    this.code = code;
    this.retryable = retryable;
  }
}

export class FrameLink {
  /**
   * `hello: null` makes a plain client (it serves nothing; it calls, subscribes and publishes).
   * @param {{ url: string, token: string, hello: Record<string, unknown> | null, handler?: Handler,
   *   log?: (line: string) => void, onHello?: (ack: unknown) => void }} options
   *   (`onHello` runs on every (re)connection once the frame accepted it; a plain client gets null.)
   */
  constructor(options) {
    this.url = options.url.replace(/^http/, 'ws').replace(/\/+$/, '') + `/ws?token=${encodeURIComponent(options.token)}`;
    this.hello = options.hello;
    this.handler = options.handler ?? (async (/** @type {string} */ method) => {
      throw new RpcError('unknown_method', `unknown method \`${method}\``);
    });
    /** @type {Map<string, Set<(topic: string, data: unknown) => void>>} */
    this.listeners = new Map();
    this.log = options.log ?? (() => undefined);
    this.onHello = options.onHello ?? (() => undefined);
    /** @type {WebSocket | null} */
    this.socket = null;
    this.nextId = 1;
    /** @type {Map<number, { resolve: (v: unknown) => void, reject: (e: Error) => void }>} */
    this.pending = new Map();
    /** @type {Map<number, AbortController>} */
    this.inflight = new Map();
    this.retryMs = 250;
    this.stopped = false;
    this.draining = false;
    this.startedAt = Date.now();
    this.accepted = false;
  }

  start() {
    this.open();
  }

  stop() {
    this.stopped = true;
    for (const controller of this.inflight.values()) controller.abort();
    this.socket?.close();
  }

  /**
   * Call a method on the frame.
   * @param {string} method
   * @param {unknown} params
   */
  call(method, params) {
    return new Promise((resolve, reject) => {
      const socket = this.socket;
      if (!socket || socket.readyState !== WebSocket.OPEN) {
        reject(new RpcError('connection_closed', 'not connected to the frame', true));
        return;
      }
      const id = this.nextId++;
      this.pending.set(id, { resolve, reject });
      socket.send(JSON.stringify({ t: 'req', id, method, params }));
    });
  }

  /**
   * Publish an event through the frame.
   * @param {string} topic
   * @param {unknown} data
   */
  publish(topic, data) {
    const socket = this.socket;
    if (socket && socket.readyState === WebSocket.OPEN && this.accepted) {
      socket.send(JSON.stringify({ t: 'evt', topic, data }));
    }
  }

  /**
   * Listen to events whose topic starts with `prefix` (the frame is told on every (re)connect).
   * @param {string} prefix
   * @param {(topic: string, data: unknown) => void} listener
   */
  on(prefix, listener) {
    let set = this.listeners.get(prefix);
    if (!set) {
      set = new Set();
      this.listeners.set(prefix, set);
    }
    set.add(listener);
    this.resubscribe();
    return () => set?.delete(listener);
  }

  resubscribe() {
    const topics = [...this.listeners.keys()];
    if (topics.length && this.accepted) this.call('frame.subscribe', { topics }).catch((error) => this.log(`subscribe: ${error.message}`));
  }

  inFlight() {
    return this.inflight.size;
  }

  open() {
    if (this.stopped) return;
    const socket = new WebSocket(this.url);
    this.socket = socket;
    socket.on('open', async () => {
      this.retryMs = 250;
      if (!this.hello) {
        this.accepted = true;
        this.resubscribe();
        this.onHello(null);
        return;
      }
      try {
        const ack = /** @type {{ accepted?: boolean, reason?: string }} */ (await this.call('frame.hello', { ...this.hello, pid: process.pid }));
        if (ack && ack.accepted === false) {
          this.log(`frame rejected the hello: ${ack.reason ?? ''}`);
          return;
        }
        this.accepted = true;
        this.log(`serving ${String(this.hello.contract ?? this.hello.category)} for the frame`);
        this.resubscribe();
        this.onHello(ack);
      } catch (error) {
        this.log(`hello failed: ${error instanceof Error ? error.message : String(error)}`);
      }
    });
    socket.on('message', (raw) => this.receive(String(raw)));
    socket.on('error', (error) => this.log(`frame socket error: ${error.message}`));
    socket.on('close', () => {
      if (this.socket !== socket) return;
      this.socket = null;
      this.accepted = false;
      for (const entry of this.pending.values()) entry.reject(new RpcError('connection_closed', 'connection to the frame closed', true));
      this.pending.clear();
      for (const controller of this.inflight.values()) controller.abort();
      this.inflight.clear();
      if (this.stopped) return;
      setTimeout(() => this.open(), this.retryMs);
      this.retryMs = Math.min(this.retryMs * 2, 4000);
    });
  }

  /** @param {string} raw */
  receive(raw) {
    /** @type {any} */
    let message;
    try {
      message = JSON.parse(raw);
    } catch {
      return;
    }
    switch (message?.t) {
      case 'res': {
        const entry = this.pending.get(message.id);
        if (!entry) return;
        this.pending.delete(message.id);
        if (message.ok) entry.resolve(message.result ?? null);
        else entry.reject(new RpcError(message.error?.code ?? 'internal', message.error?.message ?? 'error', Boolean(message.error?.retryable)));
        return;
      }
      case 'req':
        void this.serve(message.id, message.method, message.params ?? {});
        return;
      case 'cancel':
        this.inflight.get(message.id)?.abort();
        return;
      case 'evt':
        for (const [prefix, set] of this.listeners) {
          if (typeof message.topic === 'string' && message.topic.startsWith(prefix)) for (const listener of [...set]) listener(message.topic, message.data);
        }
        return;
      default:
        return;
    }
  }

  /**
   * @param {number} id
   * @param {string} method
   * @param {unknown} params
   */
  async serve(id, method, params) {
    const controller = new AbortController();
    this.inflight.set(id, controller);
    const socket = this.socket;
    const send = (/** @type {unknown} */ message) => {
      if (socket && socket.readyState === WebSocket.OPEN) socket.send(JSON.stringify(message));
    };
    try {
      let result;
      if (method === 'plugin.health') {
        result = { status: this.draining ? 'draining' : 'ok', in_flight: this.inflight.size - 1, uptime_ms: Date.now() - this.startedAt, pid: process.pid };
      } else if (method === 'plugin.drain') {
        this.draining = true;
        const limit = Number(/** @type {any} */ (params)?.timeout_ms ?? 30_000);
        const deadline = Date.now() + limit;
        while (this.inflight.size > 1 && Date.now() < deadline) await new Promise((r) => setTimeout(r, 25));
        result = { remaining: this.inflight.size - 1 };
      } else if (method === 'plugin.export_state') {
        result = null;
      } else if (method === 'plugin.import_state' || method === 'plugin.activate') {
        this.draining = false;
        result = null;
      } else if (this.draining && !method.startsWith('plugin.')) {
        throw new RpcError('draining', 'the browser is draining', true);
      } else {
        result = await this.handler(method, params, {
          item: (data) => send({ t: 'item', id, data }),
          signal: controller.signal,
        });
      }
      if (controller.signal.aborted) {
        send({ t: 'res', id, ok: false, error: { code: 'cancelled', message: 'request cancelled', retryable: false } });
      } else {
        send({ t: 'res', id, ok: true, result: result ?? null });
      }
    } catch (error) {
      // Any error that carries a CPP/1 code keeps it (shell modules bring their own error classes).
      const coded = /** @type {{ code?: unknown, retryable?: unknown }} */ (error ?? {});
      const code = typeof coded.code === 'string' ? coded.code : 'internal';
      const retryable = coded.retryable === true;
      send({ t: 'res', id, ok: false, error: { code, message: error instanceof Error ? error.message : String(error), retryable } });
    } finally {
      this.inflight.delete(id);
    }
  }
}
