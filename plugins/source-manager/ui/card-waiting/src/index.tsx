/**
 * card.waiting@1 — code changes to CortexCode waiting for an answer (`source.proposals`, pending),
 * one line each: the plugins it changes and its plain-English summary. A change made in a
 * conversation opens its card there; one made outside a conversation (night work) opens here,
 * under its line: the summary, why, the risk, Approve / Decline and Show the change. Nothing
 * waiting: nothing.
 */

import { useState } from 'react';
import {
  DiffView,
  definePlugin,
  errorMessage,
  openTarget,
  parseUnifiedDiff,
  useClient,
  useQuery,
  type FileDiff,
  type SourceFileDiff,
  type SourceProposal,
} from '@cortex/sdk';
import './waiting.css';

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

/** A change made outside a conversation, answered here. */
function Change({ proposal }: { proposal: SourceProposal }) {
  const client = useClient();
  const [busy, setBusy] = useState<'approve' | 'decline' | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [showing, setShowing] = useState(false);
  const diff = useQuery<FileDiff[]>(showing ? `source.diff:proposal:${proposal.id}` : null, async (c) => diffFiles((await c.call<{ files: SourceFileDiff[] }>('source.diff', { proposal: proposal.id }))?.files ?? []));
  const answer = async (approve: boolean) => {
    setBusy(approve ? 'approve' : 'decline');
    setError(null);
    try {
      await client.call('source.decide', { id: proposal.id, approve });
    } catch (cause) {
      setError(errorMessage(cause));
    } finally {
      setBusy(null);
    }
  };
  return (
    <div className="cx-waiting-open" data-proposal={proposal.id}>
      <p>{proposal.summary}</p>
      {proposal.why && <p className="cx-waiting-line">Why: {proposal.why}</p>}
      {proposal.risk && <p className="cx-waiting-line">Risk: {proposal.risk}</p>}
      <div className="cx-waiting-acts">
        <button className="detent tiny" type="button" data-action="approve" disabled={busy !== null} onClick={() => void answer(true)}>{busy === 'approve' ? 'Approving…' : 'Approve'}</button>
        <button className="detent tiny warn" type="button" data-action="decline" disabled={busy !== null} onClick={() => void answer(false)}>{busy === 'decline' ? 'Declining…' : 'Decline'}</button>
        <button className="detent tiny" type="button" data-action="show-change" aria-expanded={showing} onClick={() => setShowing(!showing)}>{showing ? 'Hide the change' : 'Show the change'}</button>
      </div>
      {error && <p className="inline-error">Not answered: {error}</p>}
      {showing && (
        <div className="cx-waiting-diff">
          {diff.data ? (diff.data.length ? <DiffView files={diff.data} /> : <p className="hint">No file changes.</p>)
            : diff.error ? <p className="inline-error">The change could not be read: {diff.error}</p>
            : <p className="hint">Reading the change…</p>}
        </div>
      )}
    </div>
  );
}

function WaitingSection() {
  const proposals = useQuery<SourceProposal[]>('source.proposals', async (c) => (await c.call<{ proposals: SourceProposal[] }>('source.proposals', {}))?.proposals ?? [], { topics: ['source.changed'] });
  const name = usePluginNames();
  const [open, setOpen] = useState<string | null>(null);
  const waiting = (proposals.data ?? []).filter((proposal) => proposal.state === 'pending');
  if (waiting.length === 0) return null;
  const show = (proposal: SourceProposal) => {
    if (proposal.session_id && typeof proposal.entry_seq === 'number') {
      void openTarget({ open: 'approval', kind: 'approval', session_id: proposal.session_id, seq: proposal.entry_seq });
    } else {
      setOpen(open === proposal.id ? null : proposal.id);
    }
  };
  return (
    <section className="cx-cs" data-section="waiting" data-build={__CORTEX_BUILD__}>
      <div className="cx-cs-l"><span className="ap live">Waiting</span></div>
      <div className="cx-cs-b">
        {waiting.map((proposal) => {
          const names = proposal.plugins.map(name).join(', ') || 'CortexCode';
          return (
            <div key={proposal.id}>
              <button type="button" className={`cx-cs-row${open === proposal.id ? ' on' : ''}`} data-proposal={proposal.id} onClick={() => show(proposal)}
                title={`Change to ${names}\n${proposal.summary}`}>
                <span className="cx-pip cx-pip-waiting" />
                <span className="t"><span className="cx-waiting-who">{names}</span><span className="cx-waiting-sum">{proposal.summary}</span></span>
              </button>
              {open === proposal.id && <Change proposal={proposal} />}
            </div>
          );
        })}
      </div>
    </section>
  );
}

export default definePlugin({ slot: 'card.waiting@1', component: WaitingSection });
