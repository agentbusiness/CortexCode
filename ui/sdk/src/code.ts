/**
 * The code view's shared state (`pane.code@1` and its sub-slots `code.toolbar@1`, `code.tree@1`,
 * `code.editor@1`), kept in the workbench's durable store so three separately swapped plugins see
 * one view: which tree it looks at (the person's working tree, or an agent's workspace), the diff
 * scope and base, and the open file. The diff itself is one cached query all three share.
 */

import { kinds, type Diff, type Json, type JournalEntry } from './types';
import { getShared, setShared, useJournal, useQuery, useShared, type QueryResult } from './runtime';
import { useSelection } from './selection';

export type CodeScope = 'uncommitted' | 'staged' | 'branch' | 'last_turn';

export interface CodeTarget {
  /** Absolute path of the working tree shown. */
  root: string;
  /** What it is, in a word: the project's name, or `@agent`. */
  label: string;
  /** The agent whose workspace this is, if it is one. */
  agent?: string | null;
}

export interface CodeOpen {
  root: string;
  /** Path relative to `root`. */
  path: string;
  /** 1-based line to bring into view. */
  line?: number | null;
}

export interface CodeOptions {
  /** Show every file in the tree, not only changed ones. */
  allFiles: boolean;
  /** Mark changes in the editor (off: the plain file). */
  marks: boolean;
  /** Side by side instead of inline. */
  split: boolean;
  /** The list of every changed file's diff instead of one file. */
  list: boolean;
  /** Markdown files: rendered instead of source. */
  rendered: boolean;
  /** Long lines wrap at the pane's edge (off: they scroll sideways). */
  wrap: boolean;
}

export const DEFAULT_CODE_OPTIONS: CodeOptions = { allFiles: false, marks: true, split: false, list: false, rendered: false, wrap: true };

/** The tree the code view shows: chosen explicitly, else the selected session's workspace. */
export function useCodeTarget(): { target: CodeTarget | null; setTarget: (target: CodeTarget | null) => void } {
  const [chosen, setChosen] = useShared<CodeTarget | null>('code.target', null);
  const { workspace, project, session } = useSelection();
  const fallback: CodeTarget | null = workspace
    ? { root: workspace, label: session?.kind === 'agent' ? `@${session.agent ?? session.title}` : project?.name ?? workspace, agent: session?.kind === 'agent' ? session.agent ?? null : null }
    : null;
  // A choice made for another project does not carry over.
  const target = chosen && project && (chosen.root === project.root || chosen.agent) ? chosen : fallback;
  return { target, setTarget: setChosen };
}

export function useCodeScope() {
  return useShared<CodeScope>('code.scope', 'uncommitted');
}

/** The ref the branch scope compares against (default: the vcs plugin's base). */
export function useCodeBase() {
  return useShared<string | null>('code.base', null);
}

export function useCodeOpen() {
  return useShared<CodeOpen | null>('code.open', null);
}

export function useCodeOptions(): [CodeOptions, (patch: Partial<CodeOptions>) => void] {
  const [options, setOptions] = useShared<CodeOptions>('code.options', DEFAULT_CODE_OPTIONS);
  const merged = { ...DEFAULT_CODE_OPTIONS, ...options };
  return [merged, (patch) => setOptions((current) => ({ ...DEFAULT_CODE_OPTIONS, ...current, ...patch }))];
}

/** Open a file in the code view (and bring the view to the front). */
export function openInCode(open: CodeOpen): void {
  setShared('code.open', open);
  showPane('code');
}

/** Paths the last turn of a session changed (its tool results' `changed_files`). */
export function lastTurnFiles(entries: readonly JournalEntry[]): string[] {
  let start = 0;
  for (let i = entries.length - 1; i >= 0; i--) {
    if (entries[i]!.kind === kinds.USER_MESSAGE) {
      start = i;
      break;
    }
  }
  const paths = new Set<string>();
  for (const entry of entries.slice(start)) {
    if (entry.kind !== kinds.TOOL_RESULT) continue;
    const changed = entry.data.changed_files;
    if (Array.isArray(changed)) for (const path of changed as Json[]) if (typeof path === 'string') paths.add(path);
  }
  return [...paths];
}

/** Whether an event is about a tree (`{root}` from vcs, `{workspace}` from tools). */
export function aboutRoot(root: string | null) {
  return (_topic: string, data: unknown) => {
    const record = (data ?? {}) as { root?: string; workspace?: string };
    const about = record.root ?? record.workspace;
    return !about || !root || about === root;
  };
}

/** The diff the code view shows, shared by its plugins (one cached query per tree and scope). */
export function useCodeDiff(): QueryResult<Diff> & { scope: CodeScope; root: string | null } {
  const { target } = useCodeTarget();
  const [scope] = useCodeScope();
  const [base] = useCodeBase();
  const { sessionId } = useSelection();
  const journal = useJournal(scope === 'last_turn' ? sessionId : null);
  const turn = scope === 'last_turn' ? lastTurnFiles(journal.entries) : null;
  const root = target?.root ?? null;
  const key = root ? `code.diff:${root}:${scope}:${base ?? ''}:${turn?.join('|') ?? ''}` : null;
  const result = useQuery<Diff>(
    key,
    async (client) => {
      if (scope === 'last_turn') {
        const diff = await client.call<Diff>('vcs.diff', { root, scope: 'uncommitted' });
        const files = diff.files.filter((file) => turn!.some((path) => path === file.path || path.endsWith(`/${file.path}`)));
        let added = 0;
        let removed = 0;
        for (const file of files) for (const hunk of file.hunks) for (const line of hunk.lines) {
          if (line.kind === '+') added++;
          else if (line.kind === '-') removed++;
        }
        return { files, added, removed };
      }
      return client.call<Diff>('vcs.diff', base && scope === 'branch' ? { root, scope, base } : { root, scope });
    },
    { topics: ['vcs.changed', 'tools.files_changed'], when: aboutRoot(root) },
  );
  return { ...result, scope, root };
}

/** Bring a right-pane view to the front: the pane opens (a collapsed pane would hide what was
 *  asked for) and the card and overlay that cover it close. */
export function showPane(view: string): void {
  setShared('pane.tab', view);
  setShared('layout.paneCollapsed', false);
  setShared('card.open', false);
  setShared('overlay.open', false);
}

export function currentPane(): string | undefined {
  return getShared<string>('pane.tab');
}
