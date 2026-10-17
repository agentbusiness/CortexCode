/**
 * settings.host@1 — the settings window (CortexCode has no settings page elsewhere). Every plugin
 * on a `settings.<name>@1` slot is a panel — model, plugins, agents, notify, and any
 * dropped in — listed on the left; the open one fills the right. Props: `{ onClose }`.
 */

import { useEffect, useRef } from 'react';
import { definePlugin, useHost, useShared, useUiPlugins } from '@cortex/sdk';
import { settingsPanels } from './panels';
import './settings.css';

function SettingsHost({ onClose }: { onClose?: () => void }) {
  const host = useHost();
  const panels = settingsPanels(useUiPlugins());
  const [open, setOpen] = useShared<string | null>('settings.open', null);
  const current = panels.find((panel) => panel.name === open) ?? panels[0];
  const close = () => (onClose ? onClose() : setOpen(null));
  const closeRef = useRef(close);
  closeRef.current = close;
  const dialog = useRef<HTMLDivElement>(null);
  // The dialog pattern: focus moves in, Escape closes, focus returns where it was.
  useEffect(() => {
    const before = document.activeElement instanceof HTMLElement ? document.activeElement : null;
    dialog.current?.focus({ preventScroll: true });
    const onKey = (event: KeyboardEvent) => {
      if (event.key !== 'Escape' || event.defaultPrevented) return;
      event.preventDefault();
      closeRef.current();
    };
    document.addEventListener('keydown', onKey);
    return () => {
      document.removeEventListener('keydown', onKey);
      if (before?.isConnected) before.focus({ preventScroll: true });
    };
  }, []);
  return (
    <div className="sheet-scrim open cx-settings-scrim" data-build={__CORTEX_BUILD__}>
      <div ref={dialog} className="cx-panel cx-settings" role="dialog" aria-modal="true" aria-label="Settings" tabIndex={-1}>
        <div className="sheet-h">
          <span className="ap">Settings</span>
          <span className="grow" />
          <button className="detent" type="button" onClick={close}>Close</button>
        </div>
        <div className="cx-settings-body">
          <ul className="nav cx-settings-nav">
            {panels.map((panel, index) => (
              <li key={panel.slot} className={panel === current ? 'on' : undefined}>
                <button type="button" onClick={() => setOpen(panel.name)} data-panel={panel.name}>
                  <span className="kk">{['I', 'II', 'III', 'IV', 'V', 'VI', 'VII', 'VIII', 'IX', 'X'][index] ?? index + 1}</span>
                  <span className="nm">{panel.label}</span>
                </button>
              </li>
            ))}
          </ul>
          <div className="cx-settings-panel">
            {current ? <host.Slot name={current.slot} /> : <p className="empty-line">No settings panels are installed.</p>}
          </div>
        </div>
      </div>
    </div>
  );
}

export default definePlugin({ slot: 'settings.host@1', component: SettingsHost });
