/**
 * settings.model@1 — the engine (which installed model plugin serves the model, see engine.tsx)
 * with, inside the serving engine's card, the weights it loads (model.variants: installed or not,
 * with their notes): one line when it offers one, a list with Select or Download when it offers
 * several. Choosing one calls model.select, shows the reload as it runs and its measured time, and
 * reads model.info again afterwards. Below, what is loaded (model.info: id, quantization, context,
 * slots, KV type, speculative decoding, the Metal tensor path).
 */

import { useState, type ReactNode } from 'react';
import { bytes, definePlugin, duration, errorMessage, installLine, plural, useClient, useModelState, useNow, useQuery, ServiceProblem } from '@cortex/sdk';
import { EngineChoice } from './engine';
import './model.css';

interface Variant { id: string; label: string; path: string; installed: boolean; active: boolean; notes?: string; bytes?: number | null; verified?: boolean | null; mtp?: boolean | null }
interface SelectResult { variant: string; previous: string; changed: boolean; reload_ms: number; unload_ms: number; load_ms: number }

function ModelPanel() {
  const client = useClient();
  const state = useModelState();
  const info = state.info;
  const variants = useQuery<Variant[]>('model.variants', async (c) => (await c.call<{ variants: Variant[] }>('model.variants', {}))?.variants ?? [], { topics: ['model.state', 'plugin.swapped'] });
  const [switching, setSwitching] = useState<{ id: string; started: number } | null>(null);
  const [result, setResult] = useState<SelectResult | null>(null);
  const [error, setError] = useState<string | null>(null);
  const now = useNow(250, switching !== null);
  const select = async (id: string) => {
    setSwitching({ id, started: Date.now() });
    setError(null);
    setResult(null);
    try {
      setResult(await client.call<SelectResult>('model.select', { variant: id }));
      await Promise.all([info.refresh(), variants.refresh()]);
    } catch (cause) {
      setError(errorMessage(cause));
    } finally {
      setSwitching(null);
    }
  };
  const m = info.data;
  const list = variants.data ?? [];
  // While the model loads, no variant reports itself active; the one model.info names is the one
  // being loaded.
  const isCurrent = (variant: Variant) => variant.active || (!list.some((v) => v.active) && variant.id === state.variant);
  const statusOf = (variant: Variant) => variant.active ? 'active'
    : isCurrent(variant) ? (state.phase === 'loading' || state.phase === 'starting' ? 'loading' : 'selected')
    : variant.installed ? 'installed' : 'not installed';
  const facts = (variant: Variant): ReactNode => (
    <>
      <span className={`ap ${isCurrent(variant) ? 'live' : 'faint'}`}>{statusOf(variant)}</span>
      {variant.bytes ? <span className="ap faint">{bytes(variant.bytes)}</span> : null}
      {variant.verified === true && <span className="ap origin">verified</span>}
      {variant.mtp === true && <span className="ap faint">MTP draft head</span>}
    </>
  );
  const only = list.length === 1 && isCurrent(list[0]!) ? list[0]! : null;
  const weights = (
    <div className="cx-weights" data-variants={list.length}>
      {variants.loading && !variants.data && !variants.error && <p className="hint" data-variants="loading">Reading the weights this engine offers…</p>}
      {variants.error && !variants.data && <ServiceProblem service="model" state={variants} className="inline-error" />}
      {variants.data && variants.data.length === 0 && <p className="inline-error" data-variants="none">The engine lists no weights (model.variants returned an empty list).</p>}
      {installLine(state.install) && state.install && (
        <div className="cx-model-install" data-install-phase={state.install.phase}>
          <progress max={state.install.total || 1} value={state.install.bytes} />
          <p className="hint">{installLine(state.install)}{state.install.file ? ` · ${state.install.file}` : ''}</p>
        </div>
      )}
      {(state.phase === 'loading' || state.phase === 'starting') && !state.install && <p className="hint" data-model-phase={state.phase}>The model is loading: fitting {state.variant ?? 'it'} into memory.</p>}
      {state.phase === 'failed' && !state.install && <p className="inline-error">The model failed to load: {state.reason}</p>}
      {only ? (
        <div data-variant={only.id} data-status={statusOf(only)}>
          <div className="cx-weight"><span className="ap faint">weights</span><span>{only.label}</span>{facts(only)}</div>
          {only.notes && <p className="cx-settings-line">{only.notes}</p>}
        </div>
      ) : (
        <ul className="cx-variants">
          {list.map((variant) => (
            <li key={variant.id} className={isCurrent(variant) ? 'sel' : undefined} data-variant={variant.id} data-status={statusOf(variant)}>
              <div className="cx-variant">
                <div className="t">{variant.label}{variant.id !== state.configuredVariant ? <span className="ap faint"> · not the default</span> : <span className="ap origin"> · default</span>}</div>
                <div className="r">
                  {facts(variant)}
                  <span className="grow" />
                  <button className="detent" type="button" data-action={variant.installed ? 'select-variant' : 'download-variant'} disabled={variant.active || switching !== null || state.install !== null} onClick={() => void select(variant.id)}
                    title={variant.installed ? `Load ${variant.label} (reloads the weights)` : `Download ${variant.label}${variant.bytes ? ` (${bytes(variant.bytes)})` : ''}, then load it`}>
                    {switching?.id === variant.id ? (state.install ? 'Downloading…' : `Loading… ${duration(now - switching.started)}`) : variant.installed ? 'Select' : 'Download'}
                  </button>
                </div>
                {variant.notes && <p className="cx-settings-line">{variant.notes}</p>}
              </div>
            </li>
          ))}
        </ul>
      )}
      {result && <p className="cx-settings-line" data-reload-ms={result.reload_ms}>{result.changed ? `Switched from ${result.previous} to ${result.variant} in ${duration(result.reload_ms)} (unload ${duration(result.unload_ms)}, load ${duration(result.load_ms)}).` : `${result.variant} was already loaded.`}</p>}
      {error && <p className="inline-error">{error}</p>}
    </div>
  );
  return (
    <div className="cx-settings-panel-body" data-build={__CORTEX_BUILD__} data-panel="model">
      <EngineChoice serving={weights} />
      <h3 className="cx-settings-title">The model</h3>
      {info.error && !m && <ServiceProblem service="model" state={info} className="inline-error" />}
      {state.testVariant && (
        <p className="inline-error cx-model-test" data-test-variant={state.variant ?? ''}>
          Test instance: {state.variant} is loaded, not the configured {state.configuredVariant}. Answers and speeds here are not the real setup's.
        </p>
      )}
      {m && (
        <table className="tbl">
          <tbody>
            <tr><td>Weights</td><td>{m.variant}{state.testVariant && <span className="ap live"> test</span>} <span className="ap faint">{m.family}</span></td></tr>
            <tr><td>Model</td><td className="cx-path">{m.model_id}</td></tr>
            <tr><td>Quantization</td><td>{m.quant} · <span className="num">{bytes(m.file_bytes)}</span></td></tr>
            <tr><td>Context</td><td><span className="num">{m.ctx_total.toLocaleString()}</span> tokens, shared by up to {plural(m.slots, 'resident conversation')} (each may use it all up to native <span className="num">{m.ctx_native.toLocaleString()}</span>)</td></tr>
            <tr><td>KV cache</td><td>{m.kv_type}</td></tr>
            <tr><td>Speculative decoding</td><td>{m.speculative || 'off'}</td></tr>
            <tr><td>Backend</td><td>{m.backend}{m.tensor_api === true ? ' · tensor path on' : m.tensor_api === false ? ' · tensor path off' : ''}</td></tr>
          </tbody>
        </table>
      )}
    </div>
  );
}

export default definePlugin({ slot: 'settings.model@1', component: ModelPanel });
