/**
 * Hosts that gather a family of slots (`pane.<name>@1`, `card.<name>@1`, `settings.<name>@1`, the
 * band) present each contribution by the `label` and `order` its manifest gives, falling back to
 * the slot's name. Nothing about which plugins exist is written into a host.
 */

import type { UiPlugin } from './types';

/** `pane.code@1` -> `code`. */
export function slotName(slot: string): string {
  const base = slot.replace(/@\d+$/, '');
  return base.slice(base.lastIndexOf('.') + 1);
}

export function pluginLabel(plugin: Pick<UiPlugin, 'slot' | 'label'>): string {
  if (plugin.label && plugin.label.trim()) return plugin.label.trim();
  const name = slotName(plugin.slot).replace(/[-_]+/g, ' ');
  return name.charAt(0).toUpperCase() + name.slice(1);
}

/** Manifest order first (lower first; unordered last), then label, then id. */
export function byOrder(a: UiPlugin, b: UiPlugin): number {
  const rank = (plugin: UiPlugin) => (typeof plugin.order === 'number' && Number.isFinite(plugin.order) ? plugin.order : Number.MAX_SAFE_INTEGER);
  return rank(a) - rank(b) || pluginLabel(a).localeCompare(pluginLabel(b)) || a.id.localeCompare(b.id);
}

export interface Contribution {
  /** The slot's own name (`code` for `pane.code@1`). */
  name: string;
  slot: string;
  label: string;
  plugin: UiPlugin;
}

/**
 * One entry per slot of a family (`pattern` matches the slots, e.g. /^pane\.[a-z][a-z0-9-]*@1$/),
 * the newest version for each, sorted by manifest order. `exclude` drops slots (a host's own).
 */
export function contributions(plugins: readonly UiPlugin[], pattern: RegExp, exclude: readonly string[] = []): Contribution[] {
  const bySlot = new Map<string, UiPlugin>();
  for (const plugin of plugins) {
    if (!pattern.test(plugin.slot) || exclude.includes(plugin.slot)) continue;
    const current = bySlot.get(plugin.slot);
    if (!current || compareVersions(plugin.version, current.version) > 0) bySlot.set(plugin.slot, plugin);
  }
  return [...bySlot.values()].sort(byOrder).map((plugin) => ({ name: slotName(plugin.slot), slot: plugin.slot, label: pluginLabel(plugin), plugin }));
}

function compareVersions(a: string, b: string): number {
  const pa = a.split(/[.-]/).map((part) => Number.parseInt(part, 10));
  const pb = b.split(/[.-]/).map((part) => Number.parseInt(part, 10));
  for (let i = 0; i < Math.max(pa.length, pb.length); i++) {
    const x = Number.isFinite(pa[i]) ? pa[i]! : 0;
    const y = Number.isFinite(pb[i]) ? pb[i]! : 0;
    if (x !== y) return x - y;
  }
  return 0;
}
