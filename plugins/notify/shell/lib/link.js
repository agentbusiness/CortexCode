// @ts-check
/**
 * The notify shell module's own connection to the frame: a CPP/1 client over the frame's WebSocket
 * (one JSON envelope per message). It subscribes to the notify topics, calls notify methods and
 * publishes the focus state. It reconnects on its own (the frame may restart with a new port and
 * token, so the address is resolved again on every attempt).
 *
 * The shell's `api.serve()` is for serving a category; this module serves none (cc-notify serves
 * `notify`), it is a client like the workbench, so it keeps its own link.
 */

/**
 * @typedef {{ url: string, token: string }} FrameAddress
 * @typedef {{ new (url: string): WebSocketLike }} WebSocketCtor
 * @typedef {{ readyState: number, send(data: string): void, close(): void,
 *   addEventListener(type: string, listener: (event: any) => void): void }} WebSocketLike
 */

const OPEN = 1;

export class FrameClient {
  /**
   * @param {{
   *   address: () => FrameAddress | null,
   *   topics: string[],
   *   WebSocketImpl?: WebSocketCtor,
   *   log?: (line: string) => void,
   *   onOpen?: () => void,
   *   onEvent?: (topic: string, data: any) => void,
   *   setTimer?: (fn: () => void, ms: number) => any,
   *   clearTimer?: (handle: any) => void,
   * }} options
   */
  constructor(options) {
    this.address = options.address;
    this.topics = options.topics;
    this.WebSocketImpl = options.WebSocketImpl ?? /** @type {WebSocketCtor} */ (/** @type {any} */ (globalThis).WebSocket);
    this.log = options.log ?? (() => undefined);
    this.onOpen = options.onOpen ?? (() => undefined);
    this.onEvent = options.onEvent ?? (() => undefined);
    this.setTimer = options.setTimer ?? ((fn, ms) => setTimeout(fn, ms));
    this.clearTimer = options.clearTimer ?? ((handle) => clearTimeout(handle));
    /** @type {WebSocketLike | null} */
    this.socket = null;
    this.ready = false;
    this.stopped = false;
    this.nextId = 1;
    this.retryMs = 250;
    /** @type {Map<number, { resolve: (value: any) => void, reject: (error: Error) => void, timer: any }>} */
    this.pending = new Map();
    /** @type {any} */
    this.retryTimer = null;
  }

  start() {
    this.stopped = false;
    this.open();
  }

  stop() {
    this.stopped = true;
    if (this.retryTimer) this.clearTimer(this.retryTimer);
    this.retryTimer = null;
    this.failPending('the link was stopped');
    this.socket?.close();
    this.socket = null;
    this.ready = false;
  }

  /** @param {string} reason */
  failPending(reason) {
    for (const [id, entry] of this.pending) {
      this.clearTimer(entry.timer);
      entry.reject(Object.assign(new Error(reason), { code: 'connection_closed', retryable: true }));
      this.pending.delete(id);
    }
  }

  open() {
    if (this.stopped) return;
    const address = this.address();
    if (!address) {
      this.retryLater();
      return;
    }
    const url = `${address.url.replace(/^http/, 'ws').replace(/\/+$/, '')}/ws?token=${encodeURIComponent(address.token)}`;
    let socket;
    try {
      socket = new this.WebSocketImpl(url);
    } catch (error) {
      this.log(`cannot open the frame socket: ${error instanceof Error ? error.message : String(error)}`);
      this.retryLater();
      return;
    }
    this.socket = socket;
    socket.addEventListener('open', () => {
      if (this.socket !== socket) return;
      this.retryMs = 250;
      this.call('frame.subscribe', { topics: this.topics })
        .then(() => {
          this.ready = true;
          this.onOpen();
        })
        .catch((error) => this.log(`subscribe failed: ${error.message}`));
    });
    socket.addEventListener('message', (event) => this.receive(String(event.data)));
    socket.addEventListener('error', () => undefined);
    socket.addEventListener('close', () => {
      if (this.socket !== socket) return;
      this.socket = null;
      this.ready = false;
      this.failPending('connection to the frame closed');
      this.retryLater();
    });
  }

  retryLater() {
    if (this.stopped || this.retryTimer) return;
    this.retryTimer = this.setTimer(() => {
      this.retryTimer = null;
      this.open();
    }, this.retryMs);
    this.retryMs = Math.min(this.retryMs * 2, 4000);
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
    if (message?.t === 'res') {
      const entry = this.pending.get(message.id);
      if (!entry) return;
      this.pending.delete(message.id);
      this.clearTimer(entry.timer);
      if (message.ok) entry.resolve(message.result ?? null);
      else entry.reject(Object.assign(new Error(message.error?.message ?? 'error'), { code: message.error?.code ?? 'internal' }));
    } else if (message?.t === 'evt' && typeof message.topic === 'string') {
      try {
        this.onEvent(message.topic, message.data);
      } catch (error) {
        this.log(`event handler failed: ${error instanceof Error ? error.stack : String(error)}`);
      }
    } else if (message?.t === 'req') {
      // Nobody calls a plain client; answer so the caller is not left waiting.
      this.send({ t: 'res', id: message.id, ok: false, error: { code: 'unknown_method', message: `${message.method} is not served here` } });
    }
  }

  /** @param {unknown} message */
  send(message) {
    const socket = this.socket;
    if (!socket || socket.readyState !== OPEN) return false;
    socket.send(JSON.stringify(message));
    return true;
  }

  /**
   * @param {string} method
   * @param {unknown} params
   * @param {number} [timeoutMs]
   * @returns {Promise<any>}
   */
  call(method, params, timeoutMs = 10000) {
    return new Promise((resolve, reject) => {
      const id = this.nextId++;
      const timer = this.setTimer(() => {
        if (this.pending.delete(id)) reject(Object.assign(new Error(`${method} timed out after ${timeoutMs} ms`), { code: 'timeout' }));
      }, timeoutMs);
      this.pending.set(id, { resolve, reject, timer });
      if (!this.send({ t: 'req', id, method, params })) {
        this.pending.delete(id);
        this.clearTimer(timer);
        reject(Object.assign(new Error('not connected to the frame'), { code: 'connection_closed', retryable: true }));
      }
    });
  }

  /**
   * @param {string} topic
   * @param {unknown} data
   */
  publish(topic, data) {
    return this.ready && this.send({ t: 'evt', topic, data });
  }
}
