import { useState } from 'react';
import { DiffView, hunkCounts, parseUnifiedDiff, type FileDiff, type Json, type JournalEntry } from '@cortex/sdk';
import { displayDiffs } from './StepRow';
import { str, type Step, type Turn } from './transcript';

/** One file a turn changed: its net line counts and, once fetched or derived, its diffs. */
export interface EditedFile {
  path: string;
  added: number;
  removed: number;
  /** The turn created the file. */
  created: boolean;
  /** The turn deleted the file. */
  deleted: boolean;
  binary: boolean;
  diffs: FileDiff[];
}

/**
 * The turn's Changed note, written by the host from the step log when the turn ended: every file
 * the turn's steps changed (edits, commands' effects, deletions), with the steps' numbers so the
 * change can be undone exactly. Absent for turns recorded before the step log existed.
 */
export interface ChangedNote {
  seq: number;
  workspace: string;
  files: EditedFile[];
  seqs: number[];
  fromSeq: number;
  toSeq: number;
}

export function changedNote(turn: Turn): ChangedNote | null {
  for (const block of turn.blocks) {
    if (block.kind !== 'entry' || block.entry.kind !== 'note' || str(block.entry.data.purpose) !== 'changed') continue;
    const entry: JournalEntry = block.entry;
    const data = entry.data as Record<string, Json>;
    const raw = Array.isArray(data.files) ? (data.files as Json[]) : [];
    const files: EditedFile[] = raw.flatMap((item) => {
      if (!item || typeof item !== 'object' || Array.isArray(item)) return [];
      const record = item as Record<string, Json>;
      const path = str(record.path);
      if (!path) return [];
      return [{
        path,
        added: typeof record.added === 'number' ? record.added : 0,
        removed: typeof record.removed === 'number' ? record.removed : 0,
        created: record.created === true,
        deleted: record.deleted === true,
        binary: record.binary === true,
        diffs: [],
      }];
    });
    const seqs = Array.isArray(data.seqs) ? (data.seqs as Json[]).filter((seq): seq is number => typeof seq === 'number') : [];
    if (!files.length || !seqs.length) continue;
    return {
      seq: entry.seq,
      workspace: str(data.workspace),
      files,
      seqs,
      fromSeq: typeof data.from_seq === 'number' ? data.from_seq : Math.min(...seqs),
      toSeq: typeof data.to_seq === 'number' ? data.to_seq : Math.max(...seqs),
    };
  }
  return null;
}

/** A turn's edits by file from the edit tool's own results (turns from before the step log). */
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
        const file = files.get(path) ?? { path, added: 0, removed: 0, created: false, deleted: false, binary: false, diffs: [] };
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

/** The steps that edited files, for the undo of turns without a Changed note (newest last). */
export function editSteps(turn: Turn): Step[] {
  return turn.blocks.flatMap((block) => (block.kind === 'steps' ? block.steps.filter((step) => step.result && step.result.data.ok !== false && displayDiffs(step.result.data.display).length > 0) : []));
}

export function editsTitle(files: readonly EditedFile[]): string {
  return `Changed ${files.length} file${files.length === 1 ? '' : 's'}`;
}

const SHOWN = 3;

export interface EditsActions {
  onOpenFile: (path: string) => void;
  /** Undo the turn's steps through the step log (null: not possible here). */
  onUndoSteps: ((note: ChangedNote) => Promise<void>) | null;
  /** Undo a turn without a note by reverse-applying its edits (null: not possible here). */
  onUndoLegacy: ((files: EditedFile[]) => Promise<void>) | null;
  /** The net diff of one path over the turn's steps, a unified diff or empty. */
  stepDiff: ((note: ChangedNote, path: string) => Promise<string>) | null;
}

/**
 * What the turn changed, once it is done: one card listing the files with their added and removed
 * lines, a View changes that opens every diff in place, and Undo, which puts the files back as they
 * were before the turn. With the host's Changed note the list is complete (a file a command
 * changed is in it) and the undo is exact (the step log's contents); without it, the edit tool's
 * own results stand in.
 */
export function Edits({ turn, actions }: { turn: Turn; actions: EditsActions }) {
  const note = changedNote(turn);
  const files = note ? note.files : editedFiles(turn);
  const [all, setAll] = useState(false);
  const [viewing, setViewing] = useState(false);
  const [diffs, setDiffs] = useState<FileDiff[] | null>(null);
  const [state, setState] = useState<'ready' | 'undoing' | 'undone'>('ready');
  const [error, setError] = useState<string | null>(null);
  if (files.length === 0) return null;
  const added = files.reduce((sum, file) => sum + file.added, 0);
  const removed = files.reduce((sum, file) => sum + file.removed, 0);
  const shown = all ? files : files.slice(0, SHOWN);
  const canUndo = note ? actions.onUndoSteps !== null : actions.onUndoLegacy !== null;
  const undo = async () => {
    setState('undoing');
    setError(null);
    try {
      if (note && actions.onUndoSteps) await actions.onUndoSteps(note);
      else if (!note && actions.onUndoLegacy) await actions.onUndoLegacy(files);
      setState('undone');
    } catch (cause) {
      setState('ready');
      setError(cause instanceof Error ? cause.message : String(cause));
    }
  };
  const view = async () => {
    if (viewing) {
      setViewing(false);
      return;
    }
    setViewing(true);
    if (!note) {
      setDiffs(files.flatMap((file) => file.diffs));
      return;
    }
    if (diffs || !actions.stepDiff) return;
    const fetched: FileDiff[] = [];
    for (const file of note.files) {
      if (file.binary) continue;
      try {
        const text = await actions.stepDiff(note, file.path);
        if (text) fetched.push(...parseUnifiedDiff(text).map((diff) => ({ ...diff, path: diff.path || file.path })));
      } catch (cause) {
        setError(cause instanceof Error ? cause.message : String(cause));
      }
    }
    setDiffs(fetched);
  };
  return (
    <div className="cx-edits" data-files={files.length} data-state={state} data-source={note ? 'log' : 'edits'}>
      <div className="cx-edits-h">
        <span className="cx-edits-title">{editsTitle(files)}</span>
        <span className="cx-edits-counts"><span className="cx-add">+{added}</span> <span className="cx-del">-{removed}</span></span>
        <span className="grow" />
        {canUndo && (
          <button type="button" className="detent" data-action="undo" disabled={state !== 'ready'} onClick={() => void undo()}
            title={note ? 'Put these files back as they were before this turn (the step log keeps what they were)' : 'Put these files back as they were before this turn'}>
            {state === 'undoing' ? 'Undoing…' : state === 'undone' ? 'Undone' : 'Undo'}
          </button>
        )}
        <button type="button" className="detent" data-action="view" aria-expanded={viewing} onClick={() => void view()}>{viewing ? 'Hide changes' : 'View changes'}</button>
      </div>
      <ol className="cx-edits-files">
        {shown.map((file) => {
          const slash = file.path.lastIndexOf('/');
          const dir = slash >= 0 ? file.path.slice(0, slash + 1) : '';
          const name = slash >= 0 ? file.path.slice(slash + 1) : file.path;
          return (
            <li key={file.path} data-path={file.path} data-deleted={file.deleted || undefined} data-created={file.created || undefined}>
              <button type="button" className="cx-edits-file" onClick={() => actions.onOpenFile(file.path)} title={file.deleted ? `${file.path} was deleted` : `Open ${file.path} in the editor`} disabled={file.deleted}>
                <span className="cx-edits-path">{dir && <span className="cx-edits-dir">{dir}</span>}<span className="cx-edits-name">{name}</span></span>
                {file.created && <span className="ap faint">new</span>}
                {file.deleted && <span className="ap faint">deleted</span>}
                <span className="grow" />
                {file.binary ? <span className="ap faint">binary</span> : <span className="cx-edits-counts"><span className="cx-add">+{file.added}</span> <span className="cx-del">-{file.removed}</span></span>}
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
      {viewing && diffs && diffs.length > 0 && <div className="cx-edits-view"><DiffView files={diffs} onOpenFile={actions.onOpenFile} /></div>}
      {viewing && diffs && diffs.length === 0 && <p className="cx-note">No text changes to show.</p>}
      {viewing && !diffs && <p className="cx-note">Loading the changes…</p>}
      {error && <p className="inline-error">Not undone: {error}</p>}
    </div>
  );
}
