/**
 * workbench.band@1 — the workbench's connection to the frame, only when it is not working: while
 * connected it draws nothing; connecting (at start, or reconnecting after the frame went away) and
 * unreachable show as the design's state mark in rubric, so a lost frame is never silent.
 */

import { definePlugin, useConnection } from '@cortex/sdk';

function Connection() {
  const state = useConnection();
  if (state === 'open') return null;
  const label = state === 'connecting' ? 'Connecting to the frame' : 'Frame unreachable';
  return (
    <div className={`state cx-conn-${state}`} role="status" title="The workbench's connection to the frame: it reconnects on its own" data-build={__CORTEX_BUILD__}>
      <span className={`pip${state === 'connecting' ? ' hollow' : ''}`} />
      <span className="ap live" data-connection={state}>{label}</span>
    </div>
  );
}

export default definePlugin({ slot: 'workbench.band@1', component: Connection });
