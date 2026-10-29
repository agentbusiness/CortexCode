/**
 * workbench.rightPane@1 — the right pane host. Every plugin on a `pane.<name>@1` slot is a view
 * (code, terminal, markdown, board, browser, and any other dropped in); one is in front. A view
 * mounts the first time it is opened and stays mounted after, hidden, so a terminal or a browser
 * keeps its life while another view is in front. Props to each view: `{ active }`. The pane is
 * opened and closed from the band (Open pane / Close pane, next to the card's button).
 */

import { useState } from 'react';
import { definePlugin, useHost, useShared, useUiPlugins } from '@cortex/sdk';
import { paneViews } from './views';
import './rightpane.css';

function RightPane() {
  const host = useHost();
  const views = paneViews(useUiPlugins());
  const [tab, setTab] = useShared<string>('pane.tab', 'code');
  const active = views.some((view) => view.name === tab) ? tab : views[0]?.name ?? '';
  const [visited, setVisited] = useState<string[]>(active ? [active] : []);
  if (active && !visited.includes(active)) setVisited([...visited, active]);

  return (
    <div className="cx-pane" data-build={__CORTEX_BUILD__} data-tab={active}>
      <div className="cx-pane-tabs">
        <div className="seg-set" role="tablist" aria-label="Right pane">
          {views.map((view) => (
            <button key={view.name} type="button" role="tab" aria-selected={active === view.name} aria-pressed={active === view.name}
              data-tab={view.name} onClick={() => setTab(view.name)}>
              {view.label}
            </button>
          ))}
        </div>
      </div>
      <div className="cx-pane-body">
        {views.length === 0 && <p className="empty-line">No right-pane views are installed.</p>}
        {views.filter((view) => visited.includes(view.name)).map((view) => (
          <div key={view.name} className="cx-pane-view" hidden={active !== view.name} data-view={view.name} role="tabpanel">
            <host.Slot name={view.slot} props={{ active: active === view.name }} />
          </div>
        ))}
      </div>
    </div>
  );
}

export default definePlugin({ slot: 'workbench.rightPane@1', component: RightPane });
