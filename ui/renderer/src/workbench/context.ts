import { createContext } from 'react';
import type { UiPlugin, UiPluginDefinition } from '@cortex/sdk';
import type { PluginRegistry } from './registry';
import type { SwapRecord } from './engine';

export interface WorkbenchServices {
  registry: PluginRegistry;
  loadModule: (plugin: UiPlugin) => Promise<UiPluginDefinition>;
  loadStyle: (plugin: UiPlugin, key: string) => Promise<() => void>;
  onSwap: (record: SwapRecord) => void;
}

export const WorkbenchContext = createContext<WorkbenchServices | null>(null);

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
