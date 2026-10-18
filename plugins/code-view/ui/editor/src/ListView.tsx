import { useEffect, useState } from 'react';
import {
  ago,
  errorMessage,
  firstChangedLine,
  openInCode,
  runtime,
  useClient,
  useCodeDiff,
  useHost,
  useQuery,
  useShared,
  type Diff,
  type FileDiff,
  type VcsStatus,
  ServiceProblem,
} from '@cortex/sdk';
import { LazyDiff } from './LazyDiff';

interface Checkpoint { id: string; label: string; ts_ms: number }

/**
 * Every change of the scope as one list (the review): per hunk Stage and Revert (indices of the
 * `unstaged` diff), Unstage in the staged scope; a file's name opens it in the File view at its
 * change; the tree scrolls the list to a file (`code.diffFocus`). The tree's checkpoints and a
 * commit sit at the foot.
 */
export function ListView({ root }: { root: string }) {
  const client = useClient();
  const host = useHost();
  const diff = useCodeDiff();
  const [scroller, setScroller] = useState<HTMLDivElement | null>(null);
  const [focus] = useShared<{ path: string; at: number } | null>('code.diffFocus', null);
  const checkpoints = useQuery<Checkpoint[]>(`vcs.checkpoints:${root}`, async (c) => (await c.call<{ checkpoints: Checkpoint[] }>('vcs.checkpoints', { root }))?.checkpoints ?? [], { topics: ['vcs.changed'] });
  const scope = diff.scope;
  const status = useQuery<VcsStatus>(`vcs.status:${root}`, (c) => c.call<VcsStatus>('vcs.status', { root }), { topics: ['vcs.changed', 'tools.files_changed'] });
  // Hunk actions name hunks of the unstaged diff; in the uncommitted scope show exactly those.
  const unstaged = useQuery<Diff>(scope === 'uncommitted' || scope === 'last_turn' ? `code.diff:${root}:unstaged-list` : null, (c) => c.call<Diff>('vcs.diff', { root, scope: 'unstaged' }), { topics: ['vcs.changed', 'tools.files_changed'] });
  const [busy, setBusy] = useState<string | null>(null);
  const [confirm, setConfirm] = useState<string | null>(null);
  const [message, setMessage] = useState('');

  const act = async (key: string, method: string, params: Record<string, unknown>, done: string) => {
    setBusy(key);
    setConfirm(null);
    try {
      await client.call(method, { root, ...params });
      runtime().queries.invalidate('code.diff:');
      runtime().queries.invalidate(`vcs.status:${root}`);
      host.notify(done);
    } catch (cause) {
      host.notify(errorMessage(cause));
    } finally {
      setBusy(null);
    }
  };
  const commit = async (all: boolean) => {
    if (!message.trim()) return;
    setBusy('commit');
    try {
      const result = await client.call<{ sha: string }>('vcs.commit', { root, message: message.trim(), all });
      setMessage('');
      runtime().queries.invalidate('code.diff:');
      runtime().queries.invalidate(`vcs.status:${root}`);
      host.notify(`Committed ${result.sha.slice(0, 8)}`);
    } catch (cause) {
      host.notify(errorMessage(cause));
    } finally {
      setBusy(null);
    }
  };

  // In the uncommitted scopes, hunks come from the unstaged diff (so their indices are the ones
  // Stage and Revert take); files with nothing unstaged (new, or fully staged) are listed as they are.
  const scoped = diff.data?.files ?? [];
  const unstagedFiles = (unstaged.data?.files ?? []).filter((file) => scoped.some((s) => s.path === file.path));
  const rest = scoped.filter((file) => !unstagedFiles.some((u) => u.path === file.path)).map((file) => ({ ...file, status: file.status === 'modified' ? 'staged' : file.status }));
  const files: FileDiff[] = scope === 'uncommitted' || scope === 'last_turn' ? [...unstagedFiles, ...rest] : scoped;
  const staged = (status.data?.files ?? []).filter((file) => file.staged).length;
  const editable = scope === 'uncommitted' || scope === 'last_turn';

  // The tree picked a file: bring its part of the list to the top (once it is in the list).
  const listed = files.length;
  useEffect(() => {
    if (!focus || !scroller) return;
    const section = scroller.querySelector<HTMLElement>(`[data-path="${CSS.escape(focus.path)}"]`);
    if (!section) return;
    scroller.scrollTo({ top: section.getBoundingClientRect().top - scroller.getBoundingClientRect().top + scroller.scrollTop - 8 });
  }, [focus, scroller, listed]);
  return (
    <div className="cx-list" data-scope={scope} data-files={files.length}>
      <div className="cx-list-scroll" ref={setScroller}>
        {diff.error && !diff.data && <ServiceProblem service="vcs" state={diff} className="inline-error" />}
        {diff.data && files.length === 0 && <p className="empty-line">Nothing in this scope.</p>}
        {files.length > 0 && (
          <LazyDiff
            files={files}
            scroller={scroller}
            onOpenFile={(file) => openInCode({ root, path: file.path, line: firstChangedLine(file.hunks) })}
            hunkActions={(file, _hunk, index) => {
              if (scope === 'staged') {
                return <button className="detent" type="button" disabled={busy !== null} data-action="unstage-hunk" onClick={() => void act(`u:${file.path}:${index}`, 'vcs.unstage', { paths: [file.path], hunk: index }, `Unstaged a change in ${file.path}`)}>Unstage</button>;
              }
              if (!editable || !unstagedFiles.some((u) => u.path === file.path)) return null;
              const id = `${file.path}:${index}`;
              return (
                <>
                  <button className="detent" type="button" disabled={busy !== null} data-action="stage-hunk" onClick={() => void act(`s:${id}`, 'vcs.stage', { paths: [file.path], hunk: index }, `Staged a change in ${file.path}`)}>Stage</button>
                  {confirm === id ? (
                    <>
                      <button className="detent warn" type="button" disabled={busy !== null} data-action="confirm-revert" onClick={() => void act(`r:${id}`, 'vcs.revert', { paths: [file.path], hunk: index }, `Reverted a change in ${file.path}`)}>Discard it</button>
                      <button className="detent" type="button" onClick={() => setConfirm(null)}>Keep</button>
                    </>
                  ) : (
                    <button className="detent" type="button" disabled={busy !== null} data-action="revert" onClick={() => setConfirm(id)}>Revert</button>
                  )}
                </>
              );
            }}
            fileActions={editable ? (file) => (
              <button className="detent" type="button" disabled={busy !== null} onClick={() => void act(`sf:${file.path}`, 'vcs.stage', { paths: [file.path] }, `Staged ${file.path}`)}>Stage file</button>
            ) : undefined}
          />
        )}
      </div>
      <Checkpoints root={root} list={checkpoints.data ?? []} />
      <div className="composer flush cx-commit">
        <div className="composer-h">
          <span className="ap">Commit</span>
          <span className="ap faint">{staged} staged</span>
          <span className="grow" />
          <span className="ap faint">on {status.data?.branch ?? '…'}</span>
        </div>
        <textarea className="write" rows={2} value={message} placeholder="What this commit does" aria-label="Commit message" onChange={(event) => setMessage(event.target.value)} />
        <div className="tools">
          <div className="grow" />
          <button className="detent" type="button" disabled={!message.trim() || busy !== null || staged === 0} onClick={() => void commit(false)}>Commit staged</button>
          <button className="detent send" type="button" disabled={!message.trim() || busy !== null} onClick={() => void commit(true)}>{busy === 'commit' ? 'Committing…' : 'Commit all'}</button>
        </div>
      </div>
    </div>
  );
}

/** The tree's checkpoints as ticks on a hairline; pick one to restore the whole tree to it. */
function Checkpoints({ root, list }: { root: string; list: Checkpoint[] }) {
  const client = useClient();
  const host = useHost();
  const [picked, setPicked] = useState<string | null>(null);
  const [armed, setArmed] = useState(false);
  if (list.length === 0) return null;
  const sorted = [...list].sort((a, b) => a.ts_ms - b.ts_ms);
  const first = sorted[0]!.ts_ms;
  const span = Math.max(1, sorted[sorted.length - 1]!.ts_ms - first);
  const chosen = sorted.find((checkpoint) => checkpoint.id === picked) ?? null;
  const restore = async () => {
    if (!chosen) return;
    try {
      const result = await client.call<{ backup: Checkpoint }>('vcs.restore', { root, id: chosen.id });
      host.notify(`Restored “${chosen.label}”; the tree as it was is kept as checkpoint ${result.backup.id.slice(0, 8)}`);
      setArmed(false);
      runtime().queries.invalidate('code.');
    } catch (cause) {
      host.notify(errorMessage(cause));
    }
  };
  return (
    <div className="cx-checkpoints" aria-label="Checkpoints">
      <span className="ap faint">Checkpoints</span>
      <div className="cx-scrub">
        {sorted.map((checkpoint) => (
          <button key={checkpoint.id} type="button" className={`cx-tick${checkpoint.id === picked ? ' on' : ''}`}
            style={{ left: `${((checkpoint.ts_ms - first) / span) * 100}%` }}
            title={`${checkpoint.label} · ${new Date(checkpoint.ts_ms).toLocaleString()}`} aria-label={checkpoint.label}
            onClick={() => { setPicked(checkpoint.id); setArmed(false); }} />
        ))}
      </div>
      {chosen ? (
        <>
          <span className="ap">{chosen.label}</span>
          <span className="ap faint">{ago(chosen.ts_ms)}</span>
          <button className={`detent${armed ? ' warn' : ''}`} type="button" onClick={() => (armed ? void restore() : setArmed(true))}>
            {armed ? 'Restore the whole tree' : 'Restore…'}
          </button>
        </>
      ) : (
        <span className="ap faint">{sorted.length} kept</span>
      )}
    </div>
  );
}
