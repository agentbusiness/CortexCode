/**
 * settings.appearance@1 — the ground: Plate (dark, the design's default), Auto (follows the
 * machine), Page (light). The workbench applies the choice (shared `view`); the theme draws both
 * grounds.
 */

import { definePlugin, useShared } from '@cortex/sdk';

type View = 'dark' | 'light' | 'system';
const VIEWS: Array<{ view: View; label: string; note: string }> = [
  { view: 'dark', label: 'Plate', note: 'The dark ground, the design\'s default.' },
  { view: 'system', label: 'Auto', note: 'Follows this Mac: Plate at night, Page by day, as macOS switches.' },
  { view: 'light', label: 'Page', note: 'The light ground.' },
];

function Appearance() {
  const [view, setView] = useShared<View>('view', 'dark');
  const current = VIEWS.find((entry) => entry.view === view) ?? VIEWS[0]!;
  return (
    <div className="cx-settings-panel-body" data-build={__CORTEX_BUILD__} data-panel="appearance">
      <h3 className="cx-settings-title">Ground</h3>
      <div className="seg-set" role="group" aria-label="Ground">
        {VIEWS.map((entry) => (
          <button key={entry.view} type="button" data-view={entry.view} aria-pressed={view === entry.view} onClick={() => setView(entry.view)}>{entry.label}</button>
        ))}
      </div>
      <p className="cx-settings-line">{current.note}</p>
    </div>
  );
}

export default definePlugin({ slot: 'settings.appearance@1', component: Appearance });
