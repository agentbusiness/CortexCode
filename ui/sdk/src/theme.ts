/**
 * The active theme, as data. The theme is a plugin (slot `theme@1`, exactly one active): its
 * stylesheet carries the tokens as CSS custom properties, the fonts and the shared primitives, and
 * its module publishes what non-CSS renderers need — the token values of each ground, CodeMirror's
 * theme and highlight style as plain specs, and xterm.js palettes — here, so the editor and the
 * terminal restyle the moment a new theme version is swapped in.
 */

import { useSyncExternalStore } from 'react';

/** A ground: `plate` (dark, the design's default) or `page` (light). */
export type Ground = 'plate' | 'page';

export interface XtermPalette {
  background: string;
  foreground: string;
  cursor: string;
  cursorAccent: string;
  selectionBackground: string;
  black: string; red: string; green: string; yellow: string; blue: string; magenta: string; cyan: string; white: string;
  brightBlack: string; brightRed: string; brightGreen: string; brightYellow: string; brightBlue: string; brightMagenta: string; brightCyan: string; brightWhite: string;
}

/** One highlight rule: Lezer tag names (`keyword`, `string`, `function(variableName)`…) and their style. */
export interface HighlightRule {
  tags: string[];
  style: Record<string, string>;
}

export interface ThemeData {
  id: string;
  version: string;
  /** Token values by ground (`--ground`, `--ink`, `--rubric`…). */
  tokens: Record<Ground, Record<string, string>>;
  /** Font stacks (`reading`, `apparatus`, `mono`). */
  fonts: Record<string, string>;
  codemirror: {
    /** An `EditorView.theme` spec (values may use the tokens' CSS variables). */
    theme: Record<string, Record<string, string>>;
    highlight: HighlightRule[];
  };
  xterm: Record<Ground, XtermPalette>;
}

let active: ThemeData | null = null;
const listeners = new Set<() => void>();

/** Called by the theme plugin when its version becomes the active one. */
export function publishTheme(theme: ThemeData): void {
  active = theme;
  for (const listener of [...listeners]) listener();
}

export function activeTheme(): ThemeData | null {
  return active;
}

export function subscribeTheme(listener: () => void): () => void {
  listeners.add(listener);
  return () => listeners.delete(listener);
}

/** The active theme's data (null until a theme plugin is active). */
export function useThemeData(): ThemeData | null {
  return useSyncExternalStore(subscribeTheme, activeTheme);
}

/** The ground on screen now (the workbench sets it on the document as `data-theme`). */
export function currentGround(): Ground {
  return typeof document !== 'undefined' && document.documentElement.dataset.theme === 'light' ? 'page' : 'plate';
}

function subscribeGround(listener: () => void): () => void {
  if (typeof MutationObserver === 'undefined' || typeof document === 'undefined') return () => undefined;
  const observer = new MutationObserver(listener);
  observer.observe(document.documentElement, { attributes: true, attributeFilter: ['data-theme'] });
  return () => observer.disconnect();
}

export function useGround(): Ground {
  return useSyncExternalStore(subscribeGround, currentGround);
}
