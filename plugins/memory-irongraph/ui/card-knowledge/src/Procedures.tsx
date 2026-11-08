import { useState } from 'react';
import { errorMessage, useClient, useHost, useQuery } from '@cortex/sdk';

interface Episode { session_id: string; run_id: string; task_key: string; text: string; outcome: string }
interface Use { key: string; task_key: string; work: string; outcome: string; verdict: string; shown: boolean; session_id: string; run_id: string }
interface Revision {
  key: string; name: string; applicability: string; steps: string[]; state: string; reason?: string;
  created_ms: number; sources: Episode[]; uses: Use[];
}

export function Procedures({ projectId }: { projectId: string }) {
  const client = useClient();
  const host = useHost();
  const [busy, setBusy] = useState<string | null>(null);
  const [error, setError] = useState('');
  const [notes, setNotes] = useState<Record<string, string>>({});
  const items = useQuery<{ items: Revision[] }>(`memory.procedure.list:${projectId}`,
    (c) => c.call<{ items: Revision[] }>('memory.procedure.list', { project_id: projectId }), { topics: ['memory.knowledge.changed'] });
  const review = async (revision: Revision, action: string, useKey?: string) => {
    if (busy) return;
    setBusy(revision.key); setError('');
    try {
      await client.call('memory.procedure.review', { project_id: projectId, revision: revision.key, action, use_key: useKey, reason: notes[revision.key] ?? '' });
      await items.refresh();
      host.notify('Procedure review saved');
    } catch (cause) { setError(errorMessage(cause)); }
    finally { setBusy(null); }
  };
  const revisions = items.data?.items ?? [];
  return <details className="cx-kn-procedures">
    <summary>Learned procedures{revisions.length ? ` · ${revisions.length}` : ''}</summary>
    <p className="ap">The assistant can propose a short method from different repairs. You choose whether to try it on future work. Regular use needs your approval after you judge it helpful on two different new tasks. A completed task alone does not establish that the method helped.</p>
    {(error || items.error) && <p role="alert">{error || items.error}</p>}
    {!items.data && !items.error && <p className="ap faint">Loading procedure proposals…</p>}
    {items.data && revisions.length === 0 && <p className="ap faint">No proposals yet. They can appear after related repairs on different tasks have taught a reusable method.</p>}
    <ul className="cx-kn-list">
      {revisions.map((revision) => {
        const observations = revision.uses.filter((u) => u.shown && u.outcome !== 'running');
        const isOrigin = (u: Use) => revision.sources.some((e) => e.task_key === u.task_key || (e.session_id === u.session_id && e.run_id === u.run_id));
        const helpful = new Set(observations.filter((u) => u.outcome === 'done' && u.verdict === 'helped' && !isOrigin(u)).map((u) => u.task_key)).size;
        const canApprove = helpful >= 2 && !observations.some((u) => u.verdict === 'hurt');
        const live = revision.state === 'trial' || revision.state === 'active' || revision.state === 'candidate';
        return <li key={revision.key} className="cx-kn-item" data-live={String(live)}>
          <div className="cx-kn-head"><strong>{revision.name}</strong><span className="ap faint">{revision.state === 'candidate' ? 'Awaiting your review' : revision.state === 'trial' ? 'Trying on relevant tasks' : revision.state === 'active' ? 'Approved for regular use' : revision.state}</span></div>
          <p className="cx-kn-text">Use when: {revision.applicability}</p>
          <ol>{revision.steps.map((step, i) => <li key={i}>{step}</li>)}</ol>
          {revision.reason && <p className="ap faint">{revision.reason}</p>}
          <input className="cx-kn-review-note" aria-label={`Review note for ${revision.name}`} placeholder="Why did this help or fail? (optional)" maxLength={1000} value={notes[revision.key] ?? ''} onChange={(event) => setNotes((old) => ({ ...old, [revision.key]: event.target.value }))} />
          <div className="cx-kn-acts">
            {revision.state === 'candidate' && <button type="button" className="detent tiny" disabled={busy !== null} onClick={() => void review(revision, 'trial')}>Try on future tasks</button>}
            {revision.state === 'trial' && <button type="button" className="detent tiny" disabled={busy !== null || !canApprove} title={canApprove ? 'Approve this exact revision' : 'Review two successful uses on different new tasks as helpful first'} onClick={() => void review(revision, 'approve')}>Approve regular use</button>}
            {(revision.state === 'candidate' || revision.state === 'trial') && <button type="button" className="detent tiny" disabled={busy !== null} onClick={() => void review(revision, 'reject')}>Reject</button>}
            {revision.state === 'active' && <button type="button" className="detent tiny warn" disabled={busy !== null} onClick={() => void review(revision, 'withdraw')}>Withdraw</button>}
            {busy === revision.key && <span className="ap" role="status">Saving review…</span>}
          </div>
          <details><summary>Evidence and trial outcomes</summary>
            <p className="ap faint">{helpful} different new tasks reviewed as helpful. This describes your assessments, not a controlled measurement.</p>
            <ul>{revision.sources.map((source, i) => <li key={i}>{source.text}<span className="ap faint"> · {source.outcome}</span></li>)}</ul>
            {observations.length === 0 && <p className="ap faint">No completed uses to review yet.</p>}
            {observations.map((use) => <div key={use.key} className="cx-kn-observation">
              <p>{use.work} <span className="ap faint">· {use.outcome} · {use.verdict === 'unreviewed' ? 'awaiting review' : use.verdict}</span></p>
              {!isOrigin(use) && <div className="cx-kn-acts">
                <button type="button" className="detent tiny" disabled={busy !== null || use.outcome !== 'done'} onClick={() => void review(revision, 'helped', use.key)}>Helped</button>
                <button type="button" className="detent tiny" disabled={busy !== null} onClick={() => void review(revision, 'hurt', use.key)}>Hurt the task</button>
                <button type="button" className="detent tiny" disabled={busy !== null} onClick={() => void review(revision, 'unclear', use.key)}>Cannot tell</button>
              </div>}
            </div>)}
          </details>
        </li>;
      })}
    </ul>
  </details>;
}
