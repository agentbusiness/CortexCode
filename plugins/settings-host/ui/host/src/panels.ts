import { contributions, type UiPlugin } from '@cortex/sdk';

export interface Panel { name: string; slot: string; label: string }

/** Every `settings.<name>@1` plugin other than the host is a panel, named and ordered by its manifest. */
export function settingsPanels(plugins: readonly UiPlugin[]): Panel[] {
  return contributions(plugins, /^settings\.[a-z][a-z0-9-]*@1$/, ['settings.host@1']).map(({ name, slot, label }) => ({ name, slot, label }));
}
