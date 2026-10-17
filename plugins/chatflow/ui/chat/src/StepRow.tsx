import { useState } from 'react';
import { DiffView, duration, hunkCounts, parseUnifiedDiff, useNow, type FileDiff, type Json, type JournalEntry } from '@cortex/sdk';
import { useContext } from 'react';
import { RevealContext, holds } from './TurnView';
import { str, type Step } from './transcript';

/** The tool's name as the row's label. */
const TOOL_LABEL: Record<string, string> = {
  read: 'Read',
  edit: 'Edit',
  run: 'Run',
  cx: 'cx',
  write: 'Write',
  search: 'Search',
};

/** `read` -> Read; a host call like `memory.search` is labelled by its verb: Search. */
export function toolLabel(tool: string): string {
  if (TOOL_LABEL[tool]) return TOOL_LABEL[tool];
  const verb = tool.split('.').pop() ?? tool;
  return verb.charAt(0).toUpperCase() + verb.slice(1).replace(/_/g, ' ');
}

/**
 * Diffs a tool result carries for the UI (`ToolOutcome.display`): a unified diff string under
 * `diff` / `patch`, or vcs-shaped files with hunks under `files` / `diffs`, or one file's `hunks`.
 */
export function displayDiffs(display: Json | undefined): FileDiff[] {
  if (!display || typeof display !== 'object' || Array.isArray(display)) return [];
  const record = display as Record<string, Json>;
  for (const key of ['diff', 'patch', 'unified_diff']) {
    const value = record[key];
    if (typeof value === 'string' && value.includes('@@')) {
      const files = parseUnifiedDiff(value);
      if (files.length) return files.map((file) => ({ ...file, path: file.path || str(record.path) }));
    }
  }
  for (const key of ['files', 'diffs']) {
    const value = record[key];
    if (Array.isArray(value)) {
      const files = value.filter(isFileDiff);
      if (files.length) return files as unknown as FileDiff[];
    }
  }
  if (Array.isArray(record.hunks) && typeof record.path === 'string') {
    const candidate = { path: record.path, status: str(record.status) || 'modified', binary: false, hunks: record.hunks } as unknown;
    if (isFileDiff(candidate as Json)) return [candidate as FileDiff];
  }
  return [];
}

function isFileDiff(value: Json): boolean {
  if (!value || typeof value !== 'object' || Array.isArray(value)) return false;
  const record = value as Record<string, Json>;
  return typeof record.path === 'string' && Array.isArray(record.hunks)
    && record.hunks.every((hunk) => Boolean(hunk) && typeof hunk === 'object' && Array.isArray((hunk as Record<string, Json>).lines));
}

/** The lines an edit result added and removed, over every diff it carries; null when it has none. */
export function editCounts(result: JournalEntry): { added: number; removed: number } | null {
  const diffs = displayDiffs(result.data.display);
  if (diffs.length === 0) return null;
  let added = 0;
  let removed = 0;
  for (const diff of diffs) {
    const counts = hunkCounts(diff.hunks);
    added += counts.added;
    removed += counts.removed;
  }
  return { added, removed };
}

/** What ran, in one line: the intent the model gave, else its command or path. */
function subject(call: JournalEntry): string {
  const intent = str(call.data.intent);
  if (intent) return intent;
  const args = call.data.args;
  if (args && typeof args === 'object' && !Array.isArray(args)) {
    const record = args as Record<string, Json>;
    for (const key of ['command', 'path', 'target', 'query', 'file']) {
      if (typeof record[key] === 'string' && record[key]) return record[key] as string;
    }
  }
  return str(call.data.tool) || 'tool call';
}

/** Arguments the host adds to every call (which project, which checkout): not what the step did. */
const HOST_ARGS = new Set(['project_id', 'workspace', 'session_id', 'run_id', 'intent']);

/** The call's arguments as plain `name  value` lines (a command alone as itself), never as JSON. */
function argsText(call: JournalEntry): string {
  const args = call.data.args;
  if (args === undefined || args === null) return '';
  if (typeof args !== 'object' || Array.isArray(args)) return plain(args);
  const shown = Object.entries(args as Record<string, Json>).filter(([key, value]) => !HOST_ARGS.has(key) && value !== null && value !== '');
  const command = shown.find(([key]) => key === 'command');
  if (command && shown.length === 1) return plain(command[1]);
  const width = Math.max(0, ...shown.map(([key]) => key.length));
  return shown.map(([key, value]) => `${key.replace(/_/g, ' ').padEnd(width)}  ${plain(value)}`).join('\n');
}

function plain(value: Json): string {
  if (typeof value === 'string') return value;
  if (Array.isArray(value) && value.every((item) => typeof item !== 'object' || item === null)) return value.join(', ');
  if (value && typeof value === 'object') return JSON.stringify(value);
  return String(value);
}

export function StepRow({ step, onOpenFile }: { step: Step; onOpenFile: (path: string) => void }) {
  const { call, result } = step;
  const isResultOnly = call === result;
  const running = !result;
  const ok = result ? result.data.ok !== false : true;
  const status = running ? 'running' : ok ? 'done' : 'failed';
  const now = useNow(1000, running);
  // A step something asked to show opens by itself; the person's choice wins after that.
  const reveal = useContext(RevealContext);
  const [chosen, setOpen] = useState<boolean | null>(null);
  const open = chosen ?? holds(step, reveal.seq);
  const tool = str(call.data.tool) || str(result?.data.tool) || 'tool';
  const diffs = result ? displayDiffs(result.data.display) : [];
  const counts = result ? editCounts(result) : null;
  // A finished edit says what it changed (+added -removed), the rest how long it took.
  const end = running
    ? `${Math.max(0, Math.floor((now - call.ts_ms) / 1000))}s`
    : counts ? `+${counts.added} -${counts.removed}`
      : typeof result?.data.duration_ms === 'number' ? duration(result.data.duration_ms) : '';
  const changed = Array.isArray(result?.data.changed_files) ? (result!.data.changed_files as Json[]).filter((p): p is string => typeof p === 'string') : [];
  // While it runs, what it has printed so far; the result then carries the whole output.
  const output = result ? str(result.data.output) : str(call.data.live_output);
  const command = isResultOnly ? '' : argsText(call);
  return (
    <div className={`run-item run-item-${status}`} data-call={step.callId} data-status={status} data-call-seq={step.result?.seq ?? step.call.seq}
      data-step-seqs={[step.call.seq, step.result?.seq].filter((seq) => typeof seq === 'number').join(' ')}>
      <button type="button" className={`stream-row run-step run-step-${status}`} aria-expanded={open} onClick={() => setOpen(!open)}>
        <span className="dot" aria-hidden />
        <span className="who">{toolLabel(tool)}</span>
        <span className="subject" title={subject(call)}>{subject(call)}</span>
        <span className={`row-end${counts ? ' cx-edit-counts' : ''}`}>
          {counts && !running ? <><span className="cx-add">+{counts.added}</span> <span className="cx-del">-{counts.removed}</span></> : end}
          {!running && diffs.length > 1 ? ` · ${diffs.length} files` : ''}
        </span>
      </button>
      {running && !open && output && <pre className="cx-activity-tail run-output">{output.replace(/\s+$/, '').split('\n').slice(-8).join('\n')}</pre>}
      {changed.length > 0 && !open && (
        <div className="step-notes">
          {changed.map((path) => (
            <button key={path} type="button" className="cx-file-link" onClick={() => onOpenFile(path)} title={`Open ${path} in the editor`}>{path}</button>
          ))}
        </div>
      )}
      {open && (
        <div className="grounded-item-card stream-open run-open">
          {command && (
            <>
              <span className="ap faint">{tool === 'run' ? 'Ran' : 'Asked for'}</span>
              <pre className="run-output">{command}</pre>
            </>
          )}
          {diffs.length > 0 && (
            <>
              <span className="ap faint">Change</span>
              <DiffView files={diffs} onOpenFile={onOpenFile} />
            </>
          )}
          {output && (
            <>
              <span className="ap faint">{running ? 'Output so far' : ok ? 'Output' : 'Result'}</span>
              <pre className="run-output">{output}</pre>
            </>
          )}
          {changed.length > 0 && (
            <div className="step-notes cx-flush-notes">
              <span className="ap faint">Changed</span>
              {changed.map((path) => (
                <button key={path} type="button" className="cx-file-link" onClick={() => onOpenFile(path)} title={`Open ${path} in the editor`}>{path}</button>
              ))}
            </div>
          )}
          {running && <span className="ap faint">Running…</span>}
        </div>
      )}
    </div>
  );
}
