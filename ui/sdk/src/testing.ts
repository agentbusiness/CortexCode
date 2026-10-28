/**
 * Test support for plugin packages: a runtime whose frame is a table of handlers, and a host that
 * stands in for the workbench (nested slots render as marked placeholders). Plugins test their
 * own rendering and calls with these, without a frame.
 */

import { createElement, type ReactNode } from 'react';
import { JournalRegistry } from './journal';
import { QueryCache } from './query';
import { RpcError, type Client, type ConnectionState } from './rpc';
import { HostContext, installRuntime, type Host, type Runtime, type ShellBridge } from './runtime';
import { SlotStateStore } from './store';
import type { UiPlugin } from './types';

// eslint-disable-next-line @typescript-eslint/no-explicit-any
export type FakeHandler = (params: any) => unknown;

/** A frame connection that answers from `handlers` and delivers events the test emits. */
export class FakeClient {
  state: ConnectionState = 'open';
  readonly calls: Array<{ method: string; params: unknown }> = [];
  private readonly listeners = new Map<string, Set<(topic: string, data: unknown) => void>>();

  constructor(public handlers: Record<string, FakeHandler> = {}) {}

  call<T = unknown>(method: string, params: unknown = {}): Promise<T> {
    this.calls.push({ method, params });
    const handler = this.handlers[method];
    if (!handler) return Promise.reject(new RpcError('unavailable', `no test handler for ${method}`));
    try {
      return Promise.resolve(handler(params) as T);
    } catch (error) {
      return Promise.reject(error);
    }
  }

  on(prefix: string, handler: (topic: string, data: unknown) => void): () => void {
    const set = this.listeners.get(prefix) ?? new Set();
    set.add(handler);
    this.listeners.set(prefix, set);
    return () => set.delete(handler);
  }

  emit(topic: string, data: unknown): void {
    for (const [prefix, set] of this.listeners) if (topic.startsWith(prefix)) for (const handler of [...set]) handler(topic, data);
  }

  readonly published: Array<{ topic: string; data: unknown }> = [];

  publish(topic: string, data: unknown): void {
    this.published.push({ topic, data });
  }

  onState(): () => void {
    return () => undefined;
  }

  onReconnect(): () => void {
    return () => undefined;
  }

  close(): void {}
}

export interface TestRuntime {
  client: FakeClient;
  runtime: Runtime;
}

/** Install a fresh runtime over a fake frame. */
export function installTestRuntime(handlers: Record<string, FakeHandler> = {}): TestRuntime {
  const client = new FakeClient(handlers);
  const asClient = client as unknown as Client;
  const runtime: Runtime = { node: null, client: asClient, journals: new JournalRegistry(asClient), queries: new QueryCache(asClient), state: new SlotStateStore() };
  installRuntime(runtime);
  return { client, runtime };
}

/** The workbench host, for tests: nested slots are placeholders marked with their name. */
const NO_PLUGINS: UiPlugin[] = [];

export function TestHost({ children, shell = null, plugins = NO_PLUGINS, notify }: {
  children: ReactNode;
  shell?: ShellBridge | null;
  plugins?: UiPlugin[];
  notify?: (message: string) => void;
}) {
  const host: Host = {
    Slot: ({ name, props }) => createElement('div', { 'data-test-slot': name, 'data-props': JSON.stringify(props ?? {}) }),
    Slots: ({ name }) => createElement('div', { 'data-test-slots': name }),
    shell,
    notify: notify ?? (() => undefined),
    uiPlugins: () => plugins,
    subscribeUiPlugins: () => () => undefined,
    reloadPlugin: () => undefined,
  };
  return createElement(HostContext.Provider, { value: host }, children);
}
