/**
 * settings.plugins@1 — every installed plugin on one line (`units` of frame.plugins.list): its
 * name, version, a problem when it has one, and a switch (frame.plugins.disable / enable, the whole
 * plugin); Use official (update.reset {plugins}) on a plugin whose data you changed. Clicking the
 * name opens its details: Use previous version (frame.plugins.rollback
 * {plugin}, the version its last install replaced) and its parts: the backend service (state, pid, version, known good, restarts) with Restart
 * (frame.plugins.swap: the same version, blue/green), showing the swap's steps and timings; its UI
 * packages (slot, version, hash) with Reload; its shell modules. Live from plugin.state,
 * plugin.swapped and plugin.unit. At the top, Reset all plugins (ResetAll.tsx): every plugin back
 * to its official release, after asking.
 */

import { Fragment, useState, type ReactNode } from 'react';
import { definePlugin, duration, errorMessage, useClient, useHost, useQuery, useUiPlugins, type PluginInfo } from '@cortex/sdk';
import { ResetAll, version } from './ResetAll';
import './plugins.css';

interface SwapStep { step: string; ok: boolean; ms: number; detail?: string }
interface SwapResult { ok: boolean; category: string; from_version?: string; to_version?: string; steps: SwapStep[]; error?: string | null; rolled_back: boolean }
type Info = PluginInfo & { build?: string; known_good?: boolean; transport?: string };
/** A plugin as one unit (frame.plugins.list `units`); `served` marks a category or UI package no
 * installed plugin claims (served by the app over the WebSocket), which cannot be toggled. */
interface Unit { id: string; name?: string; version?: string; enabled: boolean; ui?: string[]; shell?: string[]; category?: string | null; previous_version?: string | null; served?: boolean }
interface PackageSwap { ok: boolean; plugin: string; from_version?: string; to_version?: string; error?: string | null; steps: SwapStep[] }
interface UpdateStatus { pending?: { plugin: string; version: string; files?: string[] }[]; failed?: { plugin: string; version: string; error?: string }[] }

/** Where a package version came from, by its build tag: `+dev.` a build of yours, `+b` the app's
 * build, none a release. */
function origin(full: string | undefined): string {
  if (!full) return '';
  const tag = full.split('+')[1] ?? '';
  return tag.startsWith('dev.') ? 'your build' : tag.startsWith('b') ? "the app's build" : 'release';
}

/** A full-width row under a part: a swap's or rollback's outcome and its steps. */
function Outcome({ attr, text, ok, steps }: { attr: Record<string, string>; text: string; ok: boolean; steps: SwapStep[] }) {
  return (
    <tr className="cx-swap-row" {...attr}>
      <td colSpan={3}>
        <div className="cx-swap" data-swap-ok={String(ok)}>
          <span className={`ap ${ok ? 'origin' : 'live'}`}>{text}</span>
          <span className="cx-steps">{steps.map((step, index) => <span key={`${step.step}-${index}`} className={`ap ${step.ok ? 'faint' : 'live'}`} title={step.detail}>{step.step} {duration(step.ms)}</span>)}</span>
        </div>
      </td>
    </tr>
  );
}

function PluginsPanel() {
  const client = useClient();
  const host = useHost();
  const ui = useUiPlugins();
  const listed = useQuery<{ plugins: Info[]; units: Unit[] | null }>('frame.plugins.list', async (c) => {
    const result = await c.call<Info[] | { plugins: Info[]; units?: Unit[] }>('frame.plugins.list', {});
    return Array.isArray(result) ? { plugins: result, units: null } : { plugins: result?.plugins ?? [], units: result?.units ?? null };
  }, { topics: ['plugin.state', 'plugin.swapped', 'plugin.unit'] });
  // What the updater knows per plugin: a newer version waiting for an answer, or one that could not start.
  const updates = useQuery<UpdateStatus>('update.status:plugins', (c) => c.call<UpdateStatus>('update.status', {}), { topics: ['update.changed'] });
  const pendingOf = new Map((updates.data?.pending ?? []).map((pending) => [pending.plugin, pending]));
  const failedOf = new Map((updates.data?.failed ?? []).map((failed) => [failed.plugin, failed]));
  const [busy, setBusy] = useState<string | null>(null);
  const [results, setResults] = useState<Record<string, SwapResult | string>>({});
  const [toggleErrors, setToggleErrors] = useState<Record<string, string>>({});
  const [unitResults, setUnitResults] = useState<Record<string, PackageSwap | string>>({});
  const [open, setOpen] = useState<string | null>(null);
  // Plugins whose data in the live folder you changed (update.reset_plan): those get Use official.
  const planned = useQuery<{ plugins: { plugin: string; files?: string[] }[] }>('update.reset_plan', (c) => c.call('update.reset_plan', {}), { topics: ['plugin.swapped', 'plugin.unit', 'source.changed', 'update.changed'] });
  const changedOf = new Map((planned.data?.plugins ?? []).filter((item) => (item.files?.length ?? 0) > 0).map((item) => [item.plugin, item.files!.length]));

  const restart = async (category: string) => {
    setBusy(category);
    try {
      const result = await client.call<SwapResult>('frame.plugins.swap', { category });
      setResults((all) => ({ ...all, [category]: result }));
      await listed.refresh();
    } catch (cause) {
      setResults((all) => ({ ...all, [category]: errorMessage(cause) }));
    } finally {
      setBusy(null);
    }
  };
  const toggle = async (name: string, enable: boolean) => {
    setBusy(name);
    setToggleErrors(({ [name]: _gone, ...rest }) => rest);
    try {
      await client.call(enable ? 'frame.plugins.enable' : 'frame.plugins.disable', { plugin: name });
      await listed.refresh();
    } catch (cause) {
      setToggleErrors((all) => ({ ...all, [name]: errorMessage(cause) }));
    } finally {
      setBusy(null);
    }
  };
  const useOfficial = async (name: string) => {
    if (!window.confirm(`Put ${name} back to its official release? Your ${changedOf.get(name) === 1 ? 'change' : `${changedOf.get(name)} changes`} to its data are dropped.`)) return;
    setBusy(name);
    setToggleErrors(({ [name]: _gone, ...rest }) => rest);
    try {
      await client.call('update.reset', { plugins: [name] });
      await Promise.all([listed.refresh(), planned.refresh()]);
    } catch (cause) {
      setToggleErrors((all) => ({ ...all, [name]: errorMessage(cause) }));
    } finally {
      setBusy(null);
    }
  };
  const rollbackUnit = async (name: string) => {
    setBusy(name);
    try {
      const result = await client.call<PackageSwap>('frame.plugins.rollback', { plugin: name });
      setUnitResults((all) => ({ ...all, [name]: result }));
      await listed.refresh();
    } catch (cause) {
      setUnitResults((all) => ({ ...all, [name]: errorMessage(cause) }));
    } finally {
      setBusy(null);
    }
  };

  // A plugin, to the person, is one installed package: its backend, UI packages and shell modules.
  // The frame lists every plugin (`units`); a running service or UI package no unit claims is
  // grouped under its own id (UI package ids are `<plugin>.<package>`).
  const services = listed.data?.plugins ?? [];
  const units = new Map<string, Unit>((listed.data?.units ?? []).map((unit) => [unit.id, { ...unit }]));
  const claimedCategories = new Set([...units.values()].map((unit) => unit.category).filter(Boolean));
  // A service no unit declares as its backend: one served by a plugin's app-shell module over the
  // WebSocket (the plugin of the same id), else its own entry.
  for (const service of services) {
    if (claimedCategories.has(service.category)) continue;
    claimedCategories.add(service.category);
    const owner = units.get(service.category);
    units.set(service.category, owner && !owner.category
      ? { ...owner, category: service.category }
      : { id: service.category, version: service.version, enabled: true, served: true, category: service.category, ui: [] });
  }
  const claimedUi = new Set([...units.values()].flatMap((unit) => unit.ui ?? []));
  for (const plugin of ui) {
    if (claimedUi.has(plugin.id)) continue;
    const name = plugin.id.split('.')[0]!;
    const unit = units.get(name) ?? { id: name, enabled: true, served: true, ui: [] };
    units.set(name, { ...unit, ui: [...(unit.ui ?? []), plugin.id] });
  }
  const uiById = new Map(ui.map((plugin) => [plugin.id, plugin]));

  const serviceRows = (unit: Unit): ReactNode => {
    // The instance serving the category first, then standby ones.
    const instances = services.filter((service) => service.category === unit.category).sort((a, b) => Number(b.state === 'active') - Number(a.state === 'active'));
    return instances.map((service) => {
      const process = (service.transport ?? (service.exec === 'websocket' ? 'websocket' : 'process')) !== 'websocket';
      const outcome = results[service.category];
      return (
        <Fragment key={service.instance}>
          <tr className={`cx-part${outcome ? ' cx-has-swap' : ''}`} data-category={service.category} data-state={service.state} data-pid={service.pid}>
            <td><span className="cx-part-kind">service</span> {service.category}</td>
            <td>
              <span className={service.state === 'active' ? 'cx-on' : 'cx-off'}>{service.state}</span>
              <span className="ap faint cx-sub">
                {process ? <>pid <span className="num">{service.pid || '—'}</span></> : 'served by the app'}

                {service.known_good ? ' · known good' : null}
                {process ? ` · ${service.restarts ?? 0} restart${service.restarts === 1 ? '' : 's'}` : null}
              </span>
            </td>
            <td>
              {process && service.state === 'active' ? (
                <div className="cx-plugin-acts">
                  <button className="detent" type="button" disabled={busy !== null} data-action="restart" onClick={() => void restart(service.category)}>{busy === service.category ? 'Restarting…' : 'Restart'}</button>
                </div>
              ) : null}
            </td>
          </tr>
          {outcome && (typeof outcome === 'string'
            ? <Outcome attr={{ 'data-swap-for': service.category }} text={outcome} ok={false} steps={[]} />
            : <Outcome attr={{ 'data-swap-for': service.category }} ok={outcome.ok} steps={outcome.steps}
                text={outcome.ok ? `restarted ${outcome.to_version ?? ''}` : outcome.rolled_back ? `rolled back: ${outcome.error ?? ''}` : `failed: ${outcome.error ?? ''}`} />)}
        </Fragment>
      );
    });
  };

  return (
    <div className="cx-settings-panel-body" data-build={__CORTEX_BUILD__} data-panel="plugins">
      <h3 className="cx-settings-title">Plugins</h3>
      <p className="hint cx-settings-lead">Plugins update themselves. Click a name for its details.</p>
      <ResetAll />
      {listed.error && !listed.data && <p className="inline-error">The frame did not list its plugins: {listed.error}</p>}
      <table className="tbl cx-plugins">
        <colgroup><col className="cx-col-name" /><col className="cx-col-facts" /><col className="cx-col-acts" /></colgroup>
        <tbody>
          {[...units.values()].sort((a, b) => a.id.localeCompare(b.id)).map((unit) => {
            const name = unit.id;
            const off = !unit.enabled;
            const result = unitResults[name];
            const toggleable = !unit.served;
            const shown = open === name;
            // The one fact worth a line of its own: the plugin is off, or its service is not running.
            const running = services.filter((service) => service.category === unit.category);
            const problem = off ? 'off'
              : unit.category && running.length > 0 && !running.some((service) => service.state === 'active') ? running[0]!.state
              : null;
            return (
              <Fragment key={name}>
                <tr className={`cx-unit${shown ? ' cx-open' : ''}`} data-vertical={name} data-enabled={off ? 'false' : 'true'}>
                  <td>
                    <button className="cx-plugin-name" type="button" aria-expanded={shown} data-action="details" onClick={() => setOpen(shown ? null : name)}>{unit.name || name}</button>
                  </td>
                  <td>
                    <span className="ap faint num" title={unit.version ? `${unit.version} · ${origin(unit.version)}` : undefined}>{unit.version ? version(unit.version) : '—'}</span>
                    {problem && <span className="ap live"> {problem}</span>}
                  </td>
                  <td>
                    {changedOf.has(name) && (
                      <button className="detent cx-official" type="button" disabled={busy !== null} data-action="use-official" title={`Drop your ${changedOf.get(name) === 1 ? 'change' : `${changedOf.get(name)} changes`} to its data and install the official release`} onClick={() => void useOfficial(name)}>{busy === name ? 'Resetting…' : 'Use official'}</button>
                    )}
                    {toggleable || off ? (
                      <button type="button" className="sw" aria-pressed={!off} aria-label={`${unit.name || name} on`} disabled={busy !== null} data-action={off ? 'enable' : 'disable'} onClick={() => void toggle(name, off)} />
                    ) : null}
                  </td>
                </tr>
                {failedOf.has(name) && (
                  <tr className="cx-swap-row" data-update-failed-for={name}>
                    <td colSpan={3}><p className="inline-error" data-update-failed={name}>Version {version(failedOf.get(name)!.version)} could not start, so this version keeps running: {failedOf.get(name)!.error}</p></td>
                  </tr>
                )}
                {pendingOf.has(name) && (
                  <tr className="cx-swap-row" data-update-pending-for={name}>
                    <td colSpan={3}><p className="hint" data-update-pending={name}>Version {version(pendingOf.get(name)!.version)} is ready. You changed this plugin ({pendingOf.get(name)!.files?.length ?? 0} files), so the popup at the bottom right asks whether to use it.</p></td>
                  </tr>
                )}
                {toggleErrors[name] && (
                  <tr className="cx-swap-row" data-toggle-error-for={name}>
                    <td colSpan={3}><p className="inline-error" data-toggle-error={name}>{name}: {toggleErrors[name]}</p></td>
                  </tr>
                )}
                {result && (typeof result === 'string'
                  ? <Outcome attr={{ 'data-unit-swap-for': name }} text={result} ok={false} steps={[]} />
                  : <Outcome attr={{ 'data-unit-swap-for': name }} ok={result.ok} steps={result.steps}
                      text={result.ok ? `now ${version(result.to_version ?? '')} (was ${version(result.from_version ?? '')})` : `failed: ${result.error ?? ''}`} />)}
                {shown && unit.previous_version && !off ? (
                  <tr className="cx-part" data-previous={unit.previous_version}>
                    <td><span className="cx-part-kind">version</span> {origin(unit.version)}</td>
                    <td><span className="ap faint">before: <span className="num">{version(unit.previous_version)}</span></span></td>
                    <td><div className="cx-plugin-acts"><button className="detent" type="button" disabled={busy !== null} data-action="rollback-plugin" onClick={() => void rollbackUnit(name)}>Use previous version</button></div></td>
                  </tr>
                ) : null}
                {shown && !off && unit.category ? serviceRows(unit) : null}
                {shown && off && unit.category ? (
                  <tr className="cx-part" data-category={unit.category} data-state="disabled"><td><span className="cx-part-kind">service</span> {unit.category}</td><td colSpan={2}><span className="ap faint">stopped</span></td></tr>
                ) : null}
                {shown && (unit.ui ?? []).map((id) => {
                  const loaded = uiById.get(id);
                  return (
                    <tr key={id} className="cx-part" data-ui-plugin={id}>
                      <td><span className="cx-part-kind">ui</span> {id.startsWith(`${name}.`) ? id.slice(name.length + 1) : id}</td>
                      <td>
                        {loaded ? <>
                          <span className="ap faint">{loaded.slot}</span>
                          <span className="ap faint cx-sub" title={`${loaded.version} · ${loaded.hash}`}><span className="num">{loaded.version}</span></span>
                        </> : <span className="ap faint">not loaded</span>}
                      </td>
                      <td>{loaded ? <div className="cx-plugin-acts"><button className="detent" type="button" data-action="reload" onClick={() => host.reloadPlugin(id)}>Reload</button></div> : null}</td>
                    </tr>
                  );
                })}
                {shown && (unit.shell ?? []).map((id) => (
                  <tr key={`shell-${id}`} className="cx-part" data-shell-module={id}>
                    <td><span className="cx-part-kind">app shell</span> {id}</td>
                    <td colSpan={2}><span className="ap faint">runs in the CortexCode app</span></td>
                  </tr>
                ))}
              </Fragment>
            );
          })}
        </tbody>
      </table>
    </div>
  );
}

export default definePlugin({ slot: 'settings.plugins@1', component: PluginsPanel });
