/**
 * code.editor@1 — the code view's editor: the whole file in CodeMirror 6, editable, with what
 * changed marked inline (`@codemirror/merge`: added lines on a wash with a gutter rule, removed
 * lines as read-only struck-through widgets, or side by side), Stage / Revert (or Unstage) on
 * every change, saving through `tools.save`. Over the code it writes who made each change (the
 * journal step, with a jump to it) and, for each changed symbol, the call sites that reach it
 * (`memory.impact`); hovering a name shows its signature and callers (`memory.symbol`). The
 * header names the task the file is for and the tests that reach the change, warns when another
 * working tree changed the same file, and a hairline at the foot holds the tree's checkpoints.
 * In list mode it is the review: every change of the scope, and a commit.
 */

import { definePlugin, useCodeOpen, useCodeOptions, useCodeTarget } from '@cortex/sdk';
import { FileEditor } from './FileEditor';
import { ListView } from './ListView';
import './editor.css';

function Editor({ active }: { active?: boolean }) {
  const { target } = useCodeTarget();
  const [open] = useCodeOpen();
  const [options] = useCodeOptions();
  if (!target) return <div className="cx-editor-view"><p className="empty-line">Open a project to see its code.</p></div>;
  if (options.list) return <div className="cx-editor-view" data-build={__CORTEX_BUILD__}><ListView root={target.root} /></div>;
  const file = open && open.root === target.root ? open : null;
  return (
    <div className="cx-editor-view" data-build={__CORTEX_BUILD__}>
      {file ? (
        <FileEditor key={`${file.root}\u0000${file.path}`} open={file} active={Boolean(active)} />
      ) : (
        <div className="cx-editor-empty">
          <p className="empty-line">Pick a file in the tree. Changed files come first; each opens at its first change, with who made it.</p>
        </div>
      )}
    </div>
  );
}

export default definePlugin({ slot: 'code.editor@1', component: Editor });
