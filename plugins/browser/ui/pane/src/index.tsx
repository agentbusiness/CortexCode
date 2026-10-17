/**
 * pane.browser@1 — the agent browser's chrome: tabs, an address field and its buttons, and the
 * region the shell lays the live page over (an Electron `WebContentsView`, positioned from the
 * bounds this pane reports). The page itself is the shell's; agents drive the same tabs through
 * `browser.*`, so the person watches what the agent sees.
 */

import { useEffect, useLayoutEffect, useRef, useState } from 'react';
import {
  definePlugin,
  errorMessage,
  runtime,
  useClient,
  useHost,
  useQuery,
  useSelection,
  useShared,
  type BrowserTab,
  type BrowserTabsEvent,
  ServiceProblem,
} from '@cortex/sdk';
import './browser.css';

function BrowserPane({ active }: { active?: boolean }) {
  const client = useClient();
  const host = useHost();
  const { sessionId } = useSelection();
  const tabs = useQuery<BrowserTabsEvent>('browser.tabs', (c) => c.call<BrowserTabsEvent>('browser.tabs', {}));
  const [address, setAddress] = useState('');
  const [editing, setEditing] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const region = useRef<HTMLDivElement>(null);

  // The shell publishes the tab list whenever it changes; the cache takes it as it comes.
  useEffect(() => runtime().client.on('browser.tabs', (_topic, data) => {
    runtime().queries.write<BrowserTabsEvent>('browser.tabs', () => data as BrowserTabsEvent);
  }), []);

  const list = tabs.data?.tabs ?? [];
  const activeId = tabs.data?.active ?? list.at(-1)?.tab_id ?? null;
  const current = list.find((tab) => tab.tab_id === activeId) ?? null;
  useEffect(() => {
    if (!editing) setAddress(current?.url ?? '');
  }, [current?.url, editing]);

  // Report where the page goes; nothing is shown while the pane is not in front.
  // A floating panel (the card, settings) over the pane: the native page steps back while it is open.
  const [overlay] = useShared<boolean>('overlay.open', false);
  const shown = Boolean(active) && current !== null && !overlay;
  useLayoutEffect(() => {
    const element = region.current;
    const shell = host.shell;
    if (!element || !shell) return;
    const report = () => {
      const rect = element.getBoundingClientRect();
      shell.setBrowserBounds({ x: rect.left, y: rect.top, width: rect.width, height: rect.height, visible: shown && rect.width > 0 && rect.height > 0 });
    };
    report();
    const observer = new ResizeObserver(report);
    observer.observe(element);
    window.addEventListener('resize', report);
    return () => {
      observer.disconnect();
      window.removeEventListener('resize', report);
      shell.setBrowserBounds({ x: 0, y: 0, width: 0, height: 0, visible: false });
    };
  }, [host.shell, shown]);

  const run = async (method: string, params: Record<string, unknown>) => {
    setError(null);
    try {
      await client.call(method, params);
      await tabs.refresh();
    } catch (cause) {
      setError(errorMessage(cause));
    }
  };

  const go = async () => {
    const url = address.trim();
    if (!url) return;
    setEditing(false);
    if (current) await run('browser.navigate', { tab_id: current.tab_id, url });
    else await run('browser.open', { url, session_id: sessionId });
  };

  const unavailable = tabs.error && !tabs.data;
  return (
    <div className="cx-browser" data-build={__CORTEX_BUILD__} data-tabs={list.length} data-active-tab={activeId ?? ''}>
      <div className="bar cx-browser-bar">
        <button className="detent" type="button" disabled={!current} onClick={() => current && void run('browser.back', { tab_id: current.tab_id })}>Back</button>
        <button className="detent" type="button" disabled={!current} onClick={() => current && void run('browser.forward', { tab_id: current.tab_id })}>Forward</button>
        <button className="detent" type="button" disabled={!current} onClick={() => current && void run('browser.reload', { tab_id: current.tab_id })}>Reload</button>
        <label className="field cx-address">
          <input
            type="text"
            value={address}
            placeholder="Address"
            aria-label="Address"
            spellCheck={false}
            onFocus={() => setEditing(true)}
            onBlur={() => setEditing(false)}
            onChange={(event) => setAddress(event.target.value)}
            onKeyDown={(event) => {
              if (event.key === 'Enter') {
                event.preventDefault();
                void go();
              }
            }}
          />
        </label>
        <button className="detent send" type="button" disabled={!address.trim()} onClick={() => void go()}>Go</button>
      </div>
      <div className="cx-browser-tabs">
        {list.map((tab: BrowserTab) => (
          <span key={tab.tab_id} className={`cx-btab${tab.tab_id === activeId ? ' on' : ''}`}>
            <button type="button" className="cx-btab-pick" onClick={() => void run('browser.select', { tab_id: tab.tab_id })} title={tab.url}>
              {tab.loading ? '… ' : ''}{tab.title || tab.url || 'New tab'}
            </button>
            <button type="button" className="cx-btab-close" onClick={() => void run('browser.close', { tab_id: tab.tab_id })} aria-label={`Close ${tab.title || tab.url}`}>Close</button>
          </span>
        ))}
        <button className="detent" type="button" onClick={() => void run('browser.open', { url: address.trim() || 'about:blank', session_id: sessionId })}>New tab</button>
      </div>
      {error && <p className="inline-error" role="alert">{error}</p>}
      <div className="cx-browser-region" ref={region} data-visible={shown ? 'true' : 'false'}>
        {unavailable && <ServiceProblem service="browser" state={tabs} className="empty-line" />}
        {!unavailable && list.length === 0 && <p className="empty-line">No page open. Type an address, or let an agent open one.</p>}
        {!host.shell && <p className="hint">Pages show here inside the CortexCode app.</p>}
      </div>
    </div>
  );
}

export default definePlugin({ slot: 'pane.browser@1', component: BrowserPane });
