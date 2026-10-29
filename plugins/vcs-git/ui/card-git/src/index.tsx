/**
 * card.git@1 — git in the card, one line: the branch (or a detached HEAD), ahead and behind its
 * upstream, the changes (+A −R and files, opening the code view's review), and a menu of actions:
 * Commit, Push (it leaves the machine, so in ask mode it asks first), New branch, Checkpoint,
 * Restore, Check pull request (that needs the network). The chosen action opens under the line.
 * No project or not a repository: nothing.
 */

import { useState } from 'react';
import {
  Menu,
  definePlugin,
  errorMessage,
  runtime,
  setShared,
  showPane,
  useClient,
  useCodeTarget,
  useHost,
  useQuery,
  useSelection,
  useShared,
  type PrStatus,
  type VcsStatus,
} from '@cortex/sdk';
import './git.css';

type Panel = null | 'commit' | 'branch' | 'restore' | 'push';
interface Checkpoint { id: string; label: string; ts_ms: number }

function GitSection() {
  const client = useClient();
  const host = useHost();
  const { project } = useSelection();
  const { target } = useCodeTarget();
  const [mode] = useShared<'ask' | 'auto'>('agents.defaultMode', 'auto');
  const root = target?.root ?? project?.root ?? null;
  const status = useQuery<VcsStatus>(root ? `vcs.status:${root}` : null, (c) => c.call<VcsStatus>('vcs.status', { root }), { topics: ['vcs.changed', 'tools.files_changed'] });
  const checkpoints = useQuery<Checkpoint[]>(root ? `vcs.checkpoints:${root}` : null, async (c) => (await c.call<{ checkpoints: Checkpoint[] }>('vcs.checkpoints', { root }))?.checkpoints ?? [], { topics: ['vcs.changed'] });
  const [panel, setPanel] = useState<Panel>(null);
  const [text, setText] = useState('');
  const [busy, setBusy] = useState(false);
  const [pr, setPr] = useState<PrStatus | null>(null);
  const [picked, setPicked] = useState<string | null>(null);

  const run = async (work: () => Promise<string>) => {
    setBusy(true);
    try {
      host.notify(await work());
      setPanel(null);
      setText('');
      runtime().queries.invalidate(`vcs.status:${root}`);
      runtime().queries.invalidate('code.diff:');
    } catch (cause) {
      host.notify(errorMessage(cause));
    } finally {
      setBusy(false);
    }
  };

  const data = status.data;
  if (!root) return null;
  if (status.error && !data) {
    return status.errorCode === 'unavailable' ? (
      <section className="cx-cs" data-section="git" data-build={__CORTEX_BUILD__}>
        <div className="cx-cs-l"><span className="ap">Git</span></div>
        <div className="cx-cs-b"><div className="cx-cs-row"><span className="ap live" title={status.error}>unavailable</span></div></div>
      </section>
    ) : null;
  }
  if (!data?.is_repo) return null;
  const openReview = () => {
    setShared('code.scope', 'uncommitted');
    setShared('code.options', { ...(runtime().state.get<Record<string, unknown>>('workbench', 'code.options') ?? {}), list: true });
    showPane('code');
  };
  const push = () => run(async () => {
    const result = await client.call<{ ok: boolean; available: boolean; detail: string; remote: string; branch: string }>('vcs.push', { root });
    return result.ok ? `Pushed ${result.branch} to ${result.remote}` : result.available ? `Push refused: ${result.detail}` : `The remote is not reachable: ${result.detail}`;
  });
  const checkPr = () => {
    setBusy(true);
    client.call<PrStatus>('vcs.pr.status', { root }).then(setPr).catch((cause) => host.notify(errorMessage(cause))).finally(() => setBusy(false));
  };
  const toggle = (next: Panel) => { setText(''); setPanel(panel === next ? null : next); };
  const changed = data.files.length;
  const staged = data.files.filter((f) => f.staged).length;
  return (
    <section className="cx-cs" data-section="git" data-build={__CORTEX_BUILD__}>
      <div className="cx-cs-l"><span className="ap">Git</span></div>
      <div className="cx-cs-b">
        <div className="cx-cs-row">
          <button type="button" className="cx-cs-row" onClick={openReview} data-action="changes"
            title={`Review the changes in the code view${changed ? ` · ${changed} file${changed === 1 ? '' : 's'}, ${staged} staged` : ''}${data.upstream ? ` · against ${data.upstream}` : ' · no upstream'}`}>
            <span className="cx-mono cx-git-branch" data-testid="card-branch">{data.detached ? (data.head ?? '').slice(0, 10) : data.branch}</span>
            {data.detached && <span className="ap hot">detached</span>}
            {target?.agent && <span className="ap origin">@{target.agent}</span>}
            {data.upstream && (data.ahead > 0 || data.behind > 0) && <span className="ap faint">↑{data.ahead} ↓{data.behind}</span>}
            <span className="grow" />
            {changed > 0 ? (
              <>
                <span className="ap cx-plus" data-testid="card-added">+{data.added_lines}</span>
                <span className="ap cx-minus" data-testid="card-removed">−{data.removed_lines}</span>
                <span className="ap faint">{changed} file{changed === 1 ? '' : 's'}</span>
              </>
            ) : <span className="ap faint">clean</span>}
          </button>
          <Menu
            variant="detent"
            label="Actions"
            title="Git actions"
            className="tiny cx-git-menu"
            disabled={busy}
            items={[
              { key: 'commit', label: 'Commit…', note: changed ? `${changed} changed, ${staged} staged` : 'Nothing to commit', disabled: changed === 0, onSelect: () => toggle('commit') },
              { key: 'push', label: 'Push', note: data.upstream ? `to ${data.upstream}` : 'to its remote', onSelect: () => (mode === 'ask' ? toggle('push') : void push()) },
              { key: 'branch', label: 'New branch…', note: `from ${data.branch}`, onSelect: () => toggle('branch') },
              { key: 'checkpoint', label: 'Checkpoint', note: 'Keep the tree as it is; history untouched', onSelect: () => void run(async () => {
                const checkpoint = await client.call<Checkpoint>('vcs.checkpoint', { root, label: `From the card · ${new Date().toLocaleTimeString()}` });
                return `Checkpoint ${checkpoint.id.slice(0, 8)} kept (your history is untouched)`;
              }) },
              { key: 'restore', label: 'Restore…', note: `${(checkpoints.data ?? []).length} checkpoint${(checkpoints.data ?? []).length === 1 ? '' : 's'}`, disabled: (checkpoints.data ?? []).length === 0, onSelect: () => toggle('restore') },
              { key: 'pr', label: 'Check pull request', note: 'Asks GitHub (uses the network)', onSelect: checkPr },
            ]}
          />
        </div>
        {panel === 'commit' && (
          <div className="cx-git-panel">
            <textarea className="write one" value={text} placeholder="What this commit does" aria-label="Commit message" onChange={(event) => setText(event.target.value)} />
            <div className="cx-git-acts">
              <button className="detent tiny" type="button" disabled={!text.trim() || busy || staged === 0} onClick={() => void run(async () => `Committed ${(await client.call<{ sha: string }>('vcs.commit', { root, message: text.trim(), all: false })).sha.slice(0, 8)}`)}>Commit staged</button>
              <button className="detent tiny send" type="button" disabled={!text.trim() || busy} onClick={() => void run(async () => `Committed ${(await client.call<{ sha: string }>('vcs.commit', { root, message: text.trim(), all: true })).sha.slice(0, 8)}`)}>Commit all</button>
              <span className="grow" />
              <button className="detent tiny" type="button" onClick={() => setPanel(null)}>Cancel</button>
            </div>
          </div>
        )}
        {panel === 'push' && (
          <div className="cx-git-panel cx-git-ask">
            <span className="ap hot">Leaves the machine</span>
            <span className="t">Push {data.branch} to {data.upstream ?? 'its remote'}?</span>
            <span className="grow" />
            <button className="detent tiny warn" type="button" data-action="push-confirm" disabled={busy} onClick={() => void push()}>Push</button>
            <button className="detent tiny" type="button" onClick={() => setPanel(null)}>Not now</button>
          </div>
        )}
        {panel === 'branch' && (
          <div className="cx-git-panel cx-git-row">
            <label className="field cx-git-field"><input value={text} placeholder="new-branch-name" aria-label="Branch name" onChange={(event) => setText(event.target.value)} /></label>
            <button className="detent tiny send" type="button" disabled={!/^[\w./-]+$/.test(text.trim()) || busy} onClick={() => void run(async () => {
              await client.call('vcs.checkout', { root, name: text.trim(), from: data.branch });
              return `On ${text.trim()} now`;
            })}>Create</button>
          </div>
        )}
        {panel === 'restore' && (
          <div className="cx-git-panel">
            <ul className="cx-git-checkpoints">
              {[...(checkpoints.data ?? [])].sort((a, b) => b.ts_ms - a.ts_ms).map((checkpoint) => (
                <li key={checkpoint.id}>
                  <button type="button" className={`cx-cs-row${picked === checkpoint.id ? ' on' : ''}`} onClick={() => setPicked(checkpoint.id)} title={new Date(checkpoint.ts_ms).toLocaleString()}>
                    <span className="cx-mono">{checkpoint.id.slice(0, 8)}</span>
                    <span className="t">{checkpoint.label}</span>
                  </button>
                </li>
              ))}
            </ul>
            <div className="cx-git-acts">
              <button className="detent tiny warn" type="button" disabled={!picked || busy} onClick={() => void run(async () => {
                const result = await client.call<{ backup: Checkpoint }>('vcs.restore', { root, id: picked });
                return `Restored; the tree as it was is kept as checkpoint ${result.backup.id.slice(0, 8)}`;
              })}>Restore the whole tree</button>
              <span className="grow" />
              <button className="detent tiny" type="button" onClick={() => setPanel(null)}>Cancel</button>
            </div>
          </div>
        )}
        {pr && (
          <div className="cx-cs-row cx-git-pr">
            {pr.available && pr.number ? (
              <a className="ap" href={pr.url ?? undefined} onClick={(event) => { event.preventDefault(); if (pr.url) { showPane('browser'); void client.call('browser.open', { url: pr.url }); } }}>
                PR #{pr.number} · {pr.state ?? 'open'}{pr.ci && pr.ci !== 'none' ? ` · CI ${pr.ci}` : ''}
              </a>
            ) : <span className="ap faint" title={pr.detail}>{pr.available ? 'No pull request for this branch' : 'GitHub is not reachable'}</span>}
          </div>
        )}
      </div>
    </section>
  );
}

export default definePlugin({ slot: 'card.git@1', component: GitSection });
