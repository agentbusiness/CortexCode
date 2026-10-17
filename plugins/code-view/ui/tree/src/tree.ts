/**
 * The file tree, structurally: changed (and, on request, all) files, single-child directories
 * folded into one row (`src/embedding`), counts summed up the tree, a language mark per file by
 * extension, and the files more than one working tree changed. No judgement lives here.
 */

export interface TreeNode {
  /** What the row shows: a name, or folded directories (`src/embedding`). */
  label: string;
  /** Full path of the directory or file. */
  path: string;
  dir: boolean;
  children: TreeNode[];
  added: number;
  removed: number;
  /** A directory whose contents are not known yet (all-files mode, not loaded). */
  unloaded?: boolean;
  status?: string;
}

export interface TreeInput {
  path: string;
  dir?: boolean;
  added?: number;
  removed?: number;
  status?: string;
  unloaded?: boolean;
}

/** Build the tree; directories first, then files, by name; counts summed up the tree. */
export function buildTree(items: readonly TreeInput[], fold = true): TreeNode {
  const root: TreeNode = { label: '', path: '', dir: true, children: [], added: 0, removed: 0 };
  const dirs = new Map<string, TreeNode>([['', root]]);
  const ensureDir = (path: string): TreeNode => {
    const existing = dirs.get(path);
    if (existing) return existing;
    const slash = path.lastIndexOf('/');
    const parent = ensureDir(slash < 0 ? '' : path.slice(0, slash));
    const node: TreeNode = { label: path.slice(slash + 1), path, dir: true, children: [], added: 0, removed: 0 };
    parent.children.push(node);
    dirs.set(path, node);
    return node;
  };
  for (const item of items) {
    const clean = item.path.replace(/^\.\//, '').replace(/\/+$/, '');
    if (!clean) continue;
    if (item.dir) {
      const node = ensureDir(clean);
      if (item.unloaded && node.children.length === 0) node.unloaded = true;
      continue;
    }
    const slash = clean.lastIndexOf('/');
    const parent = ensureDir(slash < 0 ? '' : clean.slice(0, slash));
    if (parent.children.some((child) => !child.dir && child.path === clean)) continue;
    parent.unloaded = false;
    parent.children.push({ label: clean.slice(slash + 1), path: clean, dir: false, children: [], added: item.added ?? 0, removed: item.removed ?? 0, status: item.status });
  }
  const finish = (node: TreeNode): void => {
    for (const child of node.children) if (child.dir) finish(child);
    node.children.sort((a, b) => Number(b.dir) - Number(a.dir) || a.label.localeCompare(b.label));
    if (node.dir && node.path !== '') {
      node.added = node.children.reduce((sum, child) => sum + child.added, 0);
      node.removed = node.children.reduce((sum, child) => sum + child.removed, 0);
    }
  };
  finish(root);
  root.added = root.children.reduce((sum, child) => sum + child.added, 0);
  root.removed = root.children.reduce((sum, child) => sum + child.removed, 0);
  if (fold) foldSingles(root);
  return root;
}

/** `src` holding only `embedding` becomes one row `src/embedding` (structural folding). */
function foldSingles(node: TreeNode): void {
  for (let i = 0; i < node.children.length; i++) {
    let child = node.children[i]!;
    while (child.dir && !child.unloaded && child.children.length === 1 && child.children[0]!.dir) {
      const only = child.children[0]!;
      child = { ...only, label: `${child.label}/${only.label}` };
    }
    node.children[i] = child;
    if (child.dir) foldSingles(child);
  }
}

/** Keep files whose path contains `filter` (case-insensitive), with the directories that hold them. */
export function filterItems<T extends { path: string; dir?: boolean }>(items: readonly T[], filter: string): T[] {
  const needle = filter.trim().toLowerCase();
  if (!needle) return [...items];
  return items.filter((item) => !item.dir && item.path.toLowerCase().includes(needle));
}

/** Every directory path in a tree (for collapse-all and expand-all). */
export function dirPaths(node: TreeNode): string[] {
  const out: string[] = [];
  const walk = (current: TreeNode) => {
    for (const child of current.children) {
      if (!child.dir) continue;
      out.push(child.path);
      walk(child);
    }
  };
  walk(node);
  return out;
}

const LANGS: Record<string, string> = {
  rs: 'RS', ts: 'TS', tsx: 'TSX', js: 'JS', jsx: 'JSX', mjs: 'JS', cjs: 'JS', py: 'PY', sh: 'SH', zsh: 'SH', bash: 'SH',
  md: 'MD', markdown: 'MD', toml: 'TOML', json: 'JSON', yaml: 'YML', yml: 'YML', css: 'CSS', html: 'HTML', go: 'GO',
  c: 'C', h: 'H', cpp: 'C++', swift: 'SWIFT', sql: 'SQL', lock: 'LOCK', txt: 'TXT',
};

/** The file's language as a short apparatus mark (the design sets type, it has no icon set). */
export function langMark(path: string): string {
  const name = path.slice(path.lastIndexOf('/') + 1);
  if (name === 'Cargo.toml') return 'TOML';
  if (name === 'Dockerfile') return 'DOCK';
  if (name === 'Makefile') return 'MAKE';
  const ext = name.includes('.') ? name.slice(name.lastIndexOf('.') + 1).toLowerCase() : '';
  return LANGS[ext] ?? (ext ? ext.slice(0, 4).toUpperCase() : 'FILE');
}

/** Files that more than one owner (the person's tree, agents' workspaces) changed. */
export function overlaps(byOwner: ReadonlyMap<string, readonly string[]>): Map<string, string[]> {
  const owners = new Map<string, string[]>();
  for (const [owner, paths] of byOwner) {
    for (const path of new Set(paths)) owners.set(path, [...(owners.get(path) ?? []), owner]);
  }
  return new Map([...owners].filter(([, list]) => list.length > 1));
}
