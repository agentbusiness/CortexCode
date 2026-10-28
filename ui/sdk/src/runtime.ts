/**
 * The workbench runtime shared by the host and every UI plugin: a connection, a journal registry
 * and a query cache per computer, and one durable state store. The host installs this computer's
 * at boot; another computer's is opened on first use, through the link the mesh gives to that
 * computer's frame. The project shown decides which computer the regions talk to (its sessions,
 * files and memory live there); a `ComputerScope` names one for a subtree (the sidebar's rows).
 * The SDK module is shared through the import map, so every plugin version sees the same instances.
 */

import { createContext, createElement, useCallback, useContext, useEffect, useMemo, useReducer, useRef, useState, useSyncExternalStore } from 'react';
import type { ComponentType, ReactNode } from 'react';
import { JournalRegistry, type JournalSnapshot } from './journal';
import { QueryCache, type QueryState } from './query';
import { Client, type ConnectionState } from './rpc';
import { SlotStateStore } from './store';
import type { UiPlugin } from './types';

export interface Runtime {
  /** The computer this runtime talks to: null for this one, else the member's node. */
  node: string | null;
  client: Client;
  journals: JournalRegistry;
  queries: QueryCache;
  state: SlotStateStore;
}

let installed: Runtime | null = null;
const computers = new Map<string, Runtime>();

/** Create the runtime for a frame connection (host only). */
export function createRuntime(url: string): Runtime {
  const client = new Client({ url });
  return { node: null, client, journals: new JournalRegistry(client), queries: new QueryCache(client), state: new SlotStateStore() };
}

export function installRuntime(runtime: Runtime): void {
  installed = runtime;
}

/** This computer's runtime: its frame, the layout and the network's project list. */
export function ownRuntime(): Runtime {
  if (!installed) throw new Error('the CortexCode runtime is not installed (the workbench host installs it at boot)');
  return installed;
}

/** A computer's runtime (null: this one). Another's connects through the mesh's link to its
 * frame, asked again at every reconnection; the durable state stays this computer's. */
export function runtimeFor(node: string | null | undefined): Runtime {
  const own = ownRuntime();
  if (!node) return own;
  let found = computers.get(node);
  if (!found) {
    const client = new Client({
      url: async () => {
        const link = await own.client.call<{ url: string; token: string }>('mesh.link', { node });
        return `${link.url.replace(/^http/, 'ws')}/ws?token=${encodeURIComponent(link.token)}`;
      },
    });
    found = { node, client, journals: new JournalRegistry(client), queries: new QueryCache(client), state: own.state };
    computers.set(node, found);
  }
  return found;
}

/** The computer of the project shown (null: this one). */
export function activeComputer(): string | null {
  return installed?.state.get<string | null>('workbench', 'projectNode') ?? null;
}

/** The runtime of the project shown. */
export function runtime(): Runtime {
  return runtimeFor(activeComputer());
}

const ComputerContext = createContext<{ node: string | null } | null>(null);

/** Everything inside talks to `node`'s frame (null: this computer's), whatever project is shown. */
export function ComputerScope({ node, children }: { node: string | null; children?: ReactNode }) {
  const value = useMemo(() => ({ node }), [node]);
  return createElement(ComputerContext.Provider, { value }, children);
}

/** The runtime a component talks to: its `ComputerScope`'s, else the project shown's. */
export function useRuntime(): Runtime {
  const scoped = useContext(ComputerContext);
  const { state } = ownRuntime();
  const active = useSyncExternalStore(
    useCallback((listener: () => void) => state.subscribe('workbench', 'projectNode', listener), [state]),
    () => state.get<string | null>('workbench', 'projectNode') ?? null,
  );
  return runtimeFor(scoped ? scoped.node : active);
}

/** The computer a component's data comes from (null: this one). */
export function useComputer(): string | null {
  return useRuntime().node;
}

// ─── host and slot context ──────────────────────────────────────────────────

export interface BrowserBounds {
  x: number;
  y: number;
  width: number;
  height: number;
  visible: boolean;
}

/** What the Electron shell exposes to the renderer through its preload (nothing else). */
export interface ShellBridge {
  /** The native folder dialog (`title`: what the folder is for; default: open a project). */
  pickFolder(title?: string): Promise<string | null>;
  /** The native file dialog (e.g. a plugin executable to swap in). */
  pickFile(title: string): Promise<string | null>;
  /** Show a file in Finder (open externally); resolves to an error message or ''. */
  reveal(path: string): Promise<string>;
  /** Where the browser pane is (the browser shell module lays the page over it). */
  setBrowserBounds(bounds: BrowserBounds): void;
  /** The local path of a dropped or picked File ('' when it has none). Absent in older shells. */
  pathForFile?(file: File): string;
  /** Write pasted bytes where the chat flow's attachment store takes them in
   * (`<CORTEX_HOME>/attachments/incoming/`); absent in older shells. */
  saveAttachment?(bytes: Uint8Array, name: string): Promise<{ path?: string; error?: string }>;
  /** Open an https page in the user's own browser (not the agent browser), e.g. GitHub's sign-in;
   * resolves to an error message or ''. Absent in older shells. */
  openExternal?(url: string): Promise<string>;
}

export interface SlotRenderProps {
  /** The slot name, e.g. `pane.code@1`. */
  name: string;
  /** Extra props handed to the plugin component. */
  props?: Record<string, unknown>;
  className?: string;
}

/** Every plugin contributed to a slot, each in its own slot box, in their manifests' `order`. */
export type SlotListProps = SlotRenderProps;

export interface Host {
  /** Render the plugin that fills a slot (nested slots hot-swap independently). */
  Slot: ComponentType<SlotRenderProps>;
  /** Render every plugin contributed to a slot (a host's list: band items, settings panels…). */
  Slots: ComponentType<SlotListProps>;
  /** The Electron shell bridge, or null outside Electron. */
  shell: ShellBridge | null;
  /** Show one transient line in the workbench toast. */
  notify(message: string): void;
  /** The UI plugins the frame serves (live). */
  uiPlugins(): readonly UiPlugin[];
  subscribeUiPlugins(listener: () => void): () => void;
  /** Mount a UI plugin again from scratch (its durable state stays). */
  reloadPlugin(id: string): void;
}

export const HostContext = createContext<Host | null>(null);

export interface SlotInfo {
  slot: string;
  pluginId: string;
  version: string;
  hash: string;
}

export const SlotContext = createContext<SlotInfo | null>(null);

export function useHost(): Host {
  const host = useContext(HostContext);
  if (!host) throw new Error('useHost outside the workbench host');
  return host;
}

export function useSlotInfo(): SlotInfo {
  const info = useContext(SlotContext);
  if (!info) throw new Error('useSlotInfo outside a slot');
  return info;
}

// ─── plugin definition ──────────────────────────────────────────────────────

export interface SlotProps {
  slot: string;
  [key: string]: unknown;
}

export interface UiPluginDefinition {
  /** The slot this plugin fills, e.g. `workbench.sidebar@1`. */
  slot: string;
  component: ComponentType<SlotProps>;
}

/**
 * Types a plugin module's default export. The component receives `slot` plus whatever props the
 * hosting region passes (the right pane passes `active` to its tabs).
 */
export function definePlugin<P extends object = object>(definition: {
  slot: string;
  component: ComponentType<P & { slot: string }>;
}): UiPluginDefinition {
  return definition as unknown as UiPluginDefinition;
}

// ─── hooks ──────────────────────────────────────────────────────────────────

export function useClient(): Client {
  return useRuntime().client;
}

export function useConnection(): ConnectionState {
  const client = useRuntime().client;
  return useSyncExternalStore(
    useCallback((listener: () => void) => client.onState(listener), [client]),
    () => client.state,
  );
}

/** Run `handler` for every event whose topic starts with `prefix` (null: none). */
export function useEvent(prefix: string | null, handler: (topic: string, data: unknown) => void): void {
  const ref = useRef(handler);
  ref.current = handler;
  const { client } = useRuntime();
  useEffect(() => {
    if (!prefix) return;
    return client.on(prefix, (topic, data) => ref.current(topic, data));
  }, [prefix, client]);
}

export interface QueryOptions {
  /** Event topic prefixes that make this query reload. */
  topics?: string[];
  /** Reload only for events this returns true for. */
  when?: (topic: string, data: unknown) => boolean;
}

export interface QueryResult<T> extends QueryState<T> {
  refresh(): Promise<void>;
}

/**
 * Load `key` with `fetcher` (null key: nothing), cached across plugin versions, and reload it when
 * one of `topics` fires. Renders from the cache immediately, so a pre-mounted version draws data.
 */
export function useQuery<T>(key: string | null, fetcher: (client: Client) => Promise<T>, options: QueryOptions = {}): QueryResult<T> {
  const { queries, client } = useRuntime();
  const fetcherRef = useRef(fetcher);
  fetcherRef.current = fetcher;
  const whenRef = useRef(options.when);
  whenRef.current = options.when;
  const state = useSyncExternalStore(
    useCallback((listener: () => void) => (key ? queries.subscribe(key, listener) : () => undefined), [key, queries]),
    () => (key ? queries.get<T>(key) : (EMPTY_QUERY as QueryState<T>)),
  );
  const refresh = useCallback(async () => {
    if (key) await queries.fetch(key, (client) => fetcherRef.current(client));
  }, [key, queries]);
  useEffect(() => {
    if (!key) return;
    void queries.fetch(key, (client) => fetcherRef.current(client));
  }, [key, queries]);
  // A failed load is tried again, less and less often, while it is on screen.
  const failures = useRef(0);
  useEffect(() => {
    if (!key || !state.error || state.loading) {
      if (!state.error) failures.current = 0;
      return;
    }
    const delay = Math.min(1000 * 2 ** failures.current, 15000);
    failures.current++;
    const timer = setTimeout(() => void queries.fetch(key, (client) => fetcherRef.current(client)), delay);
    return () => clearTimeout(timer);
  }, [key, queries, state.error, state.loading, state.at]);
  const topics = (options.topics ?? []).join('\n');
  useEffect(() => {
    if (!key || !topics) return;
    let timer: ReturnType<typeof setTimeout> | null = null;
    const offs = topics.split('\n').map((prefix) => client.on(prefix, (topic, data) => {
      if (whenRef.current && !whenRef.current(topic, data)) return;
      // A burst of events (an agent editing many files) becomes one reload.
      if (timer) return;
      timer = setTimeout(() => {
        timer = null;
        void queries.fetch(key, (c) => fetcherRef.current(c));
      }, 120);
    }));
    return () => {
      if (timer) clearTimeout(timer);
      for (const off of offs) off();
    };
  }, [key, topics, queries, client]);
  return { ...state, refresh };
}

const EMPTY_QUERY: QueryState<unknown> = { data: undefined, error: undefined, loading: false, at: 0 };

/** Several cached queries at once (a list whose length varies: one per folder, per agent…). */
export function useQueries<T>(specs: Array<{ key: string; fetch: (client: Client) => Promise<T> }>, options: QueryOptions = {}): QueryState<T>[] {
  const { queries, client } = useRuntime();
  const keys = specs.map((spec) => spec.key).join('\n');
  const specsRef = useRef(specs);
  specsRef.current = specs;
  const whenRef = useRef(options.when);
  whenRef.current = options.when;
  const [, force] = useReducer((n: number) => n + 1, 0);
  useEffect(() => {
    const offs = (keys ? keys.split('\n') : []).map((key) => queries.subscribe(key, force));
    for (const spec of specsRef.current) void queries.fetch(spec.key, spec.fetch);
    return () => offs.forEach((off) => off());
  }, [keys, queries]);
  const topics = (options.topics ?? []).join('\n');
  useEffect(() => {
    if (!keys || !topics) return;
    let timer: ReturnType<typeof setTimeout> | null = null;
    const offs = topics.split('\n').map((prefix) => client.on(prefix, (topic, data) => {
      if (whenRef.current && !whenRef.current(topic, data)) return;
      if (timer) return;
      timer = setTimeout(() => {
        timer = null;
        for (const spec of specsRef.current) void queries.fetch(spec.key, spec.fetch);
      }, 120);
    }));
    return () => {
      if (timer) clearTimeout(timer);
      offs.forEach((off) => off());
    };
  }, [keys, topics, queries, client]);
  // The same array while every query's state is the same object, so callers can memoize on it.
  const next = specs.map((spec) => queries.get<T>(spec.key));
  const last = useRef<QueryState<T>[]>(next);
  if (last.current.length !== next.length || next.some((state, index) => state !== last.current[index])) last.current = next;
  return last.current;
}
const LOADING_JOURNAL: JournalSnapshot = { status: 'loading', entries: [] };

/** The materialized journal of a session (null: none). */
export function useJournal(sessionId: string | null): JournalSnapshot {
  const { journals } = useRuntime();
  const [, force] = useReducer((n: number) => n + 1, 0);
  useEffect(() => {
    if (!sessionId) return;
    const view = journals.acquire(sessionId);
    const off = view.subscribe(force);
    force();
    return () => {
      off();
      journals.release(view);
    };
  }, [sessionId, journals]);
  if (!sessionId) return LOADING_JOURNAL;
  return journals.peek(sessionId)?.get() ?? LOADING_JOURNAL;
}

function useStoreValue<T>(ns: string, key: string, initial: T): [T, (value: T | ((current: T) => T)) => void] {
  const { state } = ownRuntime();
  const value = useSyncExternalStore(
    useCallback((listener: () => void) => state.subscribe(ns, key, listener), [state, ns, key]),
    () => state.get<T>(ns, key),
  );
  const set = useCallback((next: T | ((current: T) => T)) => {
    const current = (state.get<T>(ns, key) ?? initial) as T;
    const resolved = typeof next === 'function' ? (next as (current: T) => T)(current) : next;
    state.set(ns, key, resolved);
    // `initial` is a default, not a dependency: it is read only when nothing is stored.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [state, ns, key]);
  return [value === undefined ? initial : value, set];
}

/** Durable state of this slot (survives hot-swaps and restarts). */
export function useSlotState<T>(key: string, initial: T): [T, (value: T | ((current: T) => T)) => void] {
  const info = useContext(SlotContext);
  return useStoreValue(info?.slot ?? 'unslotted', key, initial);
}

/** Durable state shared by the whole workbench (selection, the right pane's open tab). */
export function useShared<T>(key: string, initial: T): [T, (value: T | ((current: T) => T)) => void] {
  return useStoreValue('workbench', key, initial);
}

/** Write a shared value from outside React (e.g. after an async call). */
export function setShared(key: string, value: unknown): void {
  ownRuntime().state.set('workbench', key, value);
}

export function getShared<T>(key: string): T | undefined {
  return ownRuntime().state.get<T>('workbench', key);
}

/** The current time, re-rendered every `intervalMs` while `active`. */
export function useNow(intervalMs: number, active: boolean): number {
  const [now, setNow] = useState(() => Date.now());
  useEffect(() => {
    if (!active) return;
    setNow(Date.now());
    const timer = setInterval(() => setNow(Date.now()), intervalMs);
    return () => clearInterval(timer);
  }, [intervalMs, active]);
  return now;
}

/** Provide a slot's identity to a plugin subtree (host only). */
export function SlotProvider({ info, children }: { info: SlotInfo; children: ReactNode }) {
  return createElement(SlotContext.Provider, { value: info }, children);
}

/** The UI plugins the frame serves, live. */
export function useUiPlugins(): readonly UiPlugin[] {
  const host = useHost();
  return useSyncExternalStore(
    useCallback((listener: () => void) => host.subscribeUiPlugins(listener), [host]),
    () => host.uiPlugins(),
  );
}
