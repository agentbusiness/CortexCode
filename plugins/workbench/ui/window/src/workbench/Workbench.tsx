import { useCallback, useEffect, useMemo, useRef, useState, useSyncExternalStore, type CSSProperties } from 'react';
import { ComputerScope, Cross, Divider, HostContext, useShared, useThemeData, type Host, type ShellBridge, type SlotListProps, type SlotRenderProps } from '@cortex/sdk';
import { SlotHost } from './SlotHost';
import { Splash } from './Splash';
import { WorkbenchContext, importPlugin, linkStyle, type WorkbenchServices } from './context';
import type { PluginRegistry } from './registry';
import type { SwapRecord } from './engine';

export type ThemePreference = 'dark' | 'light' | 'system';

const VIEW_ATTR: Record<ThemePreference, string> = { system: 'auto', dark: 'plate', light: 'page' };

/** Put the chosen ground on the document: `auto` follows the machine. */
export function applyTheme(preference: ThemePreference): void {
  const media = window.matchMedia('(prefers-color-scheme: dark)');
  const resolved = preference === 'system' ? (media.matches ? 'dark' : 'light') : preference;
  document.documentElement.dataset.theme = resolved;
  document.documentElement.style.colorScheme = resolved;
}


/**
 * The workbench core: the window frame (the plate with its brand mark), the regions,
 * and the slot machinery. Everything a person can press lives in a plugin: the band's items, the
 * sidebar, the conversation, the card, the right pane. The core holds no feature UI.
 */
export function Workbench({ registry, shell }: { registry: PluginRegistry; shell: ShellBridge | null }) {
  const [view] = useShared<ThemePreference>('view', 'dark');
  const [toast, setToast] = useState<{ text: string; more: number } | null>(null);
  const toastTimer = useRef<ReturnType<typeof setTimeout> | null>(null);

  useEffect(() => {
    applyTheme(view);
    if (view !== 'system') return;
    const media = window.matchMedia('(prefers-color-scheme: dark)');
    const onChange = () => applyTheme('system');
    media.addEventListener('change', onChange);
    return () => media.removeEventListener('change', onChange);
  }, [view]);

  const notify = useCallback((text: string) => {
    // One toast: the newest line, with a count standing in for the ones it replaced.
    setToast((current) => ({ text, more: current ? current.more + 1 : 0 }));
    if (toastTimer.current) clearTimeout(toastTimer.current);
    toastTimer.current = setTimeout(() => setToast(null), 6000);
  }, []);

  const onSwap = useCallback((record: SwapRecord) => {
    const swaps = ((window as unknown as { __cortexSwaps?: SwapRecord[] }).__cortexSwaps ??= []);
    swaps.push(record);
    // A swap that worked is silent (the part simply shows its new version); only a swap that did
    // not take is told.
    if (record.outcome === 'rolled_back') notify(`Put back the previous ${slotLabel(record.slot)}`);
    else if (record.outcome === 'rejected') {
      console.error(`UI plugin for ${record.slot} rejected: ${record.reason ?? ''}`);
      notify(`Kept the running ${slotLabel(record.slot)}: the new build failed (${record.reason ?? 'no reason given'})`);
    }
  }, [notify]);

  const services = useMemo<WorkbenchServices>(() => ({ registry, loadModule: importPlugin, loadStyle: linkStyle, onSwap }), [registry, onSwap]);

  const host = useMemo<Host>(() => ({
    Slot: (props: SlotRenderProps) => <BoundSlot registry={registry} {...props} />,
    Slots: (props: SlotListProps) => <SlotList registry={registry} {...props} />,
    shell,
    notify,
    uiPlugins: () => registry.list(),
    subscribeUiPlugins: (listener) => registry.subscribe(listener),
    reloadPlugin: (id) => registry.reload(id),
  }), [registry, shell, notify]);

  // The window is drawn once a theme is active (or when the frame serves none: then unstyled).
  const theme = useThemeData();
  const themeAvailable = useSyncExternalStore((listener) => registry.subscribe(listener), () => !registry.isLoaded() || Boolean(registry.forSlot('theme@1')));
  const ready = Boolean(theme) || !themeAvailable;
  const layout = useLayout();

  return (
    <WorkbenchContext.Provider value={services}>
      <HostContext.Provider value={host}>
        <div id="cc" data-view={VIEW_ATTR[view]} className="drawn" data-theme-ready={ready ? 'true' : 'false'}>
          {/* The theme and the band are this computer's, whichever computer's project is shown. */}
          <ComputerScope node={null}>
            <BoundSlot registry={registry} name="theme@1" className="cx-theme-slot" />
          </ComputerScope>
          {ready && (<>
          <svg className="reg tl" viewBox="0 0 11 11" aria-hidden><path d="M5.5 0v11M0 5.5h11" stroke="currentColor" strokeWidth="1" /></svg>
          <svg className="reg tr" viewBox="0 0 11 11" aria-hidden><path d="M5.5 0v11M0 5.5h11" stroke="currentColor" strokeWidth="1" /></svg>
          <div className="band">
            <div className="mark">
              <b>CortexCode</b>
              <Cross />
              <i>IronGraph</i>
            </div>
            <div className="grow" />
            <ComputerScope node={null}>
              <SlotList registry={registry} name="workbench.band@1" className="cx-band-item" />
            </ComputerScope>
          </div>
          <div ref={layout.work} className="cx-work" data-pane={layout.collapsed ? 'collapsed' : 'open'} data-sidebar={layout.strip ? 'strip' : 'open'} style={layout.style}>
            <div className="cx-left">
              <BoundSlot registry={registry} name="workbench.sidebar@1" />
            </div>
            <Divider name="sidebar" label="Sidebar width" onMove={layout.moveSidebar} onEnd={layout.endSidebar} onReset={layout.resetSidebar} />
            <div className="cx-center">
              <BoundSlot registry={registry} name="workbench.chat@1" />
            </div>
            <Divider name="pane" label="Right pane width" className="cx-pane-divider" onMove={layout.movePane} onEnd={layout.endPane} onReset={layout.resetPane} />
            <div className="cx-right">
              <BoundSlot registry={registry} name="workbench.rightPane@1" />
            </div>
          </div>
          </>)}
          {/* One transient line; it leaves on its own, so the core frame has nothing to press. */}
          <div className={`toast${toast ? ' up' : ''}`} role="status" aria-live="polite">
            {toast && (
              <>
                <span className="ap">{toast.text}</span>
                {toast.more > 0 && <span className="ap faint">+{toast.more}</span>}
              </>
            )}
          </div>
        </div>
        {/* Over the window until it is drawn as the person left it. */}
        <Splash registry={registry} themeReady={ready} />
      </HostContext.Provider>
    </WorkbenchContext.Provider>
  );
}

/** A slot whose plugin can be mounted again from scratch (Reload in Settings). */
function BoundSlot({ registry, name, props, className }: SlotRenderProps & { registry: PluginRegistry }) {
  const token = useSyncExternalStore(
    (listener) => registry.subscribe(listener),
    () => {
      const plugin = registry.forSlot(name);
      return plugin ? `${plugin.id}:${registry.reloadToken(plugin.id)}` : '';
    },
  );
  return <SlotHost key={token} name={name} props={props} className={className} />;
}

/** Every plugin contributed to a slot, each in its own slot host (each hot-swaps on its own). */
function SlotList({ registry, name, props, className }: SlotListProps & { registry: PluginRegistry }) {
  const ids = useSyncExternalStore(
    (listener) => registry.subscribe(listener),
    () => registry.allForSlot(name).map((plugin) => `${plugin.id}:${registry.reloadToken(plugin.id)}`).join('\n'),
  );
  if (!ids) return null;
  return (
    <>
      {ids.split('\n').map((entry) => {
        const id = entry.slice(0, entry.lastIndexOf(':'));
        return <SlotHost key={entry} name={name} pluginId={id} props={props} className={className} />;
      })}
    </>
  );
}

function slotLabel(slot: string): string {
  const name = slot.replace(/@\d+$/, '').split('.').pop() ?? slot;
  return name.replace(/([a-z])([A-Z])/g, '$1 $2').toLowerCase();
}

// ─── the split ─────────────────────────────────────────────────────────────────────────────
// The core owns the window's three columns: the sidebar, the conversation and the right pane.
// Widths and whether the pane is collapsed live in the workbench's durable state, so they survive
// restarts and hot swaps. The right pane's plugin collapses it; the card's plugin (in the band)
// opens it again.

const SIDEBAR_DEFAULT = 248;
/** The sidebar collapsed to its strip of marks (on the 4px lattice). */
const SIDEBAR_STRIP = 44;
const SIDEBAR_MIN = 180;
const SIDEBAR_MAX = 480;
const CHAT_MIN = 360;
const PANE_MIN = 320;
const PANE_DEFAULT = '46%';

const clamp = (value: number, min: number, max: number) => Math.max(min, Math.min(max, value));

function useLayout() {
  const work = useRef<HTMLDivElement>(null);
  const [sidebar, setSidebar] = useShared<number | null>('layout.sidebar', null);
  const [pane, setPane] = useShared<number | null>('layout.pane', null);
  const [collapsed, setCollapsed] = useShared<boolean>('layout.paneCollapsed', false);
  const [strip, setStrip] = useShared<boolean>('layout.sidebarCollapsed', false);
  const live = useRef<{ sidebar: number | null; sidebarSnap: boolean; pane: number | null; snap: boolean }>({ sidebar: null, sidebarSnap: false, pane: null, snap: false });

  const measure = () => {
    const element = work.current;
    if (!element) return null;
    const box = element.getBoundingClientRect();
    const left = (element.querySelector(':scope > .cx-left') as HTMLElement | null)?.getBoundingClientRect().width ?? SIDEBAR_DEFAULT;
    const right = collapsed ? 0 : (element.querySelector(':scope > .cx-right') as HTMLElement | null)?.getBoundingClientRect().width ?? 0;
    return { element, box, left, right };
  };

  const moveSidebar = (x: number) => {
    const m = measure();
    if (!m) return;
    const max = Math.max(SIDEBAR_MIN, Math.min(SIDEBAR_MAX, m.box.width - m.right - CHAT_MIN - 2));
    const raw = x - m.box.left;
    // Below its minimum the sidebar snaps to its strip; dragged out of the strip it opens.
    const snap = raw < SIDEBAR_MIN;
    const width = snap ? SIDEBAR_STRIP : clamp(raw, SIDEBAR_MIN, max);
    live.current.sidebar = width;
    live.current.sidebarSnap = snap;
    m.element.style.setProperty('--cx-left', `${width}px`);
  };
  const endSidebar = () => {
    if (live.current.sidebar !== null) {
      if (live.current.sidebarSnap) setStrip(true);
      else {
        setSidebar(Math.round(live.current.sidebar));
        setStrip(false);
      }
    }
    live.current.sidebar = null;
    live.current.sidebarSnap = false;
  };
  const movePane = (x: number) => {
    const m = measure();
    if (!m) return;
    const raw = m.box.right - x;
    const max = Math.max(PANE_MIN, m.box.width - m.left - CHAT_MIN - 2);
    const width = clamp(raw, PANE_MIN, max);
    // Dragged below its minimum, the pane will collapse when released.
    live.current.snap = raw < PANE_MIN;
    live.current.pane = width;
    m.element.style.setProperty('--cx-pane', `${width}px`);
    const divider = m.element.querySelector(':scope > .cx-pane-divider') as HTMLElement | null;
    if (divider) divider.dataset.snap = live.current.snap ? 'collapse' : '';
  };
  const endPane = () => {
    const element = work.current;
    const divider = element?.querySelector(':scope > .cx-pane-divider') as HTMLElement | null;
    if (divider) divider.dataset.snap = '';
    if (live.current.snap) {
      // Collapsed; the width it had stays for when it opens again.
      element?.style.setProperty('--cx-pane', pane !== null ? `${pane}px` : PANE_DEFAULT);
      setCollapsed(true);
    } else if (live.current.pane !== null) {
      setPane(Math.round(live.current.pane));
    }
    live.current.pane = null;
    live.current.snap = false;
  };
  const resetSidebar = () => {
    work.current?.style.setProperty('--cx-left', `${SIDEBAR_DEFAULT}px`);
    setSidebar(null);
    setStrip(false);
  };
  const resetPane = () => {
    work.current?.style.setProperty('--cx-pane', PANE_DEFAULT);
    setPane(null);
  };

  const style = {
    '--cx-left': `${strip ? SIDEBAR_STRIP : sidebar ?? SIDEBAR_DEFAULT}px`,
    '--cx-pane': pane !== null ? `${pane}px` : PANE_DEFAULT,
  } as CSSProperties;
  return { work, collapsed, strip, style, moveSidebar, endSidebar, movePane, endPane, resetSidebar, resetPane };
}
