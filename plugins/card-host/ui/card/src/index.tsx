/**
 * workbench.band@1 — the card host (card.host), at the right of the brand band. A toggle that says
 * the state of things while closed (KV fill · agents running) and, next to it, Open pane or Close
 * pane (the right pane's one switch); and the card it opens, anchored under the band at the
 * toggle's right edge: the header (`card.header@1`), then every section plugin (`card.git@1`,
 * `card.agents@1`, `card.tasks@1`, `card.board@1`, `card.index@1`, `card.model@1`, and any other
 * `card.<name>@1` dropped in; a section with nothing to say renders nothing), then the foot: the
 * settings window (`settings.host@1`) behind a gear, how full the model's KV pool is, and Hide.
 */

import { useEffect, useRef } from 'react';
import { ComputerScope, definePlugin, useHost, useModelState, useQuery, useSelection, useSessions, useShared, useUiPlugins, type AgentRecord } from '@cortex/sdk';
import { cardSections } from './sections';
import './card.css';

/** The card is the shown project's: it talks to that project's computer, though the band it sits
 * in is this computer's. */
function CardRoot() {
  const [node] = useShared<string | null>('projectNode', null);
  return (
    <ComputerScope node={node}>
      <Card />
    </ComputerScope>
  );
}

function Card() {
  const host = useHost();
  const plugins = useUiPlugins();
  const { projectId } = useSelection();
  const [open, setOpen] = useShared<boolean>('card.open', false);
  const [settings, setSettings] = useShared<string | null>('settings.open', null);
  const [, setOverlay] = useShared<boolean>('overlay.open', false);
  const [collapsed, setCollapsed] = useShared<boolean>('layout.paneCollapsed', false);
  const agents = useQuery<AgentRecord[]>(projectId ? `agents.list:${projectId}` : null, async (c) => (await c.call<{ agents: AgentRecord[] }>('agents.list', { project_id: projectId }))?.agents ?? [], { topics: ['agents.changed'] });

  const toggle = (next: boolean) => {
    setOpen(next);
    setOverlay(next || settings !== null);
  };
  const openSettings = (panel: string | null = null) => {
    // '' opens the settings window on its first panel.
    setSettings(panel ?? '');
    setOverlay(true);
  };
  const closeSettings = () => {
    setSettings(null);
    setOverlay(open);
  };

  // With the right pane collapsed the card is where the project lives: collapsing opens it, and
  // opening the pane again (any way: its button, a link, a tab) closes it, so it does not cover
  // the pane.
  const wasCollapsed = useRef(collapsed);
  useEffect(() => {
    if (collapsed) toggle(true);
    else if (wasCollapsed.current) toggle(false);
    wasCollapsed.current = collapsed;
    // `toggle` only writes shared state.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [collapsed]);

  // Escape closes the card (the dialog pattern; the settings window closes itself first).
  useEffect(() => {
    if (!open || settings !== null) return;
    const onKey = (event: KeyboardEvent) => {
      if (event.key === 'Escape' && !event.defaultPrevented) toggle(false);
    };
    document.addEventListener('keydown', onKey);
    return () => document.removeEventListener('keydown', onKey);
    // `toggle` only writes shared state.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [open, settings]);

  const kv = useKv();
  const sessions = useSessions(projectId);
  const waiting = (sessions.data ?? []).filter((session) => session.status === 'waiting').length
    + (agents.data ?? []).filter((agent) => agent.status === 'blocked').length;
  const running = (agents.data ?? []).filter((agent) => ['working', 'running', 'blocked', 'verifying'].includes(agent.status)).length;
  const parts: string[] = [];
  if (waiting > 0) parts.push(`${waiting} need${waiting === 1 ? 's' : ''} you`);
  if (kv) parts.push(`KV ${Math.round(kv.share)}%`);
  if (agents.data) parts.push(`${running} agent${running === 1 ? '' : 's'}`);
  const sections = cardSections(plugins);

  return (
    <div className="cx-card-host" data-build={__CORTEX_BUILD__} data-open={open ? 'true' : 'false'} data-pane={collapsed ? 'collapsed' : 'open'}>
      <div className="cx-card-bar">
        <button type="button" className={`detent cx-card-toggle${waiting > 0 ? ' cx-attn' : ''}`} aria-expanded={open} aria-pressed={open} onClick={() => toggle(!open)}
          title={open ? 'Hide the card' : 'Open the card: git, agents, tasks, board, index, model, settings'} data-summary={parts.join(' · ')}>
          {parts.length ? parts.join(' · ') : 'Card'}
        </button>
        <button type="button" className="detent cx-pane-open" data-action={collapsed ? 'open-pane' : 'close-pane'} aria-pressed={!collapsed} onClick={() => setCollapsed(!collapsed)}
          title={collapsed ? 'Open the right pane again, at its last width and tab' : 'Close the right pane; the conversation takes the width'}>{collapsed ? 'Open pane' : 'Close pane'}</button>
      </div>
      {/* Mounted from the start and only shown when open, so the sections are ready at once. */}
      {(
        <div className="cx-panel cx-card" role="dialog" aria-label="Project card" hidden={!open}>
          <host.Slot name="card.header@1" className="cx-card-header" />
          {sections.map((slot) => (
            <host.Slot key={slot} name={slot} className="cx-card-section" />
          ))}
          <div className="cx-card-foot">
            <button type="button" className="cx-icon" onClick={() => openSettings()} data-action="settings" title="Settings: model, plugins, agents, machine…" aria-label="Settings">
              <Gear />
            </button>
            <KvMeter kv={kv} />
            <button type="button" className="detent tiny" data-action="hide" onClick={() => toggle(false)}>Hide</button>
          </div>
        </div>
      )}
      {/* Settings are this computer's, whichever computer's project is shown. */}
      {settings !== null && <ComputerScope node={null}><host.Slot name="settings.host@1" className="cx-settings-layer" props={{ onClose: closeSettings }} /></ComputerScope>}
    </div>
  );
}

interface Slots { slots: Array<{ busy: boolean; n_tokens: number }>; queued: number }

interface Kv { used: number; total: number; share: number }

/** How much of the model's KV pool the conversations hold, read every two seconds: the band's
 * button says it, the card's foot draws it. */
function useKv(): Kv | null {
  const { info } = useModelState();
  const slots = useQuery<Slots>('model.slots', (c) => c.call<Slots>('model.slots', {}));
  useEffect(() => {
    const timer = setInterval(() => void slots.refresh(), 2000);
    return () => clearInterval(timer);
    // `refresh` is stable per key.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);
  const total = info.data?.ctx_total ?? 0;
  if (!slots.data || total <= 0) return null;
  const used = slots.data.slots.reduce((sum, slot) => sum + slot.n_tokens, 0);
  return { used, total, share: Math.min(100, (used / total) * 100) };
}

function KvMeter({ kv }: { kv: Kv | null }) {
  if (!kv) return <span className="cx-kv-meter" />;
  return (
    <span className="cx-kv-meter" data-kv={Math.round(kv.share)} title={`${kv.used.toLocaleString()} of ${kv.total.toLocaleString()} KV tokens in use`}>
      <span className="cx-kv-bar"><span style={{ width: `${kv.share}%` }} /></span>
      <span className="ap faint">KV {Math.round(kv.share)}%</span>
    </span>
  );
}

function Gear() {
  return (
    <svg viewBox="0 0 16 16" fill="none" stroke="currentColor" strokeWidth="1.3" aria-hidden="true">
      <circle cx="8" cy="8" r="2.2" />
      <path d="M6.9 1.5h2.2l.4 1.9 1.3.6 1.6-1.1 1.6 1.6-1.1 1.6.6 1.3 1.9.4v2.2l-1.9.4-.6 1.3 1.1 1.6-1.6 1.6-1.6-1.1-1.3.6-.4 1.9H6.9l-.4-1.9-1.3-.6-1.6 1.1-1.6-1.6 1.1-1.6-.6-1.3-1.9-.4V6.9l1.9-.4.6-1.3-1.1-1.6 1.6-1.6 1.6 1.1 1.3-.6z" strokeLinejoin="round" />
    </svg>
  );
}

export default definePlugin({ slot: 'workbench.band@1', component: CardRoot });
