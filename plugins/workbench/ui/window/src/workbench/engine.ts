/**
 * The hot-swap engine of one slot.
 *
 * A new bundle is imported, mounted hidden next to the running version (React `<Activity
 * mode="hidden">` behind an error boundary), and the slot flips to it only once it has rendered
 * without error; then the old version unmounts. A version that fails to load, fails to render
 * hidden, or never becomes ready is rejected and the running version stays. A version that fails
 * after the flip is rejected too, and the last version that ran well is mounted again in its place.
 *
 * The engine holds no plugin state: whatever a slot must keep across versions lives in the
 * workbench's durable store, which every version reads.
 */

import type { ComponentType } from 'react';
import type { SlotProps, UiPlugin, UiPluginDefinition } from '@cortex/sdk';

export type Loader = (plugin: UiPlugin) => Promise<UiPluginDefinition>;
export type StyleLoader = (plugin: UiPlugin, key: string) => Promise<() => void>;

export interface Instance {
  key: string;
  plugin: UiPlugin;
  component: ComponentType<SlotProps>;
  /** Removes the instance's stylesheet. */
  dropStyle: () => void;
}

export interface SwapRecord {
  at: number;
  slot: string;
  from: string | null;
  to: string;
  outcome: 'swapped' | 'rejected' | 'rolled_back';
  reason?: string;
}

type Listener = () => void;

export const READY_TIMEOUT_MS = 15_000;

export class SlotEngine {
  readonly slot: string;
  active: Instance | null = null;
  staging: Instance | null = null;
  /** The active version failed and nothing older can replace it. */
  broken: { instance: Instance; reason: string } | null = null;
  readonly rejected = new Set<string>();
  readonly history: SwapRecord[] = [];
  /** Versions that ran well, newest last (candidates for a rollback). */
  private good: Array<{ plugin: UiPlugin; component: ComponentType<SlotProps> }> = [];
  private readonly listeners = new Set<Listener>();
  private readonly load: Loader;
  private readonly loadStyle: StyleLoader;
  private desired: UiPlugin | null = null;
  private counter = 0;
  private readyTimer: ReturnType<typeof setTimeout> | null = null;
  private loading: string | null = null;
  private disposed = false;
  private loadGeneration = 0;
  onRecord: ((record: SwapRecord) => void) | null = null;

  constructor(slot: string, load: Loader, loadStyle: StyleLoader) {
    this.slot = slot;
    this.load = load;
    this.loadStyle = loadStyle;
  }

  subscribe(listener: Listener): () => void {
    this.listeners.add(listener);
    return () => this.listeners.delete(listener);
  }

  /** The frame's current plugin for this slot. */
  desire(plugin: UiPlugin | undefined): void {
    if (!plugin || this.disposed) return;
    this.desired = plugin;
    if (this.staging && this.staging.plugin.hash !== plugin.hash) {
      this.clearTimer();
      this.staging.dropStyle();
      this.staging = null;
      this.emit();
    }
    if (plugin.hash === this.active?.plugin.hash && !this.broken) return;
    if (plugin.hash === this.staging?.plugin.hash) return;
    if (plugin.hash === this.loading) return;
    if (this.rejected.has(plugin.hash)) return;
    void this.stage(plugin);
  }

  /** The staged instance rendered without error. */
  ready(key: string): void {
    const staged = this.staging;
    if (!staged || staged.key !== key || this.desired?.hash !== staged.plugin.hash) return;
    this.clearTimer();
    const previous = this.active;
    this.active = staged;
    this.staging = null;
    this.broken = null;
    if (!this.good.some((entry) => entry.plugin.hash === staged.plugin.hash)) {
      this.good.push({ plugin: staged.plugin, component: staged.component });
    }
    previous?.dropStyle();
    this.record({
      from: previous?.plugin.hash ?? null,
      to: staged.plugin.hash,
      outcome: this.rollingBack === staged.plugin.hash ? 'rolled_back' : 'swapped',
    });
    this.rollingBack = null;
    this.emit();
  }

  /** The staged instance failed (import, render, or readiness timeout). */
  stagedFailed(key: string, reason: string): void {
    const staged = this.staging;
    if (!staged || staged.key !== key) return;
    this.clearTimer();
    this.staging = null;
    staged.dropStyle();
    this.rejected.add(staged.plugin.hash);
    this.record({ from: this.active?.plugin.hash ?? null, to: staged.plugin.hash, outcome: 'rejected', reason });
    this.emit();
  }

  /** The running instance failed after the flip: reject it and mount the last good version. */
  activeFailed(key: string, reason: string): void {
    const active = this.active;
    if (!active || active.key !== key) return;
    this.rejected.add(active.plugin.hash);
    this.good = this.good.filter((entry) => entry.plugin.hash !== active.plugin.hash);
    this.record({ from: active.plugin.hash, to: active.plugin.hash, outcome: 'rejected', reason });
    const fallback = [...this.good].reverse().find((entry) => !this.rejected.has(entry.plugin.hash));
    if (fallback) {
      this.desired = fallback.plugin;
      this.rollingBack = fallback.plugin.hash;
      void this.stageLoaded(fallback.plugin, fallback.component, ++this.loadGeneration);
    } else {
      this.broken = { instance: active, reason };
    }
    this.emit();
  }

  /** Mount the same version again after the person asked to (the broken state's button). */
  retry(): void {
    const broken = this.broken;
    if (!broken) return;
    this.rejected.delete(broken.instance.plugin.hash);
    this.broken = null;
    this.active = null;
    broken.instance.dropStyle();
    void this.stage(broken.instance.plugin);
    this.emit();
  }

  instances(): Instance[] {
    const list: Instance[] = [];
    if (this.active && !this.broken) list.push(this.active);
    if (this.staging) list.push(this.staging);
    return list;
  }

  /**
   * The frame no longer serves a plugin for this slot (it was removed or its plugin disabled):
   * unmount what runs. What the plugin kept in the workbench's store stays for when it returns.
   */
  release(): void {
    this.loadGeneration++;
    this.loading = null;
    this.desired = null;
    if (!this.active && !this.staging && !this.broken) return;
    this.clearTimer();
    this.active?.dropStyle();
    this.staging?.dropStyle();
    this.active = null;
    this.staging = null;
    this.broken = null;
    this.emit();
  }

  dispose(): void {
    this.loadGeneration++;
    this.disposed = true;
    this.clearTimer();
    this.active?.dropStyle();
    this.staging?.dropStyle();
    this.listeners.clear();
  }

  private rollingBack: string | null = null;

  private async stage(plugin: UiPlugin): Promise<void> {
    const generation = ++this.loadGeneration;
    this.loading = plugin.hash;
    let definition: UiPluginDefinition;
    try {
      definition = await this.load(plugin);
      if (!definition || typeof definition.component !== 'function') {
        throw new Error('the module has no default export with a component');
      }
      if (definition.slot !== this.slot) {
        throw new Error(`the module fills ${definition.slot}, not ${this.slot}`);
      }
    } catch (error) {
      if (generation !== this.loadGeneration) return;
      this.loading = null;
      if (this.disposed) return;
      this.rejected.add(plugin.hash);
      this.record({
        from: this.active?.plugin.hash ?? null,
        to: plugin.hash,
        outcome: 'rejected',
        reason: error instanceof Error ? error.message : String(error),
      });
      this.emit();
      return;
    }
    if (generation !== this.loadGeneration) return;
    this.loading = null;
    if (this.disposed) return;
    // Released while it loaded (the frame stopped serving the plugin): nothing to stage.
    if (!this.desired) return;
    // A newer build may have arrived while this one loaded; stage only the latest.
    if (this.desired && this.desired.hash !== plugin.hash && !this.rejected.has(this.desired.hash)) {
      void this.stage(this.desired);
      return;
    }
    await this.stageLoaded(plugin, definition.component, generation);
  }

  private async stageLoaded(plugin: UiPlugin, component: ComponentType<SlotProps>, generation: number): Promise<void> {
    const key = `${plugin.hash.slice(0, 12)}-${++this.counter}`;
    let dropStyle: () => void = () => undefined;
    try {
      dropStyle = await this.loadStyle(plugin, key);
    } catch (error) {
      if (this.disposed || generation !== this.loadGeneration || this.desired?.hash !== plugin.hash) return;
      this.rejected.add(plugin.hash);
      this.record({ from: this.active?.plugin.hash ?? null, to: plugin.hash, outcome: 'rejected', reason: `stylesheet: ${error instanceof Error ? error.message : String(error)}` });
      this.emit();
      return;
    }
    if (this.disposed || generation !== this.loadGeneration || this.desired?.hash !== plugin.hash) {
      dropStyle();
      return;
    }
    if (this.staging) {
      this.staging.dropStyle();
      this.clearTimer();
    }
    this.staging = { key, plugin, component, dropStyle };
    this.readyTimer = setTimeout(() => this.stagedFailed(key, `not ready within ${READY_TIMEOUT_MS} ms`), READY_TIMEOUT_MS);
    this.emit();
  }

  private clearTimer(): void {
    if (this.readyTimer) clearTimeout(this.readyTimer);
    this.readyTimer = null;
  }

  private record(partial: Omit<SwapRecord, 'at' | 'slot'>): void {
    const record: SwapRecord = { at: Date.now(), slot: this.slot, ...partial };
    this.history.push(record);
    this.onRecord?.(record);
  }

  private emit(): void {
    for (const listener of [...this.listeners]) listener();
  }
}
