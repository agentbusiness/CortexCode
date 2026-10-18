/**
 * The engine: which installed model plugin serves the model (frame.plugins.providers), and the
 * switch to another (frame.plugins.switch). The frame drains the serving engine, starts the other
 * in its place and serves the other's panels; a failed start brings the previous engine back. This
 * file is the same in every model plugin's settings panel (the panel shown is the serving engine's).
 */

import { useState } from 'react';
import { duration, errorMessage, useClient, useNow, useQuery } from '@cortex/sdk';

interface MachineFit { min_memory_bytes?: number | null; max_memory_bytes?: number | null }
interface Provider { plugin: string; name: string; description: string; version: string; enabled: boolean; serving: boolean; machine?: MachineFit | null; fits_machine?: boolean | null }
interface Providers { category: string; serving: string; chosen_by: string; memory_bytes: number; providers: Provider[] }
interface SwitchStep { step: string; ok: boolean; ms: number; detail: string }
interface SwitchResult { ok: boolean; from_plugin: string; to_plugin: string; changed: boolean; error?: string | null; steps: SwitchStep[] }

const GIB = 1024 ** 3;

/** "Macs with 24 GB or less", "Macs with more than 24 GB". */
export function machineRange(fit: MachineFit | null | undefined): string | null {
  if (!fit) return null;
  const max = fit.max_memory_bytes ?? null;
  const min = fit.min_memory_bytes ?? null;
  if (max !== null && min === null) return `Macs with ${Math.round(max / GIB)} GB or less`;
  if (min !== null && max === null) return `Macs with more than ${Math.round((min - 1) / GIB)} GB`;
  if (min !== null && max !== null) return `Macs with ${Math.round(min / GIB)} to ${Math.round(max / GIB)} GB`;
  return null;
}

/** Why the serving engine serves, in the person's words. */
export function chosenBy(providers: Providers): string {
  const gb = Math.round(providers.memory_bytes / GIB);
  switch (providers.chosen_by) {
    case 'user': return 'you chose it';
    case 'machine': return `the default for this Mac (${gb} GB)`;
    default: return 'the first installed';
  }
}

export function EngineChoice({ category = 'model' }: { category?: string }) {
  const client = useClient();
  const providers = useQuery<Providers>(`frame.plugins.providers:${category}`, (c) => c.call<Providers>('frame.plugins.providers', { category }), { topics: ['plugin.swapped', 'plugin.unit'] });
  const [switching, setSwitching] = useState<{ plugin: string; started: number } | null>(null);
  const [error, setError] = useState<string | null>(null);
  const now = useNow(250, switching !== null);
  const data = providers.data;
  if (!data || data.providers.length < 2) return null;
  const choose = async (plugin: string) => {
    setSwitching({ plugin, started: Date.now() });
    setError(null);
    try {
      const result = await client.call<SwitchResult>('frame.plugins.switch', { category, plugin });
      if (!result.ok) setError(`${plugin} did not start; ${result.from_plugin} keeps serving: ${result.error ?? 'see Settings > Plugins'}`);
      await providers.refresh();
    } catch (cause) {
      setError(errorMessage(cause));
    } finally {
      setSwitching(null);
    }
  };
  return (
    <>
      <h3 className="cx-settings-title">Engine</h3>
      <ul className="cx-variants" data-engines={data.providers.length}>
        {data.providers.map((provider) => {
          const range = machineRange(provider.machine);
          const status = provider.serving ? 'serving' : provider.enabled ? 'standby' : 'disabled';
          return (
            <li key={provider.plugin} className={provider.serving ? 'sel' : undefined} data-engine={provider.plugin} data-status={status}>
              <div className="cx-variant">
                <div className="t">
                  {provider.name}
                  {provider.serving && <span className="ap origin"> · {chosenBy(data)}</span>}
                  {!provider.serving && provider.fits_machine === true && <span className="ap faint"> · the default for this Mac</span>}
                </div>
                <div className="r">
                  <span className={`ap ${provider.serving ? 'live' : 'faint'}`}>{status}</span>
                  {range && <span className={`ap ${provider.fits_machine === false ? 'hot' : 'faint'}`}>{range}</span>}
                  <span className="grow" />
                  <button className="detent" type="button" data-action="switch-engine" disabled={provider.serving || !provider.enabled || switching !== null} onClick={() => void choose(provider.plugin)}
                    title={provider.serving ? 'Serving the model now' : `Drain ${data.serving} and load ${provider.name} instead`}>
                    {switching?.plugin === provider.plugin ? `Switching… ${duration(now - switching.started)}` : 'Use'}
                  </button>
                </div>
                {provider.fits_machine === false && !provider.serving && <p className="cx-settings-line">Made for {range}: on this Mac it may not load, and the current engine then keeps serving.</p>}
                {provider.description && <p className="cx-settings-line">{provider.description}</p>}
              </div>
            </li>
          );
        })}
      </ul>
      {error && <p className="inline-error">{error}</p>}
    </>
  );
}
