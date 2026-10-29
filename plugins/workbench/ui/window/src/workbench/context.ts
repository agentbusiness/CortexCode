import { createContext } from 'react';
import * as sdk from '@cortex/sdk';
import type { UiPlugin, UiPluginDefinition } from '@cortex/sdk';
import type { PluginRegistry } from './registry';
import type { SwapRecord } from './engine';

export interface WorkbenchServices {
  registry: PluginRegistry;
  loadModule: (plugin: UiPlugin) => Promise<UiPluginDefinition>;
  loadStyle: (plugin: UiPlugin, key: string) => Promise<() => void>;
  missingSdk: (plugin: UiPlugin) => Promise<string[]>;
  onSwap: (record: SwapRecord) => void;
}

export const WorkbenchContext = createContext<WorkbenchServices | null>(null);

/**
 * The SDK names a plugin bundle imports that this page's SDK lacks (read from the bundle's
 * manifest.json, served beside its entry). A page loads the SDK once, so a bundle built against a
 * newer SDK cannot link here until the window itself is updated and reloads.
 */
export async function missingSdk(plugin: UiPlugin): Promise<string[]> {
  const response = await fetch(new URL('manifest.json', new URL(plugin.entry_url, location.href)));
  if (!response.ok) return [];
  const manifest = (await response.json()) as { sdk_imports?: unknown };
  if (!Array.isArray(manifest.sdk_imports)) return [];
  return manifest.sdk_imports.filter((name): name is string => typeof name === 'string' && !(name in sdk));
}

/** Import a plugin bundle; the URL carries its content hash, so a rebuild is a new module. */
export async function importPlugin(plugin: UiPlugin): Promise<UiPluginDefinition> {
  const module = (await import(/* @vite-ignore */ plugin.entry_url)) as { default?: UiPluginDefinition };
  if (!module.default) throw new Error(`${plugin.entry_url} has no default export`);
  return module.default;
}

/** Load a plugin's stylesheet before its version is shown; returns its removal. */
export function linkStyle(plugin: UiPlugin, key: string): Promise<() => void> {
  const href = plugin.css_url;
  if (!href) return Promise.resolve(() => undefined);
  return new Promise((resolve, reject) => {
    const link = document.createElement('link');
    link.rel = 'stylesheet';
    link.href = href;
    link.dataset.pluginStyle = key;
    link.onload = () => resolve(() => link.remove());
    link.onerror = () => {
      link.remove();
      reject(new Error(`stylesheet ${href} did not load`));
    };
    document.head.appendChild(link);
  });
}
