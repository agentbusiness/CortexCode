import { useState } from 'react';
import { DiffView, hunkCounts, type FileDiff, type Json } from '@cortex/sdk';
import { displayDiffs } from './StepRow';
import { str, type Step, type Turn } from './transcript';

/** One file a turn's edits changed: its diffs in the order they were made, and their line counts. */
export interface EditedFile {
  path: string;
  added: number;
  removed: number;
  /** The turn created the file (its first edit wrote it whole). */
  created: boolean;
  diffs: FileDiff[];
}

/** A turn's edits by file, in the order the files were first touched; the counts add up. */
export function editedFiles(turn: Turn): EditedFile[] {
  const files = new Map<string, EditedFile>();
  for (const block of turn.blocks) {
    if (block.kind !== 'steps') continue;
    for (const step of block.steps) {
      const result = step.result;
      if (!result || result.data.ok === false) continue;
      const display = result.data.display;
      const created = Boolean(display && typeof display === 'object' && !Array.isArray(display) && (display as Record<string, Json>).created === true);
      for (const diff of displayDiffs(display)) {
        const path = diff.path || str((display as Record<string, Json> | undefined)?.path);
        if (!path) continue;
        const counts = hunkCounts(diff.hunks);
        const file = files.get(path) ?? { path, added: 0, removed: 0, created: false, diffs: [] };
        file.added += counts.added;
        file.removed += counts.removed;
        file.created = file.created || created;
        file.diffs.push(diff);
        files.set(path, file);
      }
    }
  }
  return [...files.values()];
}

/** The steps that edited files, for the undo (newest last). */
export function editSteps(turn: Turn): Step[] {
  return turn.blocks.flatMap((block) => (block.kind === 'steps' ? block.steps.filter((step) => step.result && step.result.data.ok !== false && displayDiffs(step.result.data.display).length > 0) : []));
}

/** `Edited 3 files  +250 -0`, then the files with their counts, the first three shown, the rest on demand. */
export function editsTitle(files: readonly EditedFile[]): string {
  return `Edited ${files.length} file${files.length === 1 ? '' : 's'}`;
}

const SHOWN = 3;

/**
 * What the turn changed, once it is done: one card listing the files with their added and removed
 * lines, a View changes that opens every diff in place, and Undo, which puts the files back as
 * they were before the turn (its own hunks reverse-applied; a file the turn created stays, empty).
 */
export function Edits({ turn, onOpenFile, onUndo }: { turn: Turn; onOpenFile: (path: string) => void; onUndo: ((files: EditedFile[]) => Promise<void>) | null }) {
  const files = editedFiles(turn);
  const [all, setAll] = useState(false);
  const [viewing, setViewing] = useState(false);
  const [state, setState] = useState<'ready' | 'undoing' | 'undone'>('ready');
  const [error, setError] = useState<string | null>(null);
  if (files.length === 0) return null;
  const added = files.reduce((sum, file) => sum + file.added, 0);
  const removed = files.reduce((sum, file) => sum + file.removed, 0);
  const shown = all ? files : files.slice(0, SHOWN);
  const undo = async () => {
    if (!onUndo) return;
    setState('undoing');
    setError(null);
    try {
      await onUndo(files);
      setState('undone');
    } catch (cause) {
      setState('ready');
      setError(cause instanceof Error ? cause.message : String(cause));
    }
  };
  return (
    <div className="cx-edits" data-files={files.length} data-state={state}>
      <div className="cx-edits-h">
        <span className="cx-edits-title">{editsTitle(files)}</span>
        <span className="cx-edits-counts"><span className="cx-add">+{added}</span> <span className="cx-del">-{removed}</span></span>
        <span className="grow" />
        {onUndo && (
          <button type="button" className="detent" data-action="undo" disabled={state !== 'ready'} onClick={() => void undo()}
            title="Put these files back as they were before this turn">
            {state === 'undoing' ? 'Undoing…' : state === 'undone' ? 'Undone' : 'Undo'}
          </button>
        )}
        <button type="button" className="detent" data-action="view" aria-expanded={viewing} onClick={() => setViewing(!viewing)}>{viewing ? 'Hide changes' : 'View changes'}</button>
      </div>
      <ol className="cx-edits-files">
        {shown.map((file) => {
          const slash = file.path.lastIndexOf('/');
          const dir = slash >= 0 ? file.path.slice(0, slash + 1) : '';
          const name = slash >= 0 ? file.path.slice(slash + 1) : file.path;
          return (
            <li key={file.path} data-path={file.path}>
              <button type="button" className="cx-edits-file" onClick={() => onOpenFile(file.path)} title={`Open ${file.path} in the editor`}>
                <span className="cx-edits-path">{dir && <span className="cx-edits-dir">{dir}</span>}<span className="cx-edits-name">{name}</span></span>
                <span className="grow" />
                <span className="cx-edits-counts"><span className="cx-add">+{file.added}</span> <span className="cx-del">-{file.removed}</span></span>
              </button>
            </li>
          );
        })}
      </ol>
      {files.length > SHOWN && (
        <button type="button" className="cx-edits-more" data-action="more" onClick={() => setAll(!all)}>
          {all ? 'Show fewer files' : `Show ${files.length - SHOWN} more file${files.length - SHOWN === 1 ? '' : 's'}`}
        </button>
      )}
      {viewing && <div className="cx-edits-view"><DiffView files={files.flatMap((file) => file.diffs)} onOpenFile={onOpenFile} /></div>}
      {error && <p className="inline-error">Not undone: {error}</p>}
    </div>
  );
}
