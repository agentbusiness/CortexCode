import { contributions, type UiPlugin } from '@cortex/sdk';

/** Section slots present (every `card.<name>@1` but the header), in their manifests' order. */
export function cardSections(plugins: readonly UiPlugin[]): string[] {
  return contributions(plugins, /^card\.[a-z][a-z0-9-]*@1$/, ['card.header@1']).map((entry) => entry.slot);
}
