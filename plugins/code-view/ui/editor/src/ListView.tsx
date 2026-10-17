import { useState } from 'react';
import {
  DiffView,
  errorMessage,
  openInCode,
  runtime,
  useClient,
  useCodeDiff,
  useHost,
  useQuery,
  type Diff,
  type FileDiff,
  type VcsStatus,
  ServiceProblem,
} from '@cortex/sdk';

/**
 * Every change of the scope as one list (the review): per hunk Stage and Revert (indices of the
 * `unstaged` diff), Unstage in the staged scope, and a commit at the foot.
 */
export function ListView({ root }: { root: string }) {
  const client = useClient();
  const host = useHost();
  const diff = useCodeDiff();
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
  return (
    <div className="cx-list" data-scope={scope} data-files={files.length}>
      <div className="cx-list-scroll">
        {diff.error && !diff.data && <ServiceProblem service="vcs" state={diff} className="inline-error" />}
        {diff.data && files.length === 0 && <p className="empty-line">Nothing in this scope.</p>}
        {files.length > 0 && (
          <DiffView
            files={files}
            onOpenFile={(path) => openInCode({ root, path })}
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
