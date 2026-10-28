import { useEffect, useState, useSyncExternalStore } from 'react';
import { Cross, useClient, type Client } from '@cortex/sdk';
import type { PluginRegistry } from './registry';
import { isStateSettled, subscribeBoot } from './boot';

/** How long the splash takes to fade once the workbench is drawn (matches splash.css). */
const FADE_MS = 320;

/** A plugin's backend that is up, or that came to rest without coming up. */
const RESTING = new Set(['active', 'crashed', 'stopped']);

interface ListedPlugin {
  plugin?: string;
  category?: string;
  state?: string;
  transport?: string;
}

interface Backends {
  /** Backends the frame runs (the ones the window may wait for). */
  total: number;
  /** Of those, the ones at rest. */
  resting: number;
  /** The first one still starting, by name. */
  starting: string | null;
  /** A backend that crashed: the workbench opens anyway and its band says so. */
  crashed: boolean;
}

const NO_BACKENDS: Backends = { total: 0, resting: 0, starting: null, crashed: false };

function summarize(result: { plugins?: ListedPlugin[]; units?: { id: string; name?: string }[] }): Backends {
  const names = new Map((result.units ?? []).map((unit) => [unit.id, unit.name || unit.id]));
  const backends = (result.plugins ?? []).filter((plugin) => plugin.transport !== 'websocket' && plugin.state !== 'standby');
  const starting = backends.find((plugin) => !RESTING.has(plugin.state ?? ''));
  return {
    total: backends.length,
    resting: backends.filter((plugin) => RESTING.has(plugin.state ?? '')).length,
    starting: starting ? names.get(starting.plugin ?? '') ?? starting.category ?? null : null,
    crashed: backends.some((plugin) => plugin.state === 'crashed'),
  };
}

/** The frame's backends while the window starts, refreshed as their states change. */
function useBackends(client: Client, watching: boolean): Backends {
  const [backends, setBackends] = useState<Backends>(NO_BACKENDS);
  useEffect(() => {
    if (!watching) return;
    let stopped = false;
    let loading = false;
    let again = false;
    // Start-up sends many state changes at once: one list in flight, one more after it.
    const load = () => {
      if (loading) {
        again = true;
        return;
      }
      loading = true;
      client
        .call<{ plugins?: ListedPlugin[]; units?: { id: string; name?: string }[] }>('frame.plugins.list', {})
        .then((result) => {
          if (!stopped) setBackends(summarize(result ?? {}));
        })
        .catch(() => undefined)
        .finally(() => {
          loading = false;
          if (again && !stopped) {
            again = false;
            load();
          }
        });
    };
    load();
    const off = client.on('plugin.state', load);
    const offOpen = client.onState((state) => {
      if (state === 'open') load();
    });
    return () => {
      stopped = true;
      off();
      offOpen();
    };
  }, [client, watching]);
  return backends;
}

/** Every slot on screen has its plugin mounted, failed or known to be missing; the conversation
 * and the sidebar are among them (before the theme they are not drawn yet). */
function useSlotsSettled(): boolean {
  const [settled, setSettled] = useState(false);
  useEffect(() => {
    const check = () => {
      const drawn = document.querySelector('#cc [data-slot="workbench.chat@1"]') && document.querySelector('#cc [data-slot="workbench.sidebar@1"]');
      setSettled(Boolean(drawn) && !document.querySelector('#cc .cx-slot[data-settled="false"]'));
    };
    check();
    const observer = new MutationObserver(check);
    observer.observe(document.body, { subtree: true, childList: true, attributes: true, attributeFilter: ['data-settled'] });
    return () => observer.disconnect();
  }, []);
  return settled;
}

/**
 * The start-up splash: the mark and one progress bar over the window until the workbench is drawn
 * as the person left it (their saved layout loaded, the theme active, every slot's plugin mounted),
 * so nothing is seen moving into place. The bar follows the frame's backends coming up, then the
 * saved state, then the drawing; the line under it names what is being waited for.
 */
export function Splash({ registry, themeReady }: { registry: PluginRegistry; themeReady: boolean }) {
  const client = useClient();
  const [revealed, setRevealed] = useState(false);
  const [gone, setGone] = useState(false);
  const stateSettled = useSyncExternalStore(subscribeBoot, isStateSettled);
  const registryLoaded = useSyncExternalStore((listener) => registry.subscribe(listener), () => registry.isLoaded());
  const backends = useBackends(client, !revealed);
  const slotsSettled = useSlotsSettled();

  const drawn = registryLoaded && themeReady && slotsSettled;
  // The saved state comes through the memory plugin; when a backend crashed it may never come, and
  // the workbench (whose band shows the stopped service) is better than a splash that waits forever.
  const done = drawn && (stateSettled || backends.crashed);

  useEffect(() => {
    if (done) setRevealed(true);
  }, [done]);
  useEffect(() => {
    if (!revealed) return;
    const timer = setTimeout(() => setGone(true), FADE_MS);
    return () => clearTimeout(timer);
  }, [revealed]);

  if (gone) return null;

  const steps = backends.total + 2;
  const reached = backends.resting + (stateSettled ? 1 : 0) + (drawn ? 1 : 0);
  const percent = revealed ? 100 : backends.total === 0 ? 4 : Math.max(4, Math.round((reached / steps) * 100));
  let step: string;
  if (revealed) step = 'Ready';
  else if (!stateSettled && backends.starting) step = `Starting ${backends.starting}`;
  else if (!stateSettled) step = backends.total === 0 ? 'Starting CortexCode' : 'Opening your projects';
  else step = 'Drawing the workbench';

  return (
    <div
      className="cx-splash"
      data-leaving={revealed ? 'true' : 'false'}
      role="progressbar"
      aria-label="Starting CortexCode"
      aria-valuemin={0}
      aria-valuemax={100}
      aria-valuenow={percent}
      aria-valuetext={step}
    >
      <div className="cx-splash-mark">
        {/* Beside the page (/ui/), from the renderer's public files. */}
        <img src="cortexcode.svg" width={76} height={76} alt="" />
        <div className="cx-splash-word">
          <b>CortexCode</b>
          <Cross className="cx-splash-cross" />
          <i>IronGraph</i>
        </div>
      </div>
      <div className="cx-splash-bar">
        <span style={{ width: `${percent}%` }} />
      </div>
      <p className="cx-splash-step">{step}</p>
    </div>
  );
}
