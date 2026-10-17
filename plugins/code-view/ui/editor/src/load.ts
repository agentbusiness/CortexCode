/**
 * What the editor shows for a file in a scope: the document, the text it is compared with, whether
 * it may be edited, and the git hunks its per-change controls act on. Each text is reconstructed
 * from the working file and the vcs plugin's hunks (structural: reverse-applying a diff).
 *
 *   uncommitted / last turn: the working file, compared with the index (what is not staged yet);
 *                            Stage and Revert act on hunks of the `unstaged` diff.
 *   staged:                  the index, compared with HEAD; Unstage acts on the `staged` diff.
 *   branch:                  HEAD, compared with the branch's base; read-only.
 */

import { decodeRead, errorMessage, reverseApply, type Client, type CodeScope, type Diff, type Hunk, type ReadResult, type VcsStatus } from '@cortex/sdk';

export type Controls = 'stage-revert' | 'unstage' | 'none';

export interface FileModel {
  key: string;
  doc: string;
  /** The compared text; null when there is nothing to compare with (no git). */
  original: string | null;
  editable: boolean;
  /** Hunks the per-change controls name by index (positions on the document's side). */
  hunks: Hunk[];
  controls: Controls;
  untracked: boolean;
  deleted: boolean;
  /** Added and removed lines of the file in the scope (as the tree counts them). */
  added: number;
  removed: number;
  /** Lines staged already (shown in the uncommitted scope, whose baseline is the index). */
  staged: number;
  note: string;
}

async function fileDiff(client: Client, root: string, scope: string, path: string, base?: string | null): Promise<Hunk[]> {
  const diff = await client.call<Diff>('vcs.diff', base ? { root, scope, path, base } : { root, scope, path });
  return (diff.files.find((file) => file.path === path) ?? diff.files[0])?.hunks ?? [];
}

function count(hunks: readonly Hunk[], kind: '+' | '-'): number {
  return hunks.reduce((sum, hunk) => sum + hunk.lines.filter((line) => line.kind === kind).length, 0);
}

export async function loadFile(client: Client, root: string, path: string, scope: CodeScope, base: string | null): Promise<FileModel> {
  const key = `${root}\u0000${path}\u0000${scope}\u0000${base ?? ''}`;
  let current = '';
  let editable = true;
  let note = '';
  let missing = false;
  try {
    const read = decodeRead(await client.call<ReadResult>('tools.read', { workspace: root, target: path }), path);
    current = read.text;
    editable = read.editable;
    if (!read.editable) note = `Read-only: ${read.reason}`;
  } catch (error) {
    const message = errorMessage(error);
    if (!/not.?found|no such file/i.test(message)) throw error;
    missing = true;
    editable = false;
  }
  let status: VcsStatus;
  try {
    status = await client.call<VcsStatus>('vcs.status', { root });
  } catch (error) {
    return { key, doc: current, original: null, editable, hunks: [], controls: 'none', untracked: false, deleted: missing, added: 0, removed: 0, staged: 0, note: `Changes unavailable: ${errorMessage(error)}` };
  }
  if (!status.is_repo) {
    return { key, doc: current, original: null, editable, hunks: [], controls: 'none', untracked: false, deleted: missing, added: 0, removed: 0, staged: 0, note: note || 'Not in a git repository: there is nothing to compare with.' };
  }
  const entry = status.files.find((file) => file.path === path);
  const untracked = entry?.status === 'untracked';
  const deleted = missing || entry?.status === 'deleted';
  if (scope === 'uncommitted' || scope === 'last_turn') {
    if (untracked) {
      return { key, doc: current, original: '', editable, hunks: [], controls: 'stage-revert', untracked, deleted, added: current ? current.split('\n').length - (current.endsWith('\n') ? 1 : 0) : 0, removed: 0, staged: 0, note };
    }
    const [unstaged, staged] = await Promise.all([fileDiff(client, root, 'unstaged', path), fileDiff(client, root, 'staged', path)]);
    const index = reverseApply(current, unstaged);
    return {
      key, doc: current, original: index, editable: editable && !deleted, hunks: unstaged, controls: 'stage-revert', untracked, deleted,
      added: count(unstaged, '+') + count(staged, '+'), removed: count(unstaged, '-') + count(staged, '-'), staged: count(staged, '+') + count(staged, '-'), note,
    };
  }
  if (scope === 'staged') {
    const [unstaged, staged] = await Promise.all([fileDiff(client, root, 'unstaged', path), fileDiff(client, root, 'staged', path)]);
    const index = reverseApply(current, unstaged);
    const head = reverseApply(index, staged);
    return { key, doc: index, original: head, editable: false, hunks: staged, controls: 'unstage', untracked, deleted, added: count(staged, '+'), removed: count(staged, '-'), staged: 0, note: 'The staged text; unstage changes here, edit them in Uncommitted.' };
  }
  const uncommitted = untracked ? [] : await fileDiff(client, root, 'uncommitted', path);
  const head = untracked ? '' : reverseApply(current, uncommitted);
  const branch = await fileDiff(client, root, 'branch', path, base);
  return { key, doc: head, original: reverseApply(head, branch), editable: false, hunks: branch, controls: 'none', untracked, deleted, added: count(branch, '+'), removed: count(branch, '-'), staged: 0, note: 'The file at HEAD against the base; read-only.' };
}
