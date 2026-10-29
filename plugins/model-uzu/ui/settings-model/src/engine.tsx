/**
 * The engine row of the model table: the installed model plugin serving the model on this machine
 * (frame.plugins.providers); every machine runs the one engine made for its memory, so there is
 * nothing to choose. Lending: whether the joined computers may use this model (mesh.lend, a switch
 * shown only where the mesh plugin runs; this computer still uses theirs). This file is the same in
 * every model plugin's settings panel (the panel shown is the serving engine's).
 */

import { useState } from 'react';
import { errorMessage, useClient, useQuery } from '@cortex/sdk';

interface Provider { plugin: string; name: string; serving: boolean; fits_machine?: boolean | null }
interface Providers { memory_bytes: number; providers: Provider[] }
interface MeshStatus { lend?: boolean }

const GIB = 1024 ** 3;

export function EngineRow() {
  const providers = useQuery<Providers>('frame.plugins.providers:model', (c) => c.call<Providers>('frame.plugins.providers', { category: 'model' }), { topics: ['plugin.swapped', 'plugin.unit'] });
  const data = providers.data;
  const engine = data?.providers.find((provider) => provider.serving);
  if (!data || !engine) return null;
  const gb = Math.round(data.memory_bytes / GIB);
  return (
    <tr data-engine={engine.plugin}>
      <td>Engine</td>
      <td>
        {engine.name}{' '}
        {engine.fits_machine === false
          ? <span className="ap hot">not made for this Mac ({gb} GB): restart CortexCode to load the one that is</span>
          : <span className="ap faint">made for this Mac ({gb} GB)</span>}
      </td>
    </tr>
  );
}

export function Lending() {
  const client = useClient();
  const status = useQuery<MeshStatus>('mesh.status', (c) => c.call<MeshStatus>('mesh.status', {}), { topics: ['mesh.changed'] });
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  if (!status.data) return null;
  const lend = status.data.lend !== false;
  const turn = async () => {
    setBusy(true);
    setError(null);
    try {
      await client.call<MeshStatus>('mesh.lend', { lend: !lend });
      await status.refresh();
    } catch (cause) {
      setError(errorMessage(cause));
    } finally {
      setBusy(false);
    }
  };
  return (
    <div className="cx-lend">
      <div className="row" data-lend={lend ? 'on' : 'off'}>
        <div className="grow">
          <div className="nm">Lend this model to my other computers</div>
          <div className="cx-lend-note">Off: they do not see it. This computer still uses theirs.</div>
        </div>
        <button type="button" className="sw" aria-pressed={lend} aria-label="Lend this model to my other computers" disabled={busy} onClick={() => void turn()} />
      </div>
      {error && <p className="inline-error">{error}</p>}
    </div>
  );
}
