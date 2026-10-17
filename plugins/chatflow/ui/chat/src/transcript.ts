/**
 * From journal entries to what the transcript draws. Purely structural: entries are grouped by the
 * user message that opened their turn, tool calls are paired with their results by `call_id`, and
 * consecutive steps fold into one run.
 */

import { kinds, type Json, type JournalEntry } from '@cortex/sdk';

export interface Step {
  callId: string;
  call: JournalEntry;
  result: JournalEntry | null;
}

export type Block =
  | { kind: 'steps'; key: string; steps: Step[] }
  | { kind: 'entry'; key: string; entry: JournalEntry };

export interface Turn {
  key: string;
  /** The user message that opened the turn (absent for entries before the first one). */
  user: JournalEntry | null;
  blocks: Block[];
  /** The routes the conductor chose during this turn, in order (e.g. LOOKUP, then ANSWER). */
  routes: JournalEntry[];
  usage: JournalEntry[];
  startedMs: number;
}

export function str(value: Json | undefined): string {
  return typeof value === 'string' ? value : '';
}

/** Group a session's entries into turns. */
export function buildTurns(entries: readonly JournalEntry[]): Turn[] {
  const results = new Map<string, JournalEntry>();
  const calls = new Set<string>();
  for (const entry of entries) {
    if (entry.kind === kinds.TOOL_RESULT) {
      const id = str(entry.data.call_id);
      if (id) results.set(id, entry);
    } else if (entry.kind === kinds.TOOL_CALL) {
      const id = str(entry.data.call_id);
      if (id) calls.add(id);
    }
  }
  const turns: Turn[] = [];
  let turn: Turn | null = null;
  const open = (user: JournalEntry | null, at: JournalEntry): Turn => {
    const next: Turn = { key: `t${at.seq}`, user, blocks: [], routes: [], usage: [], startedMs: at.ts_ms };
    turns.push(next);
    return next;
  };
  const paired = new Set<string>();
  for (const entry of entries) {
    if (entry.kind === kinds.USER_MESSAGE) {
      turn = open(entry, entry);
      continue;
    }
    if (!turn) turn = open(null, entry);
    switch (entry.kind) {
      case kinds.TOOL_RESULT: {
        // Drawn with its call; a result whose call is missing still gets a row.
        const id = str(entry.data.call_id);
        if (id && paired.has(id)) continue;
        if (!id || !calls.has(id)) {
          pushStep(turn, { callId: id || `r${entry.seq}`, call: entry, result: entry });
        }
        continue;
      }
      case kinds.TOOL_CALL: {
        const id = str(entry.data.call_id) || `c${entry.seq}`;
        paired.add(id);
        pushStep(turn, { callId: id, call: entry, result: results.get(id) ?? null });
        continue;
      }
      case kinds.ROUTE:
        turn.routes.push(entry);
        continue;
      case kinds.USAGE:
        turn.usage.push(entry);
        continue;
      default:
        turn.blocks.push({ kind: 'entry', key: `e${entry.seq}`, entry });
    }
  }
  return turns;
}

function pushStep(turn: Turn, step: Step): void {
  const last = turn.blocks[turn.blocks.length - 1];
  if (last && last.kind === 'steps') {
    last.steps.push(step);
    return;
  }
  turn.blocks.push({ kind: 'steps', key: `s${step.call.seq}`, steps: [step] });
}

export interface RunState {
  running: boolean;
  /** When the running run started. */
  since: number | null;
  /** The last lifecycle detail (`status.detail`) of the running run. */
  detail: string;
  /** The stage the running run reports on its status entry (`status.stage`), lower-case. */
  stage: string;
}

/** A run is running from a `status{state:"running"}` until a later status says otherwise. */
export function runState(entries: readonly JournalEntry[]): RunState {
  for (let i = entries.length - 1; i >= 0; i--) {
    const entry = entries[i]!;
    if (entry.kind !== kinds.STATUS) continue;
    const state = str(entry.data.state);
    if (state === 'running') return { running: true, since: entry.ts_ms, detail: str(entry.data.detail), stage: str(entry.data.stage).toLowerCase() };
    return { running: false, since: null, detail: str(entry.data.detail), stage: '' };
  }
  return { running: false, since: null, detail: '', stage: '' };
}

/** The last line of text written so far (for the one-line live indication of the model's reasoning). */
export function latestLine(text: string): string {
  const lines = text.split('\n')
    .map((line) => line.replace(/^\s*(?:[-*+>]|#+|\d+\.)\s+/, '').replace(/[*_`]+/g, '').trim())
    .filter(Boolean);
  return lines.at(-1) ?? '';
}

/** The steps of the run that is still going: those after the last user message. */
export function liveSteps(turns: readonly Turn[]): Step[] {
  const last = turns.at(-1);
  if (!last) return [];
  return last.blocks.flatMap((block) => (block.kind === 'steps' ? block.steps : []));
}

/** Text of the newest reasoning entry still streaming in the last turn. */
export function liveReasoning(turns: readonly Turn[]): string {
  const last = turns.at(-1);
  if (!last) return '';
  for (let i = last.blocks.length - 1; i >= 0; i--) {
    const block = last.blocks[i]!;
    if (block.kind === 'entry' && block.entry.kind === kinds.REASONING) {
      return block.entry.data.streaming === true ? str(block.entry.data.text) : '';
    }
  }
  return '';
}

/** Whether the turn's answer has started streaming (then the working area steps aside). */
export function answerStarted(turn: Turn | undefined): boolean {
  if (!turn) return false;
  return turn.blocks.some((block) => block.kind === 'entry' && block.entry.kind === kinds.ASSISTANT_MESSAGE && str(block.entry.data.text) !== '');
}

/** What a route entry says about the work: its detail text, or the brief or work it carries. */
export function routeText(entry: JournalEntry): string {
  const detail = entry.data.detail;
  if (typeof detail === 'string') return detail;
  if (detail && typeof detail === 'object' && !Array.isArray(detail)) {
    const record = detail as Record<string, unknown>;
    for (const key of ['brief', 'work', 'note', 'missing']) {
      if (typeof record[key] === 'string' && record[key]) return record[key] as string;
    }
  }
  return '';
}
