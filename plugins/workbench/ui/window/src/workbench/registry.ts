/**
 * Which UI plugin fills which slot: the frame's list (`frame.ui_plugins.list`), kept current by
 * `ui_plugin.changed` events. The frame serves every plugin bundle under a URL that contains its
 * content hash, so a new build is a new module URL.
 */

import { byOrder, type Client, type UiPlugin } from '@cortex/sdk';

type Listener = () => void;

export class PluginRegistry {
  private plugins: readonly UiPlugin[] = [];
  private readonly listeners = new Set<Listener>();
  private loaded = false;
  error: string | null = null;
  private readonly client: Client;

  constructor(client: Client) {
    this.client = client;
  }

  start(): void {
    this.client.on('ui_plugin.', (_topic, data) => {
      // The event carries the plugin that changed; the list stays authoritative.
      if (isUiPlugin(data)) this.upsert(data);
      void this.refresh();
    });
    this.client.onReconnect(() => void this.refresh());
    void this.refresh();
  }

  /** label/order from each build's manifest.json, by content hash (a build never changes). */
  private readonly manifests = new Map<string, Promise<Pick<UiPlugin, 'label' | 'order' | 'flow'>>>();

  private presentation(plugin: UiPlugin): Promise<Pick<UiPlugin, 'label' | 'order' | 'flow'>> {
    let pending = this.manifests.get(plugin.hash);
    if (!pending) {
      const url = new URL('manifest.json', new URL(plugin.entry_url, location.href)).href;
      pending = fetch(url)
        .then((response) => (response.ok ? response.json() : {}))
        .then((manifest: { label?: unknown; order?: unknown; flow?: unknown }) => ({
          ...(typeof manifest.label === 'string' && manifest.label.trim() ? { label: manifest.label } : {}),
          ...(typeof manifest.order === 'number' && Number.isFinite(manifest.order) ? { order: manifest.order } : {}),
          ...(typeof manifest.flow === 'string' && manifest.flow ? { flow: manifest.flow } : {}),
        }))
        // A manifest that cannot be read leaves the plugin named by its slot.
        .catch(() => ({}));
      this.manifests.set(plugin.hash, pending);
    }
    return pending;
  }

  async refresh(): Promise<void> {
    try {
      const result = await this.client.call<UiPlugin[] | { plugins: UiPlugin[] }>('frame.ui_plugins.list', {});
      const list = (Array.isArray(result) ? result : result?.plugins ?? []).filter(isUiPlugin);
      // The frame lists id, slot, version and URLs; how a host presents a plugin is in its manifest.
      this.plugins = await Promise.all(list.map(async (plugin) => ({ ...plugin, ...(await this.presentation(plugin)) })));
      this.loaded = true;
      this.error = null;
    } catch (error) {
      this.error = error instanceof Error ? error.message : String(error);
    }
    this.emit();
  }

  /** The plugin for a slot. Several for one slot: the one of `flow` (the open session's) when some
   * belong to a flow, then the highest version, then the first listed. */
  forSlot(slot: string, flow?: string): UiPlugin | undefined {
    let candidates = this.plugins.filter((plugin) => plugin.slot === slot);
    if (flow !== undefined && candidates.some((plugin) => plugin.flow)) {
      const own = candidates.filter((plugin) => plugin.flow === flow);
      if (own.length > 0) candidates = own;
    }
    if (candidates.length <= 1) return candidates[0];
    return [...candidates].sort((a, b) => compareVersions(b.version, a.version))[0];
  }

  /** Every plugin contributed to a slot, in its manifests' order (then label, then id). */
  allForSlot(slot: string): UiPlugin[] {
    return this.plugins.filter((plugin) => plugin.slot === slot).sort(byOrder);
  }

  byId(id: string): UiPlugin | undefined {
    return this.plugins.find((plugin) => plugin.id === id);
  }

  /** Bumped to mount a plugin again from scratch. */
  private readonly reloads = new Map<string, number>();

  reloadToken(id: string): number {
    return this.reloads.get(id) ?? 0;
  }

  reload(id: string): void {
    this.reloads.set(id, this.reloadToken(id) + 1);
    this.emit();
  }

  isLoaded(): boolean {
    return this.loaded;
  }

  list(): readonly UiPlugin[] {
    return this.plugins;
  }

  subscribe(listener: Listener): () => void {
    this.listeners.add(listener);
    return () => this.listeners.delete(listener);
  }

  /** Some plugin of the slot belongs to a flow: the window picks by the open session's flow. */
  byFlow(slot: string): boolean {
    return this.plugins.some((plugin) => plugin.slot === slot && plugin.flow);
  }

  private upsert(plugin: UiPlugin): void {
    if ((plugin as unknown as { removed?: boolean }).removed) return;
    const index = this.plugins.findIndex((existing) => existing.id === plugin.id);
    const next = this.plugins.slice();
    // The event carries what the frame knows; the manifest's label and order stay until refresh.
    if (index >= 0) next[index] = next[index]!.hash === plugin.hash ? { ...next[index], ...plugin } : plugin;
    else next.push(plugin);
    this.plugins = next;
    this.emit();
  }

  private emit(): void {
    for (const listener of [...this.listeners]) listener();
  }
}

export function isUiPlugin(value: unknown): value is UiPlugin {
  if (!value || typeof value !== 'object') return false;
  const plugin = value as Partial<UiPlugin>;
  return typeof plugin.id === 'string' && typeof plugin.slot === 'string'
    && typeof plugin.entry_url === 'string' && typeof plugin.hash === 'string';
}

/** Compare dotted numeric versions (`0.10.2` > `0.9.9`); non-numeric parts compare as text. */
export function compareVersions(a: string, b: string): number {
  const pa = a.split(/[.+-]/);
  const pb = b.split(/[.+-]/);
  for (let i = 0; i < Math.max(pa.length, pb.length); i++) {
    const x = pa[i] ?? '0';
    const y = pb[i] ?? '0';
    const nx = Number(x);
    const ny = Number(y);
    if (Number.isFinite(nx) && Number.isFinite(ny)) {
      if (nx !== ny) return nx - ny;
    } else if (x !== y) {
      return x < y ? -1 : 1;
    }
  }
  return 0;
}
