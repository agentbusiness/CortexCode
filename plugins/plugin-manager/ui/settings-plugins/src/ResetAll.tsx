/**
 * The status line at the top of Settings > Plugins and the reset beside it. The line says, live,
 * whether every plugin runs the build it should (the official release, or the build this app came
 * with when no release can be read) and which carry the user's changes (update.reset_plan: the
 * version installed, the version to go to, the user's changed files). The button opens a sheet with
 * the same list, asks once, runs update.reset, shows its progress (update.status) and then the
 * outcome per plugin. The reset reinstalls this panel's own plugin too, which remounts the panel:
 * the start time is kept in the slot's durable state, and the outcome is read back from
 * update.status `recent`.
 */

import { useEffect, useRef, useState } from 'react';
import { createPortal } from 'react-dom';
import { errorMessage, useClient, useQuery, useSlotInfo, useSlotState } from '@cortex/sdk';

interface ResetItem { plugin: string; name?: string; installed?: string; release?: string; from?: string; files?: string[]; action: string }
interface Plan { plugins: ResetItem[]; running?: boolean; offline?: string | null }
interface Applied { plugin: string; version: string; ms: number; outcome: string; detail?: string }
interface Status { state: string; detail?: string; recent?: Applied[] }

/** A reset started and not shown as finished yet: when, in ms (null: none). */
const STARTED = 'reset-all-started';
/** After this long a started reset with no outcome is forgotten. */
const STALE_MS = 15 * 60_000;

function changed(item: ResetItem): string {
  const n = item.files?.length ?? 0;
  return n ? `${n} changed file${n === 1 ? '' : 's'} dropped` : 'no changes of yours';
}

/** `0.1.0 (release)` or `0.1.0 (the app's build)`: the version a reset goes to. */
function target(item: ResetItem): string {
  return `${version(item.release ?? '')} (${item.from === 'app' ? "the app's build" : 'release'})`;
}

/** The version without its build tag: `0.1.0+dev.20261026.172215` reads `0.1.0`. */
export function version(full: string): string {
  return full.split('+')[0] || full;
}

/** What the reset does to one plugin, in one line. */
function line(item: ResetItem): string {
  switch (item.action) {
    case 'install': return `${version(item.installed || '—')} → ${target(item)}; ${changed(item)}`;
    case 'source': return `${target(item)} stays; ${changed(item)}`;
    case 'none': return `${target(item)}, unchanged`;
    case 'frame': return `${target(item)} needs a newer frame; left as it is`;
    default: return 'not in the releases nor in the app; left as it is';
  }
}

/** Plugin names, the first few, then "and N more". */
function some(items: ResetItem[]): string {
  const names = items.map((item) => item.name || item.plugin);
  return names.length <= 4 ? names.join(', ') : `${names.slice(0, 3).join(', ')} and ${names.length - 3} more`;
}

export function ResetAll() {
  const client = useClient();
  const owner = useSlotInfo();
  const [started, setStarted] = useSlotState<number | null>(STARTED, null);
  const [open, setOpen] = useState(started !== null);
  const [error, setError] = useState<string | null>(null);
  const [phase, setPhase] = useState<'ready' | 'resetting' | 'done'>(started !== null ? 'resetting' : 'ready');
  const [result, setResult] = useState<Applied[] | null>(null);
  const status = useQuery<Status>('update.status:reset', (c) => c.call<Status>('update.status', {}), { topics: ['update.changed'] });
  // The plan is the status line; the updater answers from its last catalog lookup, so this is cheap.
  const planned = useQuery<Plan>('update.reset_plan', (c) => c.call<Plan>('update.reset_plan', {}), { topics: ['plugin.swapped', 'plugin.unit', 'source.changed', 'update.changed'] });
  const plan = planned.data ?? null;
  const dialog = useRef<HTMLDivElement>(null);

  // Remounted during a reset (this panel's plugin was reinstalled): the outcome is in `recent`.
  useEffect(() => {
    if (started === null || phase !== 'resetting' || !status.data) return;
    const since = (status.data.recent ?? []).filter((entry) => entry.ms >= started && (entry.outcome === 'reset' || entry.outcome === 'failed'));
    if (status.data.state === 'idle' && since.length) {
      setResult(since);
      setPhase('done');
      setStarted(null);
    } else if (Date.now() - started > STALE_MS) {
      setStarted(null);
      setOpen(false);
    }
  }, [started, phase, status.data]);

  const start = () => {
    setOpen(true);
    setPhase('ready');
    setError(null);
    setResult(null);
    void planned.refresh();
  };
  const reset = async () => {
    setPhase('resetting');
    setError(null);
    setStarted(Date.now());
    try {
      const outcome = await client.call<{ plugins: Applied[] }>('update.reset', {});
      setResult(outcome.plugins);
      setPhase('done');
      setStarted(null);
    } catch (cause) {
      setError(errorMessage(cause));
      setPhase('ready');
      setStarted(null);
    } finally {
      void planned.refresh();
    }
  };
  const close = () => {
    if (phase === 'resetting') return;
    setOpen(false);
  };
  const closeRef = useRef(close);
  closeRef.current = close;

  // The dialog pattern: focus moves in, Escape closes, focus returns where it was.
  useEffect(() => {
    if (!open) return;
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
  }, [open]);

  const touched = (plan?.plugins ?? []).filter((item) => item.action === 'install' || item.action === 'source');
  const names = new Map((plan?.plugins ?? []).map((item) => [item.plugin, item.name || item.plugin]));

  // The status line: what differs from the build every plugin should run, and what carries changes.
  const source = plan?.offline ? 'the build this app came with' : 'its official release';
  const differ = (plan?.plugins ?? []).filter((item) => item.action === 'install');
  const ownBuilds = differ.filter((item) => (item.installed ?? '').split('+')[1]?.startsWith('dev.'));
  const changedOnes = (plan?.plugins ?? []).filter((item) => (item.files?.length ?? 0) > 0);
  const outside = (plan?.plugins ?? []).filter((item) => item.action === 'unofficial' || item.action === 'frame');
  const clean = plan !== null && differ.length === 0 && changedOnes.length === 0;
  const state = planned.error && !plan ? 'error' : !plan ? 'loading' : clean ? 'clean' : 'differs';
  const button = (
    <div className="cx-reset-all" data-reset-all={phase} data-reset-state={state} data-differ={differ.length} data-changed={changedOnes.length}>
      <div className="cx-reset-status">
        {state === 'loading' && <span className="cx-settings-line">Comparing every plugin with the build it should run…</span>}
        {state === 'error' && <span className="inline-error">The comparison failed: {planned.error}</span>}
        {state === 'clean' && (
          <>
            <span className="t">Every plugin runs {source}. No changes of yours.</span>
            {outside.length > 0 && <span className="cx-settings-line">Not in it, left as they are: {some(outside)}.</span>}
          </>
        )}
        {state === 'differs' && (
          <>
            {differ.length > 0 && <span className="t">{differ.length === 1 ? '1 plugin differs' : `${differ.length} plugins differ`} from {source}: {some(differ)}.{ownBuilds.length ? ` ${ownBuilds.length === 1 ? 'It is a build of yours' : `${ownBuilds.length} are builds of yours`}, left alone.` : ''}</span>}
            {changedOnes.length > 0 && <span className={differ.length > 0 ? 'cx-settings-line' : 't'}>{changedOnes.length === 1 ? '1 plugin carries' : `${changedOnes.length} plugins carry`} your changes: {some(changedOnes)}.</span>}
            {outside.length > 0 && <span className="cx-settings-line">Not in it, left as they are: {some(outside)}.</span>}
          </>
        )}
        {plan?.offline && <span className="cx-settings-line cx-reset-offline" title={plan.offline}>No official release can be read; the app's own build is the reference.</span>}
      </div>
      <CheckUpdates />
      <button className="detent warn" type="button" data-action="reset-all" disabled={open || state !== 'differs'} onClick={start}
        title={state === 'differs' ? 'Every plugin back to the build it should run, your changes dropped' : 'Nothing to reset'}>Reset all plugins…</button>
    </div>
  );
  if (!open) return button;

  const root = document.getElementById('cc') ?? document.body;
  return (
    <>
      {button}
      {createPortal(
        <div className="sheet-scrim open" data-slot={owner.slot} data-plugin-id={owner.pluginId} data-portal="reset-all">
          <div ref={dialog} className="sheet cx-reset-sheet" role="dialog" aria-modal="true" aria-label="Reset all plugins" tabIndex={-1} data-phase={phase}>
            <div className="sheet-h">
              <span className="ap">Reset all plugins to their official release</span>
              <span className="grow" />
              <button className="detent" type="button" data-action="reset-close" disabled={phase === 'resetting'} onClick={close}>Close</button>
            </div>
            <div className="sheet-b">
              {phase === 'ready' && !plan && !planned.error && <p className="hint">Reading what every plugin should run…</p>}
              {phase === 'ready' && !plan && planned.error && <p className="inline-error" data-reset-error="">{planned.error}</p>}
              {error && <p className="inline-error" data-reset-error="">{error}</p>}
              {phase === 'ready' && plan && (
                <>
                  {plan.offline && <p className="hint" data-reset-offline="">The official releases could not be read ({plan.offline}). The packages this app was built with stand in.</p>}
                  {touched.length === 0
                    ? <p>Every plugin runs its {plan.offline ? "app's build" : 'official release'}, with no changes of yours. Nothing to reset.</p>
                    : <p>Your changes to {touched.length === 1 ? 'this plugin are' : `these ${touched.length} plugins are`} dropped and the package installed. The frame, your projects and sessions stay. This cannot be undone.</p>}
                  <ul className="cx-reset-list">
                    {plan.plugins.map((item) => (
                      <li key={item.plugin} data-plugin={item.plugin} data-reset-action={item.action}>
                        <span className="t">{item.name || item.plugin}</span>
                        <span className={`ap ${item.action === 'install' || item.action === 'source' ? 'live' : 'faint'}`}>{line(item)}</span>
                      </li>
                    ))}
                  </ul>
                  {plan.running && <p className="inline-error" data-reset-running="">A conversation is running and the reset would cut it off. Wait for it to end.</p>}
                </>
              )}
              {phase === 'resetting' && <p className="hint" data-reset-progress="">{status.data?.detail || 'Resetting…'}</p>}
              {phase === 'done' && result && (
                <>
                  <p>{result.length === 0 ? 'Nothing changed.' : result.every((entry) => entry.outcome === 'reset') ? 'Every plugin runs its official release now.' : 'Some plugins could not be reset.'}</p>
                  <ul className="cx-reset-list">
                    {result.map((entry) => (
                      <li key={`${entry.plugin}-${entry.ms}`} data-plugin={entry.plugin} data-reset-outcome={entry.outcome}>
                        <span className="t">{names.get(entry.plugin) ?? entry.plugin} <span className="ap faint num" title={entry.version}>{version(entry.version)}</span></span>
                        <span className={`ap ${entry.outcome === 'failed' ? 'hot' : 'origin'}`}>{entry.outcome === 'failed' ? `failed: ${entry.detail ?? ''}` : 'reset'}</span>
                      </li>
                    ))}
                  </ul>
                </>
              )}
            </div>
            <div className="sheet-f">
              <span className="grow" />
              {phase === 'ready' && touched.length > 0 && (
                <>
                  <button className="detent" type="button" data-action="reset-cancel" onClick={close}>Cancel</button>
                  <button className="detent warn" type="button" data-action="reset-confirm" disabled={plan?.running === true} onClick={() => void reset()}>
                    Reset {touched.length} plugin{touched.length === 1 ? '' : 's'}
                  </button>
                </>
              )}
              {(phase === 'done' || (phase === 'ready' && touched.length === 0)) && (
                <button className="detent send" type="button" data-action="reset-done" onClick={close}>Close</button>
              )}
            </div>
          </div>
        </div>,
        root,
      )}
    </>
  );
}

interface CheckStatus { state: string; detail: string; error?: string | null; waiting?: string[]; pending?: unknown[]; round?: { status: string }[]; restart?: unknown | null }

/** Look at the catalog now (update.check): what applies by itself installs, as the updater's popup
 * shows; here one line says what the check found. */
function CheckUpdates() {
  const client = useClient();
  const [checking, setChecking] = useState(false);
  const [found, setFound] = useState<string | null>(null);
  const check = async () => {
    setChecking(true);
    setFound(null);
    try {
      const status = await client.call<CheckStatus>('update.check', {});
      const busy = (status.round ?? []).filter((item) => item.status !== 'done' && item.status !== 'failed').length;
      setFound(status.error ? `The check failed: ${status.error}`
        : busy > 0 ? `${busy === 1 ? '1 update' : `${busy} updates`} installing`
        : (status.waiting?.length ?? 0) > 0 ? 'Updates wait for the running work to end'
        : (status.pending?.length ?? 0) > 0 ? 'Updates wait for your answer'
        : status.restart ? 'Restart to finish the update'
        : 'Everything is up to date');
    } catch (cause) {
      setFound(errorMessage(cause));
    } finally {
      setChecking(false);
    }
  };
  return (
    <>
      {found && <span className="ap faint" data-update-check={found}>{found}</span>}
      <button className="detent" type="button" data-action="check-updates" disabled={checking} onClick={() => void check()}>{checking ? 'Checking…' : 'Check for updates'}</button>
    </>
  );
}
