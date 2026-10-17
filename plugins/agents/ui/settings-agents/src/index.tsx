/**
 * settings.agents@1 — agent defaults: the mode new conversations start in (Ask: asks before what
 * leaves the machine or cannot be undone; Auto: never asks, since every local change can be
 * undone), kept in the workbench's durable store; and the scheduler's adaptive parallelism and
 * why it is at its current level.
 */

import { definePlugin, useQuery, useShared, ServiceProblem } from '@cortex/sdk';

interface Scheduler {
  running: string[];
  queued: string[];
  max_parallel: number;
  detail?: { why?: string; bounds?: { min?: number; max?: number }; model?: { slots?: number } };
}

function AgentDefaults() {
  const [mode, setMode] = useShared<'ask' | 'auto'>('agents.defaultMode', 'auto');
  const scheduler = useQuery<Scheduler>('agents.scheduler', (c) => c.call('agents.scheduler', {}), { topics: ['agents.changed'] });
  return (
    <div className="cx-settings-panel-body" data-build={__CORTEX_BUILD__} data-panel="agents">
      <h3 className="cx-settings-title">New conversations start in</h3>
      <div className="seg-set" role="group" aria-label="Default mode">
        <button type="button" aria-pressed={mode === 'ask'} onClick={() => setMode('ask')}>Ask</button>
        <button type="button" aria-pressed={mode === 'auto'} onClick={() => setMode('auto')}>Auto</button>
      </div>
      <p className="cx-settings-line">{mode === 'ask' ? 'Ask mode asks once per kind of action that leaves this Mac or cannot be undone: pushing, publishing, deleting outside a snapshot.' : 'Auto mode never asks: every local change is in a copy-on-write workspace or a checkpoint, and can be undone.'}</p>
      <h3 className="cx-settings-title">Parallel agents</h3>
      {scheduler.data ? (
        <>
          <p className="cx-settings-line" data-limit={scheduler.data.max_parallel}>{scheduler.data.max_parallel} {scheduler.data.max_parallel === 1 ? 'agent runs' : 'agents run'} at once now · {scheduler.data.running.length} running, {scheduler.data.queued.length} queued.</p>
          <p className="hint">
            Not a setting: the limit adapts {scheduler.data.detail?.bounds?.max ? `between ${scheduler.data.detail?.bounds?.min ?? 1} and ${scheduler.data.detail.bounds.max}` : `from ${scheduler.data.detail?.bounds?.min ?? 1} up, with no ceiling`}
            {scheduler.data.detail?.model?.slots ? `, starting at the ${scheduler.data.detail.model.slots} conversations the model holds resident, minus one for you` : ''}. It steps down only when agents measurably slow your own chat, and up while more agents raise throughput.
            {scheduler.data.detail?.why && scheduler.data.detail.why !== 'initial limit' ? <> Last change: {scheduler.data.detail.why}.</> : null}
          </p>
        </>
      ) : scheduler.error ? <ServiceProblem service="agents" state={scheduler} /> : <p className="hint">Reading the scheduler…</p>}
    </div>
  );
}

export default definePlugin({ slot: 'settings.agents@1', component: AgentDefaults });
