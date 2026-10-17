/**
 * A small result cache for frame calls, shared by every plugin version.
 *
 * Because the SDK is one shared module, a plugin version mounted hidden during a hot-swap renders
 * from the same cache its predecessor filled, so the slot flips without an empty frame.
 */

import type { Client } from './rpc';

export interface QueryState<T> {
  data: T | undefined;
  error: string | undefined;
  /** The CPP/1 error code of the last failure (`unavailable`: no plugin serves the category). */
  errorCode?: string | undefined;
  loading: boolean;
  /** Milliseconds since epoch of the last successful load. */
  at: number;
}

type Listener = () => void;

interface Entry<T> {
  state: QueryState<T>;
  listeners: Set<Listener>;
  inflight: Promise<void> | null;
  /** A load was asked for while one ran: that one may have read the state before the change. */
  again: boolean;
  fetcher: (client: Client) => Promise<T>;
}

const EMPTY: QueryState<never> = { data: undefined, error: undefined, loading: false, at: 0 };

export class QueryCache {
  private readonly entries = new Map<string, Entry<unknown>>();
  private readonly client: Client;

  constructor(client: Client) {
    this.client = client;
    // A plugin that comes up (or comes back) can answer what failed while it was away.
    client.on('plugin.state', (_topic, data) => {
      if ((data as { state?: string } | null)?.state === 'active') this.retryFailed();
    });
    client.onReconnect(() => this.retryFailed());
  }

  /** Reload every key whose last load failed and that something still shows. */
  retryFailed(): void {
    for (const [key, entry] of this.entries) {
      if (entry.state.error && entry.listeners.size > 0) void this.fetch(key, entry.fetcher);
    }
  }

  get<T>(key: string): QueryState<T> {
    return (this.entries.get(key)?.state as QueryState<T> | undefined) ?? EMPTY;
  }

  subscribe(key: string, listener: Listener): () => void {
    const entry = this.ensure(key, null);
    entry.listeners.add(listener);
    return () => entry.listeners.delete(listener);
  }

  /**
   * Load (or reload) a key with its fetcher. Concurrent loads of one key share one call; a load
   * asked for while one runs makes exactly one more run after it (the running call may have read
   * the state before the change that asked), and the returned promise covers that run too.
   */
  fetch<T>(key: string, fetcher: (client: Client) => Promise<T>): Promise<void> {
    const entry = this.ensure(key, fetcher as (client: Client) => Promise<unknown>);
    entry.fetcher = fetcher as (client: Client) => Promise<unknown>;
    if (entry.inflight) {
      entry.again = true;
      return entry.inflight;
    }
    this.update(entry, { ...entry.state, loading: true });
    const run = (async () => {
      do {
        entry.again = false;
        try {
          const data = await entry.fetcher(this.client);
          this.update(entry, { data, error: undefined, errorCode: undefined, loading: false, at: Date.now() });
        } catch (error) {
          this.update(entry, {
            ...entry.state,
            error: error instanceof Error ? error.message : String(error),
            errorCode: typeof (error as { code?: unknown })?.code === 'string' ? (error as { code: string }).code : undefined,
            loading: false,
          });
        }
      } while (entry.again);
    })().finally(() => {
      entry.inflight = null;
    });
    entry.inflight = run;
    return run;
  }

  /** Reload every loaded key starting with `prefix`. */
  invalidate(prefix: string): void {
    for (const [key, entry] of this.entries) {
      if (key.startsWith(prefix) && entry.state.at > 0) void this.fetch(key, entry.fetcher);
    }
  }

  /** Replace a key's data locally (e.g. from an event that carries the new value). */
  write<T>(key: string, update: (current: T | undefined) => T): void {
    const entry = this.entries.get(key);
    if (!entry) return;
    const data = update(entry.state.data as T | undefined);
    this.update(entry, { ...entry.state, data, at: entry.state.at || Date.now() });
  }

  private ensure(key: string, fetcher: ((client: Client) => Promise<unknown>) | null): Entry<unknown> {
    let entry = this.entries.get(key);
    if (!entry) {
      entry = {
        state: EMPTY,
        listeners: new Set(),
        inflight: null,
        again: false,
        fetcher: fetcher ?? (() => Promise.reject(new Error(`no loader for ${key}`))),
      };
      this.entries.set(key, entry);
    }
    return entry;
  }

  private update(entry: Entry<unknown>, state: QueryState<unknown>): void {
    entry.state = state;
    for (const listener of [...entry.listeners]) listener();
  }
}
