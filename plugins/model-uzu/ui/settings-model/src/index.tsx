/**
 * settings.model@1 — the model on this machine, in two rows: the engine serving it (engine.tsx)
 * and the weights it loads (model.variants: the active one, else the one loading, else the
 * default); its download or load as it runs above; nothing to choose. Below, the switch that lends
 * it to the mesh.
 */

import { bytes, definePlugin, installLine, useModelState, useQuery, ServiceProblem } from '@cortex/sdk';
import { EngineRow, Lending } from './engine';
import './model.css';

interface Variant { id: string; label: string; path: string; installed: boolean; active: boolean; notes?: string; bytes?: number | null; verified?: boolean | null; mtp?: boolean | null }

function ModelPanel() {
  const state = useModelState();
  const info = state.info;
  const variants = useQuery<Variant[]>('model.variants', async (c) => (await c.call<{ variants: Variant[] }>('model.variants', {}))?.variants ?? [], { topics: ['model.state', 'plugin.swapped'] });
  const m = info.data;
  const list = variants.data ?? [];
  // The weights this engine loads: the active ones, else the ones being loaded (model.info names
  // them), else the default.
  const current = list.find((v) => v.active) ?? list.find((v) => v.id === state.variant) ?? list.find((v) => v.id === state.configuredVariant) ?? list[0] ?? null;
  const status = !current ? '' : current.active ? 'active'
    : state.install ? 'downloading'
    : state.phase === 'loading' || state.phase === 'starting' ? 'loading'
    : current.installed ? 'installed' : 'not downloaded yet';
  const progress = (
    <>
      {variants.error && !variants.data && <ServiceProblem service="model" state={variants} className="inline-error" />}
      {installLine(state.install) && state.install && (
        <div className="cx-model-install" data-install-phase={state.install.phase}>
          <progress max={state.install.total || 1} value={state.install.bytes} />
          <p className="hint">{installLine(state.install)}{state.install.file ? ` · ${state.install.file}` : ''}</p>
        </div>
      )}
      {(state.phase === 'loading' || state.phase === 'starting') && !state.install && <p className="hint" data-model-phase={state.phase}>The model is loading: fitting {state.variant ?? 'it'} into memory.</p>}
      {state.phase === 'failed' && !state.install && <p className="inline-error">The model failed to load: {state.reason}</p>}
    </>
  );
  return (
    <div className="cx-settings-panel-body" data-build={__CORTEX_BUILD__} data-panel="model">
      <h3 className="cx-settings-title">The model</h3>
      {progress}
      {info.error && !m && !state.install && <ServiceProblem service="model" state={info} className="inline-error" />}
      {state.testVariant && (
        <p className="inline-error cx-model-test" data-test-variant={state.variant ?? ''}>
          Test instance: {state.variant} is loaded, not the configured {state.configuredVariant}. Answers and speeds here are not the real setup's.
        </p>
      )}
      <table className="tbl">
        <tbody>
          <EngineRow />
          {current && (
            <tr data-variant={current.id} data-status={status}>
              <td>Weights</td>
              <td>
                {current.label}{state.testVariant && <span className="ap live"> test</span>}{' '}
                <span className={`ap ${current.active ? 'live' : 'faint'}`}>{status}</span>
                {current.bytes ? <> <span className="ap faint">{bytes(current.bytes)}</span></> : null}
                {current.verified === true && <> <span className="ap origin">verified</span></>}
              </td>
            </tr>
          )}
        </tbody>
      </table>
      <Lending />
    </div>
  );
}

export default definePlugin({ slot: 'settings.model@1', component: ModelPanel });
