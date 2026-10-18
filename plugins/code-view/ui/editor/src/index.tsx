/**
 * code.editor@1 — the code view's main area, one of two views (the toolbar's switch):
 * - File (the default): the file as it is on disk, editable, saving through `tools.save`; the
 *   numbers of changed lines are orange. Opening a file anywhere (a chat's edit, a tree row, a
 *   diff's file name) shows it here at its change.
 * - Changes: every change of the scope as one diff, drawn as it scrolls into view, with Stage and
 *   Revert, the tree's checkpoints and a commit at the foot.
 */

import { useEffect, useRef } from 'react';
import { definePlugin, useCodeOpen, useCodeTarget, useShared } from '@cortex/sdk';
import { FileEditor } from './FileEditor';
import { ListView } from './ListView';
import './editor.css';

function Editor({ active }: { active?: boolean }) {
  const { target } = useCodeTarget();
  const [open] = useCodeOpen();
  const [view, setView] = useShared<'file' | 'changes'>('code.view', 'file');
  // A file opened anywhere (after this view mounted) is shown in the File view.
  const seen = useRef(open);
  useEffect(() => {
    if (open === seen.current) return;
    seen.current = open;
    if (open) setView('file');
  }, [open, setView]);
  if (!target) return <div className="cx-editor-view"><p className="empty-line">Open a project to see its code.</p></div>;
  if (view === 'changes') return <div className="cx-editor-view" data-build={__CORTEX_BUILD__}><ListView root={target.root} /></div>;
  const file = open && open.root === target.root ? open : null;
  return (
    <div className="cx-editor-view" data-build={__CORTEX_BUILD__}>
      {file ? (
        <FileEditor key={`${file.root}\u0000${file.path}`} open={file} active={Boolean(active)} />
      ) : (
        <div className="cx-editor-empty">
          <p className="empty-line">Pick a file in the tree.</p>
        </div>
      )}
    </div>
  );
}

export default definePlugin({ slot: 'code.editor@1', component: Editor });
