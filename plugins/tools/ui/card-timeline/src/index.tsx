/**
 * card.timeline@1 — the step log in the card: what every step of every session, and the user by
 * hand, changed since the last commit (files, lines added and removed), newest first. Undo to a
 * step, undo one step (with the later steps that touched the same files, when it asks), redo the
 * last undo. A step that stands undone is drawn struck through. No project: nothing.
 */

import { useState } from 'react';
import { clock, definePlugin, errorMessage, runtime, useClient, useCodeTarget, useHost, useQuery, useSelection, useSessions } from '@cortex/sdk';
import './timeline.css';

interface TimelineFile { path: string; before?: string; after?: string; added: number; removed: number; binary: boolean; bytes: number }
interface TimelineRecord {
  seq: number;
  ts_ms: number;
  actor: string;
  session_id?: string | null;
  label: string;
  detail?: string;
  files: TimelineFile[];
  undoes?: number[];
  undone: boolean;
}
interface Timeline { records: TimelineRecord[]; head_seq: number; head?: string | null; files: number; added: number; removed: number }
interface UndoResult { record?: TimelineRecord | null; blocked_by: number[]; restored: string[]; skipped: string[] }

const SHOWN = 12;

function TimelineSection() {
  const client = useClient();
  const host = useHost();
  const { project, projectId } = useSelection();
  const { target } = useCodeTarget();
  const sessions = useSessions(projectId);
  const root = target?.root ?? project?.root ?? null;
  const timeline = useQuery<Timeline>(root ? `tools.timeline:${root}` : null, (c) => c.call<Timeline>('tools.timeline', { workspace: root, limit: 60 }), { topics: ['tools.timeline', 'vcs.changed'] });
  const [open, setOpen] = useState(false);
  const [all, setAll] = useState(false);
  const [expanded, setExpanded] = useState<number | null>(null);
  const [busy, setBusy] = useState(false);
  const [blocked, setBlocked] = useState<{ seq: number; by: number[] } | null>(null);

  if (!root) return null;
  const data = timeline.data;
  if (timeline.error && !data) {
    return timeline.errorCode === 'unavailable' ? (
      <section className="cx-cs" data-section="timeline" data-build={__CORTEX_BUILD__}>
        <div className="cx-cs-l"><span className="ap">Timeline</span></div>
        <div className="cx-cs-b"><div className="cx-cs-row"><span className="ap live" title={timeline.error}>unavailable</span></div></div>
      </section>
    ) : null;
  }
  if (!data) return null;
  const records = data.records;
  const active = records.filter((record) => !record.undone);
  const standing = active.filter((record) => !(record.undoes && record.undoes.length));
  const canRedo = active.some((record) => record.undoes && record.undoes.length > 0);

  const who = (record: TimelineRecord): string => {
    if (record.actor === 'user') return 'You';
    if (record.actor === 'host') return 'Host';
    const session = record.session_id ? sessions.data?.find((entry) => entry.id === record.session_id) : undefined;
    if (session?.agent) return `@${session.agent}`;
    return 'Chat';
  };
  const refresh = () => {
    runtime().queries.invalidate(`tools.timeline:${root}`);
    runtime().queries.invalidate(`vcs.status:${root}`);
    runtime().queries.invalidate('code.diff:');
  };
  const run = async (work: () => Promise<string>) => {
    setBusy(true);
    setBlocked(null);
    try {
      host.notify(await work());
    } catch (cause) {
      host.notify(errorMessage(cause));
    } finally {
      setBusy(false);
      refresh();
    }
  };
  const describe = (result: UndoResult): string => {
    const put = result.restored.length;
    const skipped = result.skipped.length ? ` (${result.skipped.length} skipped: ${result.skipped[0]})` : '';
    return `Put back ${put} file${put === 1 ? '' : 's'}${skipped}`;
  };
  const undoOne = (record: TimelineRecord, cascade: boolean) => void run(async () => {
    const result = await client.call<UndoResult>('tools.undo', { workspace: root, seqs: [record.seq], cascade, actor: 'user' });
    if (result.blocked_by.length) {
      setBlocked({ seq: record.seq, by: result.blocked_by });
      return `Step ${record.seq}: ${result.blocked_by.length} later step${result.blocked_by.length === 1 ? '' : 's'} touched the same files`;
    }
    return describe(result);
  });
  const undoTo = (record: TimelineRecord) => void run(async () => describe(await client.call<UndoResult>('tools.undo', { workspace: root, to_seq: record.seq - 1, cascade: true, actor: 'user' })));
  const undoLast = () => { const first = standing[0]; if (first) undoOne(first, true); };
  const redo = () => void run(async () => describe(await client.call<UndoResult>('tools.redo', { workspace: root, actor: 'user' })));
  const shown = all ? records : records.slice(0, SHOWN);
  const summary = standing.length === 0 ? 'nothing since the last commit' : `${data.files} file${data.files === 1 ? '' : 's'} · ${standing.length} step${standing.length === 1 ? '' : 's'}`;

  return (
    <section className="cx-cs" data-section="timeline" data-build={__CORTEX_BUILD__}>
      <div className="cx-cs-l"><span className="ap">Timeline</span></div>
      <div className="cx-cs-b">
        <div className="cx-cs-row">
          <button type="button" className="cx-cs-row" onClick={() => setOpen(!open)} aria-expanded={open} data-action="toggle" title="Every step since the last commit, newest first">
            <span className="t">{summary}</span>
            <span className="grow" />
            {standing.length > 0 && (
              <>
                <span className="ap cx-plus" data-testid="timeline-added">+{data.added}</span>
                <span className="ap cx-minus" data-testid="timeline-removed">−{data.removed}</span>
              </>
            )}
          </button>
          <button type="button" className="detent tiny" data-action="undo-last" disabled={busy || standing.length === 0} onClick={undoLast} title="Undo the newest step (and later steps that touched its files)">Undo</button>
          <button type="button" className="detent tiny" data-action="redo" disabled={busy || !canRedo} onClick={redo} title="Take the last undo back">Redo</button>
        </div>
        {open && (
          <>
            <ol className="cx-tl-list">
              {shown.map((record) => {
                const files = record.files.length;
                const added = record.files.reduce((sum, file) => sum + file.added, 0);
                const removed = record.files.reduce((sum, file) => sum + file.removed, 0);
                const isUndo = Boolean(record.undoes && record.undoes.length);
                const label = isUndo ? `${record.label} of ${record.undoes!.join(', ')}` : record.label === 'hand' ? 'by hand' : record.label;
                const expandedHere = expanded === record.seq;
                return (
                  <li key={record.seq} data-seq={record.seq} data-undone={record.undone}>
                    <button type="button" className="cx-cs-row" onClick={() => setExpanded(expandedHere ? null : record.seq)} title={`${record.detail || label} · ${new Date(record.ts_ms).toLocaleString()}`}>
                      <span className="cx-mono">{record.seq}</span>
                      <span className="t">{who(record)} · {label}{record.detail ? ` · ${record.detail}` : ''}</span>
                      <span className="ap faint">{files} file{files === 1 ? '' : 's'}</span>
                      {!isUndo && <span className="ap faint">+{added} −{removed}</span>}
                      <span className="ap faint">{clock(record.ts_ms)}</span>
                    </button>
                    {!record.undone && !isUndo && (
                      <>
                        <button type="button" className="detent tiny" data-action="undo-step" disabled={busy} onClick={() => undoOne(record, false)} title="Put back what this step changed">Undo</button>
                        <button type="button" className="detent tiny" data-action="undo-to" disabled={busy} onClick={() => undoTo(record)} title="Undo this step and everything after it">To here</button>
                      </>
                    )}
                    {expandedHere && (
                      <ul className="cx-tl-files">
                        {record.files.map((file) => (
                          <li key={file.path}>
                            <span className="cx-mono">{file.path}</span>
                            <span className="ap faint">{file.before === undefined ? 'new' : file.after === undefined ? 'deleted' : ''}</span>
                            {!file.binary && <span className="ap faint">+{file.added} −{file.removed}</span>}
                          </li>
                        ))}
                      </ul>
                    )}
                  </li>
                );
              })}
            </ol>
            {blocked && (
              <div className="cx-tl-acts">
                <span className="cx-tl-note">Step {blocked.seq}: later step{blocked.by.length === 1 ? '' : 's'} {blocked.by.join(', ')} touched the same files.</span>
                <span className="grow" />
                <button type="button" className="detent tiny warn" data-action="undo-cascade" disabled={busy} onClick={() => { const record = records.find((entry) => entry.seq === blocked.seq); if (record) undoOne(record, true); }}>Undo them too</button>
                <button type="button" className="detent tiny" onClick={() => setBlocked(null)}>Keep</button>
              </div>
            )}
            {records.length > SHOWN && (
              <div className="cx-tl-acts">
                <button type="button" className="detent tiny" data-action="more" onClick={() => setAll(!all)}>{all ? 'Fewer' : `${records.length - SHOWN} more`}</button>
              </div>
            )}
          </>
        )}
      </div>
    </section>
  );
}

export default definePlugin({ slot: 'card.timeline@1', component: TimelineSection });
