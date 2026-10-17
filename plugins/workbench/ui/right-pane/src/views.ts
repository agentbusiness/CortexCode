import { contributions, type UiPlugin } from '@cortex/sdk';

export interface View {
  name: string;
  slot: string;
  label: string;
}

/** Every `pane.<name>@1` plugin is a view, named and ordered by its manifest. */
export function paneViews(plugins: readonly UiPlugin[]): View[] {
  return contributions(plugins, /^pane\.[a-z][a-z0-9-]*@1$/).map(({ name, slot, label }) => ({ name, slot, label }));
}
