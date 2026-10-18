/**
 * code.tree@1 — the code view's file tree. In the File view: every file (folders loading as they
 * open) or only the changed ones, and pressing a file opens it at its first change. In the Changes
 * view: only the changed files of the scope, and pressing one scrolls the diff to it. Changed files
 * carry +A −R (a deleted-only file shows only −R), single-child folders fold into one row, a
 * filter narrows the list, and a file another working tree (an agent's) changed too says who.
 */

import { useEffect, useMemo } from 'react';
import {
  Caret,
  definePlugin,
  firstChangedLine,
  useCodeDiff,
  useCodeOpen,
  useCodeTarget,
  useQueries,
  useQuery,
  useSelection,
  useShared,
  useSlotState,
  type AgentRecord,
  type FileEntry,
  type VcsStatus,
  ServiceProblem,
} from '@cortex/sdk';
import { buildTree, dirPaths, filterItems, langMark, overlaps, type TreeInput, type TreeNode } from './tree';
import './tree.css';

function Tree() {
  const { target } = useCodeTarget();
  const { projectId, project } = useSelection();
  const diff = useCodeDiff();
  const [open, setOpen] = useCodeOpen();
  const [view] = useShared<'file' | 'changes'>('code.view', 'file');
  const [fileTree, setFileTree] = useShared<'all' | 'changed'>('code.fileTree', 'all');
  const [focus, setFocus] = useShared<{ path: string; at: number } | null>('code.diffFocus', null);
  const changesView = view === 'changes';
  // Every file only in the File view, when asked; the Changes view lists what changed.
  const options = { allFiles: !changesView && fileTree === 'all' };
  const [filter, setFilter] = useSlotState<string>('filter', '');
  const [collapsedAll, setCollapsedAll] = useSlotState<Record<string, string[]>>('collapsed', {});
  const [expandedAll, setExpandedAll] = useSlotState<Record<string, string[]>>('expanded', {});
  const [collapseSignal] = useShared<number>('code.collapse', 0);
  const root = target?.root ?? '';
  const collapsed = collapsedAll[root] ?? [];
  const expanded = expandedAll[root] ?? [];

  // Every file in all-files mode: the folders opened so far, listed by the tools plugin.
  const listedDirs = options.allFiles ? ['', ...expanded] : [];
  const listings = useQueries<FileEntry[]>(
    listedDirs.map((dir) => ({
      key: `tools.files:${root}:${dir}`,
      fetch: async (client) => {
        const result = await client.call<{ entries: FileEntry[] }>('tools.files', dir ? { workspace: root, dir } : { workspace: root });
        const prefix = dir.replace(/\/+$/, '');
        return (result?.entries ?? []).map((entry) => {
          const path = entry.path.replace(/^\.\//, '').replace(/\/+$/, '');
          return { ...entry, path: prefix && !path.startsWith(`${prefix}/`) ? `${prefix}/${path}` : path };
        });
      },
    })),
    { topics: ['tools.files_changed'] },
  );

  // Who else changed these files: the agents' workspaces (and the person's tree, from an agent's).
  const agents = useQuery<AgentRecord[]>(projectId ? `agents.list:${projectId}` : null, async (c) => (await c.call<{ agents: AgentRecord[] }>('agents.list', { project_id: projectId }))?.agents ?? [], { topics: ['agents.changed'] });
  const others = [
    ...(project && project.root !== root ? [{ owner: 'you', root: project.root }] : []),
    ...(agents.data ?? []).filter((agent) => agent.workspace && agent.workspace !== root).map((agent) => ({ owner: `@${agent.name}`, root: agent.workspace! })),
  ];
  const otherStatus = useQueries<VcsStatus>(others.map((other) => ({ key: `vcs.status:${other.root}`, fetch: (c) => c.call<VcsStatus>('vcs.status', { root: other.root }) })), { topics: ['vcs.changed', 'tools.files_changed'] });

  useEffect(() => {
    if (!collapseSignal || !root) return;
    const tree = buildTree(items(), !options.allFiles);
    setCollapsedAll((all) => ({ ...all, [root]: dirPaths(tree) }));
    setExpandedAll((all) => ({ ...all, [root]: [] }));
    // Collapse once per signal.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [collapseSignal]);

  const changed = diff.data?.files ?? [];
  const counts = useMemo(() => new Map(changed.map((file) => {
    let added = 0;
    let removed = 0;
    for (const hunk of file.hunks) for (const line of hunk.lines) {
      if (line.kind === '+') added++;
      else if (line.kind === '-') removed++;
    }
    return [file.path, { added, removed, status: file.status, hunks: file.hunks }];
  })), [changed]);

  function items(): TreeInput[] {
    const list: TreeInput[] = changed.map((file) => ({ path: file.path, added: counts.get(file.path)?.added, removed: counts.get(file.path)?.removed, status: file.status }));
    if (options.allFiles) {
      for (const listing of listings) for (const entry of listing.data ?? []) {
        if (entry.is_dir) list.push({ path: entry.path, dir: true, unloaded: !expanded.includes(entry.path) });
        else if (!counts.has(entry.path)) list.push({ path: entry.path });
      }
    }
    return filterItems(list, filter);
  }

  const owners = new Map<string, string[]>([['here', changed.map((file) => file.path)]]);
  others.forEach((other, index) => owners.set(other.owner, (otherStatus[index]?.data?.files ?? []).map((file) => file.path)));
  const shared = overlaps(owners);

  if (!target) return <div className="cx-tree-view"><p className="empty-line">Open a project to see its files.</p></div>;
  const tree = buildTree(items(), !options.allFiles && !filter);
  const toggle = (path: string, isOpen: boolean) => {
    setCollapsedAll((all) => {
      const list = all[root] ?? [];
      return { ...all, [root]: isOpen ? [...list, path] : list.filter((entry) => entry !== path) };
    });
    if (options.allFiles && !isOpen) setExpandedAll((all) => ({ ...all, [root]: [...new Set([...(all[root] ?? []), path])] }));
  };

  const renderNode = (node: TreeNode, depth: number): React.ReactNode => {
    if (node.dir) {
      const isOpen = options.allFiles ? expanded.includes(node.path) && !collapsed.includes(node.path) : !collapsed.includes(node.path);
      return (
        <li key={`d:${node.path}`} role="treeitem" aria-expanded={isOpen}>
          <button type="button" className="cx-row dir" style={{ paddingLeft: 12 + depth * 12 }} onClick={() => toggle(node.path, isOpen)} title={node.path} data-path={node.path}>
            <Caret dir={isOpen ? 'down' : 'right'} className="cx-caret" />
            <span className="nm">{node.label}</span>
            <Counts added={node.added} removed={node.removed} />
          </button>
          {isOpen && node.children.length > 0 && <ul role="group">{node.children.map((child) => renderNode(child, depth + 1))}</ul>}
        </li>
      );
    }
    const info = counts.get(node.path);
    const selected = changesView ? focus?.path === node.path : open?.root === root && open.path === node.path;
    const also = (shared.get(node.path) ?? []).filter((owner) => owner !== 'here');
    return (
      <li key={`f:${node.path}`} role="treeitem" className={selected ? 'sel' : undefined}>
        <button type="button" className="cx-row file" style={{ paddingLeft: 12 + depth * 12 + 12 }} title={node.path} data-path={node.path}
          aria-current={selected ? 'true' : undefined} data-added={info?.added ?? 0} data-removed={info?.removed ?? 0}
          onClick={() => (changesView ? setFocus({ path: node.path, at: Date.now() }) : setOpen({ root, path: node.path, line: info ? firstChangedLine(info.hunks) : null }))}>
          <span className="cx-lang ap origin">{langMark(node.path)}</span>
          <span className={`nm${info?.status === 'deleted' ? ' gone' : ''}`}>{node.label}</span>
          {also.length > 0 && <span className="ap hot cx-also" title={`Also changed in ${also.join(', ')}'s tree`}>also {also.join(' ')}</span>}
          {info && <Counts added={info.added} removed={info.removed} deleted={info.status === 'deleted'} />}
        </button>
      </li>
    );
  };

  const nothing = tree.children.length === 0;
  return (
    <div className="cx-tree-view" data-build={__CORTEX_BUILD__} data-root={root} data-files={changed.length}>
      <div className="cx-tree-bar">
        <label className="field cx-tree-filter">
          <svg viewBox="0 0 10 10" aria-hidden="true"><circle cx="4.2" cy="4.2" r="3.2" stroke="currentColor" fill="none" /><path d="M6.6 6.6L9 9" stroke="currentColor" /></svg>
          <input type="search" placeholder="Filter files…" aria-label="Filter files" value={filter} onChange={(event) => setFilter(event.target.value)} />
        </label>
        {!changesView && (
          <div className="seg-set" role="group" aria-label="Which files">
            <button type="button" aria-pressed={options.allFiles} onClick={() => setFileTree('all')}>All</button>
            <button type="button" aria-pressed={!options.allFiles} onClick={() => setFileTree('changed')}>Changed</button>
          </div>
        )}
      </div>
      <div className="cx-tree-scroll">
        {diff.error && !diff.data && <ServiceProblem service="vcs" state={diff} className="inline-error" />}
        {!diff.data && !diff.error && <p className="hint cx-pad">Reading the changes…</p>}
        {diff.data && nothing && (
          <p className="empty-line cx-pad">
            {filter ? `No file matches “${filter}”.` : options.allFiles ? 'This tree has no files.' : diff.scope === 'uncommitted' ? 'Nothing uncommitted: the tree matches HEAD.' : diff.scope === 'staged' ? 'Nothing staged.' : diff.scope === 'branch' ? 'This branch has no changes against its base.' : 'The last turn left no uncommitted change.'}
          </p>
        )}
        <ul className="cx-tree" role="tree">{tree.children.map((node) => renderNode(node, 0))}</ul>
      </div>
    </div>
  );
}

function Counts({ added, removed, deleted }: { added: number; removed: number; deleted?: boolean }) {
  if (!added && !removed) return <span className="cx-counts" />;
  return (
    <span className="cx-counts">
      {!deleted && added > 0 && <span className="ap cx-plus">+{added}</span>}
      {removed > 0 && <span className="ap cx-minus">−{removed}</span>}
    </span>
  );
}

export default definePlugin({ slot: 'code.tree@1', component: Tree });
