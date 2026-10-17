/**
 * code.toolbar@1 — the code view's bar: whose tree (yours, or an agent's workspace), the scope
 * (uncommitted, staged, branch against a base, the last turn) with its total, the compare base,
 * and the view's actions as drawn-mark buttons with titles: more, the list of all changes,
 * refresh, collapse the tree, stage or unstage everything, change marks on or off, split or inline.
 */

import { useState } from 'react';
import {
  Menu,
  definePlugin,
  errorMessage,
  runtime,
  setShared,
  useClient,
  useCodeBase,
  useCodeDiff,
  useCodeOptions,
  useCodeScope,
  useCodeTarget,
  useHost,
  useQuery,
  useSelection,
  type AgentRecord,
  type BranchesResult,
  type CodeScope,
  type VcsStatus,
} from '@cortex/sdk';
import { MarkButton } from './marks';
import './toolbar.css';

export const SCOPES: Array<{ id: CodeScope; label: string; note: string }> = [
  { id: 'uncommitted', label: 'Uncommitted', note: 'Everything not yet committed, against HEAD' },
  { id: 'staged', label: 'Staged', note: 'What the next commit takes' },
  { id: 'branch', label: 'Branch vs base', note: 'This branch against the base you compare with' },
  { id: 'last_turn', label: 'Last turn', note: 'Files the last turn of this conversation changed' },
];

function Toolbar() {
  const client = useClient();
  const host = useHost();
  const { project, projectId, sessionId } = useSelection();
  const { target, setTarget } = useCodeTarget();
  const [scope, setScope] = useCodeScope();
  const [base, setBase] = useCodeBase();
  const [options, setOptions] = useCodeOptions();
  const diff = useCodeDiff();
  const [busy, setBusy] = useState(false);
  const root = target?.root ?? null;
  const status = useQuery<VcsStatus>(root ? `vcs.status:${root}` : null, (c) => c.call<VcsStatus>('vcs.status', { root }), { topics: ['vcs.changed', 'tools.files_changed'] });
  const branches = useQuery<BranchesResult>(root ? `vcs.branches:${root}` : null, (c) => c.call<BranchesResult>('vcs.branches', { root }), { topics: ['vcs.changed'] });
  const agents = useQuery<AgentRecord[]>(projectId ? `agents.list:${projectId}` : null, async (c) => (await c.call<{ agents: AgentRecord[] }>('agents.list', { project_id: projectId }))?.agents ?? [], { topics: ['agents.changed'] });

  if (!project || !target) return <div className="bar cx-code-bar"><span className="ap faint">Open a project to see its code.</span></div>;

  const workspaces = (agents.data ?? []).filter((agent) => agent.workspace);
  const branch = status.data?.branch ?? '…';
  const compareTo = base ?? status.data?.upstream ?? 'base';
  const changed = diff.data?.files ?? [];
  const staged = (status.data?.files ?? []).filter((file) => file.staged);

  const stageAll = async (stage: boolean) => {
    setBusy(true);
    try {
      const paths = stage ? changed.map((file) => file.path) : staged.map((file) => file.path);
      if (paths.length) await client.call(stage ? 'vcs.stage' : 'vcs.unstage', { root, paths });
      runtime().queries.invalidate('code.diff:');
      runtime().queries.invalidate(`vcs.status:${root}`);
    } catch (cause) {
      host.notify(errorMessage(cause));
    } finally {
      setBusy(false);
    }
  };

  const branchItems = [
    ...(status.data?.upstream ? [{ key: `up:${status.data.upstream}`, label: status.data.upstream, note: 'upstream' }] : []),
    ...(branches.data?.branches ?? []).filter((name) => name !== branch).map((name) => ({ key: name, label: name, note: name.startsWith('cortex/') ? 'agent branch' : 'local branch' })),
  ].map((item) => ({
    ...item,
    selected: item.label === base,
    onSelect: () => {
      setBase(item.label);
      setScope('branch');
    },
  }));

  return (
    <div className="bar cx-code-bar" data-build={__CORTEX_BUILD__} data-scope={scope} data-root={root}>
      <Menu
        label="Tree"
        value={target.agent ? `@${target.agent}` : 'Yours'}
        title={target.root}
        items={[
          { key: 'mine', label: `Yours · ${project.name}`, note: project.root, selected: !target.agent, onSelect: () => setTarget({ root: project.root, label: project.name, agent: null }) },
          ...workspaces.map((agent) => ({
            key: agent.name,
            label: `@${agent.name}`,
            note: `${agent.status} · ${agent.workspace}`,
            selected: target.agent === agent.name,
            onSelect: () => setTarget({ root: agent.workspace!, label: `@${agent.name}`, agent: agent.name }),
          })),
        ]}
      />
      <Menu
        label="Scope"
        value={SCOPES.find((entry) => entry.id === scope)?.label ?? scope}
        items={SCOPES.map((entry) => ({
          key: entry.id,
          label: entry.label,
          note: entry.note,
          selected: entry.id === scope,
          disabled: entry.id === 'last_turn' && !sessionId,
          onSelect: () => setScope(entry.id),
        }))}
      />
      {diff.data && (
        <span className="cx-total" data-testid="code-total">
          <span className="ap cx-plus">+{diff.data.added}</span>
          <span className="ap cx-minus">−{diff.data.removed}</span>
        </span>
      )}
      <Menu
        label={branch}
        value={`→ ${compareTo}`}
        title="The branch you are on, and what the branch scope compares it with"
        items={branchItems.length ? branchItems : [{ key: 'none', label: 'No other branch', onSelect: () => undefined, disabled: true }]}
        className="cx-branch-pick"
      />
      <div className="grow" />
      <Menu
        variant="detent"
        label="More"
        title="More"
        className="cx-more"
        items={[
          { key: 'all', label: options.allFiles ? 'Show changed files only' : 'Show all files', onSelect: () => setOptions({ allFiles: !options.allFiles }) },
          { key: 'copy', label: 'Copy the tree’s path', note: target.root, onSelect: () => void navigator.clipboard?.writeText(target.root) },
          { key: 'refresh-index', label: 'Index changed files again', note: 'memory.refresh', onSelect: () => void client.call('memory.refresh', { project_id: projectId }).then(() => host.notify('Index refreshed')).catch((cause) => host.notify(errorMessage(cause))) },
        ]}
      />
      <MarkButton name="list" title={options.list ? 'Back to the file' : 'Every change, as a list (review)'} pressed={options.list} onClick={() => setOptions({ list: !options.list })} />
      <MarkButton name="refresh" title="Read the changes again" onClick={() => { runtime().queries.invalidate('code.'); runtime().queries.invalidate('vcs.'); runtime().queries.invalidate('tools.files'); }} />
      <MarkButton name="collapse" title="Collapse every folder in the tree" onClick={() => setShared('code.collapse', Date.now())} />
      <MarkButton name="stage" title={`Stage every change (${changed.length} file${changed.length === 1 ? '' : 's'})`} disabled={busy || changed.length === 0 || scope === 'staged' || scope === 'branch'} onClick={() => void stageAll(true)} />
      <MarkButton name="unstage" title={`Unstage everything (${staged.length} file${staged.length === 1 ? '' : 's'})`} disabled={busy || staged.length === 0} onClick={() => void stageAll(false)} />
      <MarkButton name="color" title={options.marks ? 'Show the plain file (changes unmarked)' : 'Mark the changes'} pressed={options.marks} onClick={() => setOptions({ marks: !options.marks })} />
      <MarkButton name={options.split ? 'unified' : 'split'} title={options.split ? 'Inline (unified) diff' : 'Side by side (split) diff'} pressed={options.split} onClick={() => setOptions({ split: !options.split })} />
    </div>
  );
}

export default definePlugin({ slot: 'code.toolbar@1', component: Toolbar });
