import { useState } from 'react';
import { DiffView, errorMessage, parseUnifiedDiff, useEvent, useQuery, type FileDiff, type JournalEntry, type SourceFileDiff, type SourceProgress } from '@cortex/sdk';
import { str } from './transcript';

const LABEL: Record<string, string> = {
  pending: 'Needs you',
  building: 'Building…',
  done: 'Built and swapped',
  failed: 'Build failed',
  rolled_back: 'Rolled back',
  declined: 'Declined',
};

const TONE: Record<string, string> = {
  pending: 'commitment',
  building: 'context',
  done: 'context',
  failed: 'contradiction',
  rolled_back: 'overdue',
  declined: 'overdue',
};

interface Units { units?: Array<{ id: string; name?: string }> }

/** Plugin ids to the names the person knows them by (`frame.plugins.list` units); the id otherwise. */
function usePluginNames(): (id: string) => string {
  const names = useQuery<Record<string, string>>('frame.plugins.list:names', async (c) => {
    const result = await c.call<Units>('frame.plugins.list', {});
    return Object.fromEntries((result?.units ?? []).map((unit) => [unit.id, unit.name || unit.id]));
  }, { topics: ['plugin.unit'] });
  return (id) => names.data?.[id] || id;
}

/** `source.diff` files as the diff view draws them (one unified diff per file). */
function diffFiles(files: readonly SourceFileDiff[]): FileDiff[] {
  return files.map((file) => {
    const parsed = parseUnifiedDiff(file.patch)[0];
    return { path: file.path, old_path: parsed?.old_path ?? null, status: file.status, binary: parsed?.binary ?? false, hunks: parsed?.hunks ?? [] };
  });
}

/**
 * A code change to CortexCode itself (`change`, a source proposal): what changes in plain words,
 * then why and what else it can affect, with Approve and Decline while it waits; Show the change
 * opens the diff. An answered card stays with its outcome: building (the build's latest line),
 * built and swapped, failed, rolled back, declined.
 */
export function ChangeCard({ entry, onDecide }: { entry: JournalEntry; onDecide: (id: string, approve: boolean) => Promise<void> }) {
  const data = entry.data;
  const id = str(data.id);
  const state = str(data.state) || 'pending';
  const name = usePluginNames();
  const plugins = Array.isArray(data.plugins) ? data.plugins.filter((plugin): plugin is string => typeof plugin === 'string') : [];
  const [busy, setBusy] = useState<'approve' | 'decline' | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [showing, setShowing] = useState(false);
  const [progress, setProgress] = useState('');
  const diff = useQuery<FileDiff[]>(showing && id ? `source.diff:proposal:${id}` : null, async (c) => diffFiles((await c.call<{ files: SourceFileDiff[] }>('source.diff', { proposal: id }))?.files ?? []));
  useEvent(state === 'building' ? 'source.progress' : null, (_topic, payload) => {
    const step = payload as SourceProgress;
    if (step?.proposal !== id) return;
    const what = step.plugin ? name(step.plugin) : 'the change';
    setProgress(step.detail || (step.state === 'started' ? `Building ${what}` : step.state === 'done' ? `Built ${what}` : step.state === 'failed' ? `${what} failed` : ''));
  });
  const answer = async (approve: boolean) => {
    setBusy(approve ? 'approve' : 'decline');
    setError(null);
    try {
      await onDecide(id, approve);
    } catch (cause) {
      setError(errorMessage(cause));
    } finally {
      setBusy(null);
    }
  };
  const pending = state === 'pending';
  const outcome = str(data.outcome);
  return (
    <div className={`notice cx-change ${TONE[state] ?? 'context'}`} data-seq={entry.seq} data-state={state} data-proposal={id} role={pending ? 'alert' : undefined}>
      <span className="kindmark" />
      <span className="ap lbl">{LABEL[state] ?? state.replace(/_/g, ' ')}</span>
      <p className="cx-change-title">Change to {plugins.length ? plugins.map(name).join(', ') : 'CortexCode'}</p>
      {str(data.summary) && <p>{str(data.summary)}</p>}
      {str(data.why) && <p className="cx-change-line">Why: {str(data.why)}</p>}
      {str(data.risk) && <p className="cx-change-line">Risk: {str(data.risk)}</p>}
      {state === 'building' && <p className="cx-change-progress">{progress || 'Starting the build…'}</p>}
      {!pending && state !== 'building' && outcome && <p className="cx-change-outcome">{outcome}</p>}
      <div className="acts">
        {pending && <button className="detent" type="button" data-action="approve" disabled={busy !== null} onClick={() => void answer(true)}>{busy === 'approve' ? 'Approving…' : 'Approve'}</button>}
        {pending && <button className="detent warn" type="button" data-action="decline" disabled={busy !== null} onClick={() => void answer(false)}>{busy === 'decline' ? 'Declining…' : 'Decline'}</button>}
        <button className="detent" type="button" data-action="show-change" aria-expanded={showing} onClick={() => setShowing(!showing)}>{showing ? 'Hide the change' : 'Show the change'}</button>
      </div>
      {error && <p className="inline-error">Not answered: {error}</p>}
      {showing && (
        <div className="cx-change-diff">
          {diff.data ? (diff.data.length ? <DiffView files={diff.data} /> : <p className="hint">No file changes.</p>)
            : diff.error ? <p className="inline-error">The change could not be read: {diff.error}</p>
            : <p className="hint">Reading the change…</p>}
        </div>
      )}
    </div>
  );
}
