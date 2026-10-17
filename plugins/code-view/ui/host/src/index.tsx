/**
 * pane.code@1 — the code view: one view, no tabs. The toolbar (`code.toolbar@1`) across the top,
 * the editor (`code.editor@1`) with the whole file and its inline diff in the middle, the file
 * tree (`code.tree@1`) on the right, all visible at once. Each part is its own plugin and
 * hot-swaps on its own; they share the `code.*` state of @cortex/sdk. The divider between editor
 * and tree drags (the tree's width is this plugin's durable state); a double click restores it.
 */

import { useRef, type CSSProperties } from 'react';
import { Divider, definePlugin, useHost, useSlotState } from '@cortex/sdk';
import './host.css';

const TREE_DEFAULT = 260;
const TREE_MIN = 180;
const EDITOR_MIN = 320;

function CodeHost({ active }: { active?: boolean }) {
  const host = useHost();
  const body = useRef<HTMLDivElement>(null);
  const [tree, setTree] = useSlotState<number | null>('treeWidth', null);
  const live = useRef<number | null>(null);
  const move = (x: number) => {
    const element = body.current;
    if (!element) return;
    const box = element.getBoundingClientRect();
    const max = Math.max(TREE_MIN, box.width - EDITOR_MIN - 1);
    const width = Math.max(TREE_MIN, Math.min(max, box.right - x));
    live.current = width;
    element.style.setProperty('--cx-tree', `${width}px`);
  };
  const end = () => {
    if (live.current !== null) setTree(Math.round(live.current));
    live.current = null;
  };
  const reset = () => {
    body.current?.style.setProperty('--cx-tree', `${TREE_DEFAULT}px`);
    setTree(null);
  };
  return (
    <div className="cx-code-view" data-build={__CORTEX_BUILD__}>
      <host.Slot name="code.toolbar@1" className="cx-code-toolbar" />
      <div ref={body} className="cx-code-body" style={{ '--cx-tree': `${tree ?? TREE_DEFAULT}px` } as CSSProperties}>
        <host.Slot name="code.editor@1" className="cx-code-editor" props={{ active: Boolean(active) }} />
        <Divider name="code-tree" label="File tree width" onMove={move} onEnd={end} onReset={reset} />
        <host.Slot name="code.tree@1" className="cx-code-tree" />
      </div>
    </div>
  );
}

export default definePlugin({ slot: 'pane.code@1', component: CodeHost });
