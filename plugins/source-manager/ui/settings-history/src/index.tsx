/**
 * settings.history@1 — every change to CortexCode's live data (skills, prompts, texts), newest
 * first (`source.history`): when, where it came from in a plain word, its summary, the plugins it
 * touched; Revert (after a short confirm) and Show the change per change. At the top, whether the
 * live data is up to date with the installed version (or which files conflict), with Update now.
 */

import { useState } from 'react';
import {
  DiffView,
  ServiceProblem,
  clock,
  definePlugin,
  errorMessage,
  parseUnifiedDiff,
  useClient,
  useQuery,
  type FileDiff,
  type SourceFileDiff,
} from '@cortex/sdk';
import './history.css';

/** `source.history` (`protocol/src/contracts/source.rs`, `Change`). */
interface Change {
  commit: string;
  ms: number;
  origin: string;
  summary: string;
  plugins: string[];
  files?: string[];
  reverted_by?: string | null;
  shared?: string | null;
}

interface Status {
  managed: boolean;
  update: string;
  conflicts?: string[];
}

interface UpdateResult {
  state: string;
  conflicts?: string[];
}

// `edit` is any data edit (the user's or the model's): the live folder cannot tell them apart.
const ORIGIN: Record<string, string> = {
  edit: 'Edit',
  update: 'Update',
  restore: 'Restored',
  revert: 'Reverted',
  shipped: 'Shipped',
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

function UpdateState() {
  const client = useClient();
  const status = useQuery<Status>('source.status', (c) => c.call<Status>('source.status', {}), { topics: ['source.changed'] });
  const [busy, setBusy] = useState(false);
  const [result, setResult] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const update = async () => {
    setBusy(true);
    setError(null);
    setResult(null);
    try {
      const done = await client.call<UpdateResult>('source.update', {});
      setResult(done.state === 'merged' ? 'Updated.'
        : done.state === 'up_to_date' ? 'Already up to date.'
        : done.state === 'conflicts' ? 'The update could not be merged: some files conflict.'
        : 'There is no installed version to update from.');
      await status.refresh();
    } catch (cause) {
      setError(errorMessage(cause));
    } finally {
      setBusy(false);
    }
  };
  const s = status.data;
  if (!s) return status.error ? <ServiceProblem service="source manager" state={status} className="inline-error" /> : null;
  const conflicts = s.conflicts ?? [];
  return (
    <section className="cx-history-update" data-update={s.update}>
      <div className="row">
        <div className="grow">
          <div className="nm">{s.update === 'conflicts' ? 'The last update could not be merged' : s.update === 'no_source' ? 'No installed version to update from' : 'Up to date with the installed version'}</div>
          {!s.managed && <div className="cx-history-note">This is a developer checkout: nothing is committed or merged here automatically.</div>}
        </div>
        <button className="detent" type="button" data-action="update" disabled={busy || !s.managed || s.update === 'no_source'} onClick={() => void update()}>{busy ? 'Updating…' : 'Update now'}</button>
      </div>
      {conflicts.length > 0 && (
        <ul className="cx-history-conflicts" aria-label="Files in conflict">
          {conflicts.map((path) => <li key={path} className="cx-path">{path}</li>)}
        </ul>
      )}
      {result && <p className="cx-settings-line">{result}</p>}
      {error && <p className="inline-error">Not updated: {error}</p>}
    </section>
  );
}

function ChangeRow({ change, name }: { change: Change; name: (id: string) => string }) {
  const client = useClient();
  const [showing, setShowing] = useState(false);
  const [confirming, setConfirming] = useState(false);
  const [busy, setBusy] = useState(false);
  const [result, setResult] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const diff = useQuery<FileDiff[]>(showing ? `source.diff:commit:${change.commit}` : null, async (c) => diffFiles((await c.call<{ files: SourceFileDiff[] }>('source.diff', { commit: change.commit }))?.files ?? []));
  const revert = async () => {
    setBusy(true);
    setError(null);
    try {
      await client.call('source.revert', { commit: change.commit });
      setResult('Reverted.');
      setConfirming(false);
    } catch (cause) {
      setError(errorMessage(cause));
    } finally {
      setBusy(false);
    }
  };
  const revertable = change.origin !== 'shipped' && !change.reverted_by;
  return (
    <li data-commit={change.commit} data-origin={change.origin} data-reverted={change.reverted_by ? 'true' : 'false'}>
      <div className="cx-history-change">
        <div className={`t${change.reverted_by ? ' cx-history-reverted' : ''}`}>{change.summary || 'No summary.'}</div>
        <div className="r">
          <span className="ap faint">{clock(change.ms)}</span>
          <span className="ap origin">{ORIGIN[change.origin] ?? change.origin}</span>
          {change.plugins.length > 0 && <span className="ap faint">{change.plugins.map(name).join(', ')}</span>}
          {change.reverted_by && <span className="ap hot">reverted</span>}
          {change.shared && <span className="ap faint">shared</span>}
          <span className="grow" />
          <button className="detent" type="button" data-action="show-change" aria-expanded={showing} onClick={() => setShowing(!showing)}>{showing ? 'Hide the change' : 'Show the change'}</button>
          {revertable && <button className="detent warn" type="button" data-action="revert" disabled={busy} onClick={() => setConfirming(true)}>Revert</button>}
        </div>
        {confirming && (
          <div className="cx-history-confirm">
            <span className="t">Put this back as it was before the change?</span>
            <button className="detent warn" type="button" data-action="revert-confirm" disabled={busy} onClick={() => void revert()}>{busy ? 'Reverting…' : 'Revert'}</button>
            <button className="detent" type="button" disabled={busy} onClick={() => setConfirming(false)}>Cancel</button>
          </div>
        )}
        {result && <p className="cx-settings-line">{result}</p>}
        {error && <p className="inline-error">Not reverted: {error}</p>}
        {showing && (
          <div className="cx-history-diff">
            {diff.data ? (diff.data.length ? <DiffView files={diff.data} /> : <p className="hint">No file changes.</p>)
              : diff.error ? <p className="inline-error">The change could not be read: {diff.error}</p>
              : <p className="hint">Reading the change…</p>}
          </div>
        )}
      </div>
    </li>
  );
}

function HistoryPanel() {
  const name = usePluginNames();
  const history = useQuery<Change[]>('source.history', async (c) => (await c.call<{ changes: Change[] }>('source.history', {}))?.changes ?? [], { topics: ['source.changed'] });
  const changes = history.data ?? [];
  return (
    <div className="cx-settings-panel-body" data-build={__CORTEX_BUILD__} data-panel="history">
      <h3 className="cx-settings-title">History</h3>
      <p className="cx-settings-line">Every change to CortexCode's live data (its skills, prompts and texts), newest first. Revert puts the change back as it was before.</p>
      <UpdateState />
      <h3 className="cx-settings-title">Changes</h3>
      {history.error && !history.data && <ServiceProblem service="source manager" state={history} className="inline-error" />}
      {history.data && changes.length === 0 && <p className="hint">No changes yet.</p>}
      <ul className="cx-history">
        {changes.map((change) => <ChangeRow key={change.commit} change={change} name={name} />)}
      </ul>
    </div>
  );
}

export default definePlugin({ slot: 'settings.history@1', component: HistoryPanel });
