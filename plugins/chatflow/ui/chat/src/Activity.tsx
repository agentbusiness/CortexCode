import { useContext, useState } from 'react';
import { Caret, duration, kinds, useNow, type Json, type JournalEntry } from '@cortex/sdk';
import { StepRow, editCounts, toolLabel } from './StepRow';
import { RevealContext, holds } from './TurnView';
import { isFailure, latestLine, quietExit, str, type Block, type Step, type Turn } from './transcript';

/**
 * The scaffolding of a turn's work — its steps, its reasoning, the notes the host wrote for the
 * model — as one line: while the turn runs, what it is doing now (the step under way, else the
 * newest line of its reasoning) and the model's progress; once done, what happened (each step as a
 * verb and its target), how many steps, how long and how many tokens. A click opens the full list
 * in order, one line per item, each opening to its detail. The model's own prose between steps is
 * not scaffolding: the transcript keeps it where it was written.
 */

export type ActivityItem =
  | { kind: 'step'; key: string; step: Step }
  | { kind: 'reasoning'; key: string; entry: JournalEntry }
  | { kind: 'note'; key: string; entry: JournalEntry };

/** Whether a block folds into the activity line (steps, reasoning, a note written for the model). */
export function isScaffolding(block: Block): boolean {
  if (block.kind === 'steps') return true;
  const entry = block.entry;
  if (entry.kind === kinds.REASONING) return true;
  return entry.kind === kinds.NOTE && entry.data.host_message === true && str(entry.data.text) !== '';
}

/** The turn's scaffolding in the order it happened. */
export function activityItems(blocks: readonly Block[]): ActivityItem[] {
  const items: ActivityItem[] = [];
  for (const block of blocks) {
    if (!isScaffolding(block)) continue;
    if (block.kind === 'steps') {
      for (const step of block.steps) items.push({ kind: 'step', key: `s${step.callId}`, step });
    } else if (block.entry.kind === kinds.REASONING) {
      if (str(block.entry.data.text) || block.entry.data.streaming === true) items.push({ kind: 'reasoning', key: block.key, entry: block.entry });
    } else {
      items.push({ kind: 'note', key: block.key, entry: block.entry });
    }
  }
  return items;
}

const TARGET_KEYS = ['path', 'target', 'file', 'command', 'query', 'text', 'uri'];

/** What a step acted on, in a few words: its path, command or query (never its JSON). */
function target(step: Step): string {
  const args = step.call.data.args;
  if (args && typeof args === 'object' && !Array.isArray(args)) {
    const record = args as Record<string, Json>;
    for (const key of TARGET_KEYS) if (typeof record[key] === 'string' && record[key]) return firstLine(record[key] as string);
  }
  return firstLine(str(step.call.data.intent));
}

function firstLine(text: string): string {
  return text.split('\n')[0]!.trim();
}

/**
 * `read calc.py`, `run pytest ✓`, `edit calc.py +12 -3`, `edit calc.py ✗`: the tool's verb, its
 * target and, when done, how it ended (an edit: the lines it added and removed).
 */
export function stepText(step: Step): string {
  const tool = str(step.call.data.tool) || str(step.result?.data.tool) || 'tool';
  const words = [toolLabel(tool).toLowerCase(), target(step)].filter(Boolean).join(' ');
  if (!step.result) return `${words} …`;
  if (isFailure(step.result.data)) return `${words} ✗`;
  const exit = quietExit(step.result.data);
  if (exit !== null) return `${words} · exit ${exit}`;
  const counts = editCounts(step.result);
  if (counts) return `${words} +${counts.added} -${counts.removed}`;
  return tool === 'run' ? `${words} ✓` : words;
}

/**
 * Text as `stepText` and `factsText` write it, coloured: an edit's `+added` green and `-removed`
 * red, a failed step (ending in ✗) and an `N failed` count red. Segments are joined by ` · `.
 */
export function Counted({ text }: { text: string }) {
  const segments = text.split(' · ');
  return (
    <>
      {segments.map((segment, index) => {
        const sep = index > 0 ? ' · ' : '';
        if (segment.endsWith(' ✗') || /^\d+ failed$/.test(segment)) {
          return <span key={index}>{sep}<span className="cx-failed">{segment}</span></span>;
        }
        const parts = segment.split(/(\+\d+ -\d+)/);
        return (
          <span key={index}>
            {sep}
            {parts.map((part, at) => {
              const counts = /^\+(\d+) -(\d+)$/.exec(part);
              if (!counts) return part;
              return <span key={at} className="cx-edits-counts"><span className="cx-add">+{counts[1]}</span> <span className="cx-del">-{counts[2]}</span></span>;
            })}
          </span>
        );
      })}
    </>
  );
}

/** A skill's name from its URI: `skill://lang-python/python-errors` -> `python-errors`. */
export function skillName(uri: string): string {
  return uri.split('/').filter(Boolean).pop() ?? uri;
}

/** A note's one line: the skills it brings, else its first line. */
export function noteLabel(entry: JournalEntry): string {
  const skills = Array.isArray(entry.data.skills) ? (entry.data.skills as unknown[]).map(String) : [];
  if (str(entry.data.purpose) === 'skills' && skills.length) return `Using ${skills.length === 1 ? 'skill' : 'skills'}: ${skills.map(skillName).join(' · ')}`;
  return firstLine(str(entry.data.text)).slice(0, 120);
}

export interface Facts {
  steps: number;
  failed: number;
  /** From the user's message to the turn's last entry (ms). */
  ms: number;
  /** Tokens the model wrote. */
  tokens: number;
}

export function facts(turn: Turn, items: readonly ActivityItem[]): Facts {
  const steps = items.filter((item): item is Extract<ActivityItem, { kind: 'step' }> => item.kind === 'step');
  const stamps = [
    ...turn.blocks.flatMap((block) => (block.kind === 'steps' ? block.steps.flatMap((step) => [step.call.ts_ms, step.result?.ts_ms ?? 0]) : [block.entry.ts_ms])),
    ...turn.routes.map((entry) => entry.ts_ms),
    ...turn.usage.map((entry) => entry.ts_ms),
  ];
  const start = turn.user?.ts_ms ?? turn.startedMs;
  return {
    steps: steps.length,
    failed: steps.filter((item) => item.step.result && isFailure(item.step.result.data)).length,
    ms: Math.max(0, Math.max(start, ...stamps) - start),
    tokens: turn.usage.reduce((sum, entry) => sum + (typeof entry.data.completion_tokens === 'number' ? entry.data.completion_tokens : 0), 0),
  };
}

/** The finished turn's facts in one run of text: `3 steps · 1 failed · 27.0s · 220 tokens`. */
export function factsText(f: Facts): string {
  const parts: string[] = [];
  if (f.steps) parts.push(`${f.steps} step${f.steps === 1 ? '' : 's'}`);
  if (f.failed) parts.push(`${f.failed} failed`);
  if (f.ms) parts.push(duration(f.ms));
  if (f.tokens) parts.push(`${f.tokens.toLocaleString('en-US')} tokens`);
  return parts.join(' · ');
}

/** A step under way, with how long it has run: `run pytest -q … 12s` (`now` from the clock). */
export function liveStepText(step: Step, now?: number): string {
  const text = stepText(step);
  if (step.result || now === undefined) return text;
  return `${text} ${Math.max(0, Math.floor((now - step.call.ts_ms) / 1000))}s`;
}

/**
 * What the line says. Finished: every step as `verb target`, the skills noted first. Running: only
 * what is happening now (the step under way with its elapsed time, else the newest line of the
 * reasoning being written, else the last thing done), so the person sees the command running.
 */
export function whatText(items: readonly ActivityItem[], live: boolean, now?: number): string {
  if (live) {
    for (let i = items.length - 1; i >= 0; i--) {
      const item = items[i]!;
      if (item.kind === 'step') return liveStepText(item.step, now);
      if (item.kind === 'note') return noteLabel(item.entry);
      if (item.entry.data.streaming === true) {
        const thought = latestLine(str(item.entry.data.text));
        if (thought) return thought;
      }
    }
    return '';
  }
  const parts: string[] = [];
  for (const item of items) {
    if (item.kind === 'step') parts.push(stepText(item.step));
    else if (item.kind === 'note') parts.push(noteLabel(item.entry));
  }
  // Only reasoning: the line still says what it holds, never a caret beside nothing.
  const reasoning = items.find((item): item is Extract<ActivityItem, { kind: 'reasoning' }> => item.kind === 'reasoning');
  if (parts.length === 0 && reasoning) parts.push(reasoningLabel(reasoning.entry));
  return parts.join(' · ');
}

/** The last `count` lines of a running command's output so far (`tool_call.live_output`). */
export function outputTail(text: string, count = 8): string {
  const lines = text.replace(/\s+$/, '').split('\n');
  return lines.slice(-count).join('\n');
}

/** The step under way, when the newest item is one. */
export function runningStep(items: readonly ActivityItem[]): Step | null {
  const last = items.at(-1);
  return last?.kind === 'step' && !last.step.result ? last.step : null;
}

export function Activity({ turn, items, live, progress, onOpenFile, closing = true, liveLines = [] }: {
  turn: Turn;
  items: ActivityItem[];
  /** The turn is running now: the list is open, the step under way last. */
  live: boolean;
  /** The model's progress for the running turn (empty when the working area says it). */
  progress: string;
  /** The decision being written now (its free-text fields so far), shown under the line. */
  liveLines?: string[];
  onOpenFile: (path: string) => void;
  /** The turn's last activity line: it carries the turn's time, tokens and model calls. */
  closing?: boolean;
}) {
  const reveal = useContext(RevealContext);
  // Nothing done and nothing measured: no line (never an empty container).
  const nothing = items.length === 0 && turn.usage.length === 0 && !(live && progress);
  const revealed = items.some((item) => item.kind === 'step' && holds(item.step, reveal.seq));
  const [chosen, setOpen] = useState<boolean | null>(null);
  // A step something asked to show opens the list, which then stays open until closed.
  if (revealed && chosen !== true) setOpen(true);
  // Running, the list is open (each step a line, the one under way last); done, one line.
  const open = revealed || (chosen ?? live);
  const now = useNow(1000, live);
  const known = facts(turn, items);
  if (!closing) {
    // An earlier line of the turn says what it did; the closing line says what the turn cost.
    known.ms = 0;
    known.tokens = 0;
  }
  const calls = turn.usage.length;
  // Nothing done of its own (only the turn's cost): the line names the model calls behind it.
  const what = whatText(items, live, now) || (!live && closing && calls ? `${calls} model call${calls === 1 ? '' : 's'}` : '');
  const running = live ? runningStep(items) : null;
  const tail = running ? outputTail(str(running.call.data.live_output)) : '';
  let end: string;
  if (live) {
    const since = turn.user?.ts_ms ?? turn.startedMs;
    const elapsed = `${Math.max(0, Math.floor((now - since) / 1000))}s`;
    end = [known.steps ? `${known.steps} step${known.steps === 1 ? '' : 's'}` : '', elapsed, progress].filter(Boolean).join(' · ');
  } else {
    end = factsText(known);
  }
  if (nothing || (!what && !end)) return null;
  const usage = live || !closing ? '' : usageText(turn.usage);
  return (
    <div className="cx-activity" data-open={open ? 'true' : 'false'} data-running={live ? 'true' : 'false'} data-steps={known.steps} data-failed={known.failed}>
      <button type="button" className="cx-activity-line" aria-expanded={open} onClick={() => setOpen(!open)} title={!open && what ? what : undefined}>
        <Caret dir={open ? 'down' : 'right'} className="thinking-caret" />
        {live && <span className="ap live cx-activity-running">Running</span>}
        {!open && what && <span className="cx-activity-what"><Counted text={what} /></span>}
        {end && <span className="cx-activity-facts"><Counted text={end} /></span>}
      </button>
      {live && liveLines.length > 0 && <div className="cx-activity-live">{liveLines.map((line, index) => <span key={index}>{line}</span>)}</div>}
      {tail && !open && <pre className="cx-activity-tail run-output" data-call={running!.callId}>{tail}</pre>}
      {open && (items.length > 0 || usage) && (
        <ol className="cx-activity-list sources-list" aria-label="What was done">
          {items.map((item) => (
            <li key={item.key} data-item={item.kind}>
              {item.kind === 'step' ? <StepRow step={item.step} onOpenFile={onOpenFile} /> : <Folded entry={item.entry} label={item.kind === 'reasoning' ? reasoningLabel(item.entry) : noteLabel(item.entry)} />}
            </li>
          ))}
          {usage && <li className="cx-activity-usage" data-item="usage">{usage}</li>}
        </ol>
      )}
    </div>
  );
}

function reasoningLabel(entry: JournalEntry): string {
  const first = latestLine(firstLine(str(entry.data.text)));
  return first ? `Reasoning · ${first}` : 'Reasoning';
}

/** Reasoning or a note for the model: one line, its text on demand. */
function Folded({ entry, label }: { entry: JournalEntry; label: string }) {
  const [open, setOpen] = useState(false);
  const streaming = entry.data.streaming === true;
  return (
    <div className="cx-activity-folded" data-seq={entry.seq} data-purpose={str(entry.data.purpose) || undefined}>
      <button type="button" className="cx-activity-row" aria-expanded={open} onClick={() => setOpen(!open)}>
        <Caret dir={open ? 'down' : 'right'} className="thinking-caret" />
        <span className="cx-activity-what">{label}</span>
        {streaming && <span className="thinking-dots" aria-hidden><span /><span /><span /></span>}
      </button>
      {open && <div className="thinking-content cx-activity-text">{str(entry.data.text)}</div>}
    </div>
  );
}

/** The model calls behind a finished turn, for the opened list. */
function usageText(usage: JournalEntry[]): string {
  if (usage.length === 0) return '';
  const sum = (field: string) => usage.reduce((total, entry) => total + (typeof entry.data[field] === 'number' ? (entry.data[field] as number) : 0), 0);
  const prompt = sum('prompt_tokens');
  const cached = sum('cached_tokens');
  const completion = sum('completion_tokens');
  const prefill = sum('prefill_ms');
  const decode = sum('decode_ms');
  // A count of zero is a claim: when nothing was measured, nothing is printed.
  if (prompt + completion + prefill + decode === 0) return '';
  const rate = decode > 0 ? ` · ${(completion / (decode / 1000)).toFixed(1)} tok/s` : '';
  return `${usage.length} model call${usage.length === 1 ? '' : 's'} · read ${prompt.toLocaleString('en-US')} tokens (${cached.toLocaleString('en-US')} cached) in ${duration(prefill)} · wrote ${completion.toLocaleString('en-US')} in ${duration(decode)}${rate}`;
}
