/**
 * Who changed what: a hunk is attributed to the journal step whose edit added exactly its lines
 * (structural: the added lines of the step's own diff), else to the file's newest edit.
 */

import { kinds, parseUnifiedDiff, type FileDiff, type Hunk, type Json, type JournalEntry, type Session } from '@cortex/sdk';

// ─── who changed what (journal attribution) ───────────────────────────────────────────────────

export interface Edit {
  sessionId: string;
  /** The tool_result entry that made the edit. */
  seq: number;
  ts: number;
  path: string;
  /** Lines the edit added (from its unified diff). */
  added: string[];
  /** The user message of the turn the edit belongs to. */
  turn: string;
}

function strValue(value: Json | undefined): string {
  return typeof value === 'string' ? value : '';
}

/** Edits recorded in one session's journal (tool results that changed files, with their diff). */
export function editsFromJournal(sessionId: string, entries: readonly JournalEntry[]): Edit[] {
  const out: Edit[] = [];
  let turn = '';
  for (const entry of entries) {
    if (entry.kind === kinds.USER_MESSAGE) turn = strValue(entry.data.text);
    if (entry.kind !== kinds.TOOL_RESULT) continue;
    const changed = Array.isArray(entry.data.changed_files) ? (entry.data.changed_files as Json[]).filter((p): p is string => typeof p === 'string') : [];
    if (changed.length === 0) continue;
    const display = entry.data.display;
    const diffText = display && typeof display === 'object' && !Array.isArray(display) ? strValue((display as Record<string, Json>).diff) : '';
    const files: FileDiff[] = diffText ? parseUnifiedDiff(diffText) : [];
    for (const path of changed) {
      const file = files.find((candidate) => candidate.path === path || candidate.path.endsWith(`/${path}`) || path.endsWith(`/${candidate.path}`));
      const added = file ? file.hunks.flatMap((hunk) => hunk.lines.filter((line) => line.kind === '+').map((line) => line.text)) : [];
      out.push({ sessionId, seq: entry.seq, ts: entry.ts_ms, path, added, turn });
    }
  }
  return out;
}

export interface Attribution {
  edit: Edit;
  /** All of the hunk's added lines were written by this edit (otherwise: the file's latest edit). */
  exact: boolean;
}

/**
 * For each hunk, the newest edit whose added lines include every added line of the hunk (exact),
 * else the newest edit of the file (not exact), else nothing.
 */
export function attributeHunks(path: string, hunks: readonly Hunk[], edits: readonly Edit[]): Array<Attribution | null> {
  const mine = edits.filter((edit) => edit.path === path || edit.path.endsWith(`/${path}`) || path.endsWith(`/${edit.path}`)).sort((a, b) => b.ts - a.ts);
  return hunks.map((hunk) => {
    const added = hunk.lines.filter((line) => line.kind === '+').map((line) => line.text);
    if (added.length > 0) {
      const exact = mine.find((edit) => added.every((line) => edit.added.includes(line)));
      if (exact) return { edit: exact, exact: true };
    }
    return mine[0] ? { edit: mine[0], exact: false } : null;
  });
}

/** How a session is named in an attribution: the agent, else the conversation title. */
export function sessionLabel(session: Session | undefined): string {
  if (!session) return 'a session';
  if (session.kind === 'agent') return `@${session.agent ?? session.title}`;
  return session.title.trim() || 'the conversation';
}

