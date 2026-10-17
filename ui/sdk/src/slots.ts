/**
 * The workbench's slot contracts: every region of the UI is a named, versioned slot a plugin
 * fills (or, for a host's list, contributes to). A slot's contract is its name, the props its host
 * passes, and the shared durable state it reads and writes. Hosts render slots with
 * `useHost().Slot` / `useHost().Slots`; plugins declare theirs in package.json (`cortex.slot`).
 */

/** Regions the renderer core itself lays out. */
export const CORE_SLOTS = {
  /** Items of the brand band, after the mark, at its right, in their manifests' order (a list:
   * every plugin on this slot is shown): the conversation's title and run state, a problem (the
   * frame unreachable, a service not running; nothing when all is well), the card's toggle. */
  band: 'workbench.band@1',
  sidebar: 'workbench.sidebar@1',
  chat: 'workbench.chat@1',
  /** The right pane host. */
  rightPane: 'workbench.rightPane@1',
} as const;

/** Slots hosts expose to other plugins. */
export const HOST_SLOTS = {
  /** List in the sidebar's bar (open project, new chat…). */
  sidebarActions: 'sidebar.actions@1',
  /** The conversation's composer. Props: none (reads the selection). */
  chatComposer: 'chat.composer@1',
  /** Right-pane views: every `pane.<name>@1` plugin is a view, ordered code, terminal, markdown, board, browser, then by name. Props: `{ active: boolean }`. */
  pane: (name: string) => `pane.${name}@1`,
  /** The code view's parts. Props: none (they share `code.*` state). */
  codeToolbar: 'code.toolbar@1',
  codeTree: 'code.tree@1',
  codeEditor: 'code.editor@1',
  /** The card's header (project and its menu). */
  cardHeader: 'card.header@1',
  /** Card sections, in this order, then any other `card.<name>@1`. */
  cardSections: ['card.git@1', 'card.agents@1', 'card.tasks@1', 'card.board@1', 'card.index@1', 'card.model@1'],
  /** The settings window, opened from the card. Props: `{ onClose(): void }`. */
  settingsHost: 'settings.host@1',
  /** Settings panels: every `settings.<name>@1` plugin other than the host is a panel. */
  settingsPanels: ['settings.model@1', 'settings.plugins@1', 'settings.agents@1', 'settings.appearance@1'],
} as const;

/**
 * Shared durable state keys (`useShared`), with what they hold. Every version of every plugin
 * reads the same values, so swapping a plugin never loses them.
 */
export interface SharedState {
  projectId: string | null;
  sessionId: string | null;
  /** Right pane: the view in front (`code`, `terminal`, …). */
  'pane.tab': string;
  /** The ground: `dark` (plate), `light` (page) or `system`. */
  view: 'dark' | 'light' | 'system';
  /** Widths (px) of the sidebar and the right pane; null: the default. Set by dragging the dividers. */
  'layout.sidebar': number | null;
  'layout.pane': number | null;
  /** The sidebar is collapsed to its strip of marks. */
  'layout.sidebarCollapsed': boolean;
  /** The right pane is collapsed (only the card's toggle remains). */
  'layout.paneCollapsed': boolean;
  /** Ids of plugins (verticals) this workbench disabled and the frame accepted. */
  'plugins.disabled': string[];
  /** An overlay (card, settings) covers part of the window: native views step back. */
  'overlay.open': boolean;
  'code.target': import('./code').CodeTarget | null;
  'code.scope': import('./code').CodeScope;
  'code.base': string | null;
  'code.open': import('./code').CodeOpen | null;
  'code.options': import('./code').CodeOptions;
  'markdown.open': { root: string; path: string } | null;
  /** A task to show on the board. */
  'board.focus': string | null;
  /** The board's view: tasks, agents or messages. */
  'board.view': 'tasks' | 'agents' | 'board';
  /** A journal entry to bring into view in the conversation. */
  'chat.focus': { sessionId: string; seq: number | null } | null;
  /** Bumped to put the caret in the composer (a notification about a conversation was opened). */
  'composer.focus': number;
  /** Mode new conversations start in. */
  'agents.defaultMode': 'ask' | 'auto';
  /** The card is open. */
  'card.open': boolean;
  /** The settings window is open, and on which panel. */
  'settings.open': string | null;
}
