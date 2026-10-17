/**
 * The CPP/1 client over the frame's WebSocket.
 *
 * One JSON envelope per text message (`protocol/src/envelope.rs`): `req`, `res`, `item`, `evt`,
 * `cancel`. The client reconnects on its own; requests made while the socket is down wait for it,
 * and every subscription is renewed after a reconnect so no event stream silently stops.
 */

export type Envelope =
  | { t: 'req'; id: number; method: string; params?: unknown }
  | { t: 'res'; id: number; ok: boolean; result?: unknown; error?: WireError }
  | { t: 'item'; id: number; data: unknown }
  | { t: 'evt'; topic: string; data: unknown }
  | { t: 'cancel'; id: number };

export interface WireError {
  code: string;
  message: string;
  retryable?: boolean;
  data?: unknown;
}

/** A structured protocol error (`RpcError` on the Rust side). */
export class RpcError extends Error {
  readonly code: string;
  readonly retryable: boolean;
  readonly data: unknown;

  constructor(code: string, message: string, retryable = false, data?: unknown) {
    super(message);
    this.name = 'RpcError';
    this.code = code;
    this.retryable = retryable;
    this.data = data;
  }

  static from(error: WireError): RpcError {
    return new RpcError(error.code, error.message, Boolean(error.retryable), error.data);
  }
}

export type ConnectionState = 'connecting' | 'open' | 'closed';

export interface CallOptions {
  signal?: AbortSignal;
  /** Stream items of the request, in order. */
  onItem?: (item: unknown) => void;
}

type EventHandler = (topic: string, data: unknown) => void;

interface Pending {
  resolve: (value: unknown) => void;
  reject: (error: RpcError) => void;
  onItem?: (item: unknown) => void;
}

/** Minimal surface of the WebSocket the client needs (lets tests drive it without a socket). */
export interface SocketLike {
  readyState: number;
  send(data: string): void;
  close(): void;
  onopen: ((event: unknown) => void) | null;
  onclose: ((event: unknown) => void) | null;
  onerror: ((event: unknown) => void) | null;
  onmessage: ((event: { data: unknown }) => void) | null;
}

export interface ClientOptions {
  url: string;
  /** Creates the socket; defaults to the browser WebSocket. */
  socket?: (url: string) => SocketLike;
}

const OPEN = 1;

export class Client {
  private readonly url: string;
  private readonly makeSocket: (url: string) => SocketLike;
  private socket: SocketLike | null = null;
  private nextId = 1;
  private readonly pending = new Map<number, Pending>();
  private readonly outbox: string[] = [];
  private readonly handlers = new Map<string, Set<EventHandler>>();
  private readonly stateListeners = new Set<(state: ConnectionState) => void>();
  private readonly reconnectListeners = new Set<() => void>();
  private retryMs = 250;
  private retryTimer: ReturnType<typeof setTimeout> | null = null;
  private stopped = false;
  private everOpened = false;
  private subscribedTopics = '';
  state: ConnectionState = 'connecting';

  constructor(options: ClientOptions) {
    this.url = options.url;
    this.makeSocket = options.socket ?? ((url) => new WebSocket(url) as unknown as SocketLike);
    this.open();
  }

  /** Call a method; resolves with its result or rejects with an {@link RpcError}. */
  call<T = unknown>(method: string, params: unknown = {}, options: CallOptions = {}): Promise<T> {
    const id = this.nextId++;
    return new Promise<T>((resolve, reject) => {
      if (options.signal?.aborted) {
        reject(new RpcError('cancelled', 'request cancelled'));
        return;
      }
      this.pending.set(id, {
        resolve: resolve as (value: unknown) => void,
        reject,
        onItem: options.onItem,
      });
      options.signal?.addEventListener('abort', () => {
        if (!this.pending.has(id)) return;
        this.pending.delete(id);
        this.send({ t: 'cancel', id });
        reject(new RpcError('cancelled', 'request cancelled'));
      }, { once: true });
      this.send({ t: 'req', id, method, params });
    });
  }

  /** Publish an event through the frame (fanned out to subscribers). */
  publish(topic: string, data: unknown): void {
    const text = JSON.stringify({ t: 'evt', topic, data });
    if (this.socket && this.socket.readyState === OPEN) this.socket.send(text);
  }

  /**
   * Listen to events whose topic starts with `prefix`. The frame is told about every prefix
   * anyone listens to; the returned function stops this listener.
   */
  on(prefix: string, handler: EventHandler): () => void {
    let set = this.handlers.get(prefix);
    if (!set) {
      set = new Set();
      this.handlers.set(prefix, set);
    }
    set.add(handler);
    this.syncSubscriptions();
    return () => {
      const current = this.handlers.get(prefix);
      if (!current) return;
      current.delete(handler);
      if (current.size === 0) this.handlers.delete(prefix);
    };
  }

  onState(listener: (state: ConnectionState) => void): () => void {
    this.stateListeners.add(listener);
    return () => this.stateListeners.delete(listener);
  }

  /** Called after the socket came back from a drop (not on the first open). */
  onReconnect(listener: () => void): () => void {
    this.reconnectListeners.add(listener);
    return () => this.reconnectListeners.delete(listener);
  }

  close(): void {
    this.stopped = true;
    if (this.retryTimer) clearTimeout(this.retryTimer);
    this.socket?.close();
    this.fail(new RpcError('connection_closed', 'client closed', false));
  }

  private open(): void {
    if (this.stopped) return;
    this.setState('connecting');
    const socket = this.makeSocket(this.url);
    this.socket = socket;
    socket.onopen = () => {
      const reconnect = this.everOpened;
      this.everOpened = true;
      this.retryMs = 250;
      this.setState('open');
      // The frame forgets a client's subscriptions with its socket: renew all of them first.
      this.subscribedTopics = '';
      this.syncSubscriptions();
      for (const frame of this.outbox.splice(0)) socket.send(frame);
      if (reconnect) for (const listener of this.reconnectListeners) listener();
    };
    socket.onmessage = (event) => this.receive(event.data);
    socket.onerror = () => {
      // `onclose` follows and handles the retry.
    };
    socket.onclose = () => {
      if (this.socket !== socket) return;
      this.socket = null;
      this.fail(new RpcError('connection_closed', 'connection to the frame closed', true));
      if (this.stopped) {
        this.setState('closed');
        return;
      }
      this.setState('connecting');
      this.retryTimer = setTimeout(() => this.open(), this.retryMs);
      this.retryMs = Math.min(this.retryMs * 2, 4000);
    };
  }

  private setState(state: ConnectionState): void {
    if (this.state === state) return;
    this.state = state;
    for (const listener of this.stateListeners) listener(state);
  }

  private fail(error: RpcError): void {
    const pending = [...this.pending.values()];
    this.pending.clear();
    for (const entry of pending) entry.reject(error);
  }

  private send(envelope: Envelope): void {
    const text = JSON.stringify(envelope);
    if (this.socket && this.socket.readyState === OPEN) this.socket.send(text);
    else if (envelope.t === 'req') this.outbox.push(text);
    // A cancel for a request that never left is moot: the request is still in the outbox only if
    // the socket never opened, and then it was rejected locally already.
  }

  private syncSubscriptions(): void {
    const topics = [...this.handlers.keys()].sort();
    const key = topics.join('\n');
    if (key === this.subscribedTopics) return;
    this.subscribedTopics = key;
    if (topics.length === 0) return;
    // The full set every time: correct whether the frame treats the call as additive or as a
    // replacement of this client's subscriptions.
    this.call('frame.subscribe', { topics }).catch(() => {
      // Retried on the next change or reconnect.
      this.subscribedTopics = '';
    });
  }

  private receive(raw: unknown): void {
    if (typeof raw !== 'string') return;
    let envelope: Envelope;
    try {
      envelope = JSON.parse(raw) as Envelope;
    } catch {
      return;
    }
    switch (envelope.t) {
      case 'res': {
        const entry = this.pending.get(envelope.id);
        if (!entry) return;
        this.pending.delete(envelope.id);
        if (envelope.ok) entry.resolve(envelope.result ?? null);
        else entry.reject(RpcError.from(envelope.error ?? { code: 'internal', message: 'error without detail' }));
        return;
      }
      case 'item':
        this.pending.get(envelope.id)?.onItem?.(envelope.data);
        return;
      case 'evt':
        this.dispatch(envelope.topic, envelope.data);
        return;
      case 'req':
        // The workbench serves no methods over this socket.
        this.socket?.send(JSON.stringify({
          t: 'res',
          id: envelope.id,
          ok: false,
          error: { code: 'unknown_method', message: `unknown method \`${envelope.method}\``, retryable: false },
        }));
        return;
      default:
        return;
    }
  }

  private dispatch(topic: string, data: unknown): void {
    for (const [prefix, set] of this.handlers) {
      if (!topic.startsWith(prefix)) continue;
      for (const handler of [...set]) {
        try {
          handler(topic, data);
        } catch (error) {
          console.error(`event handler for ${prefix} failed`, error);
        }
      }
    }
  }
}
