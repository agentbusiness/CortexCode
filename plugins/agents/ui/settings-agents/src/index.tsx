/**
 * settings.agents@1 — agent defaults: who does agent work (Local, or a frontier CLI found on this
 * computer: Claude Code, Codex; agents.workers / agents.worker.set, docs/FRONTIER-AGENTS.md), the
 * mode new conversations start in (Ask: asks before what leaves the machine or cannot be undone;
 * Auto: never asks, since every local change can be undone), kept in the workbench's durable store;
 * and the scheduler's adaptive parallelism and why it is at its current level.
 */

import { useState } from 'react';
import { definePlugin, errorMessage, useClient, useQuery, useShared, ServiceProblem } from '@cortex/sdk';

interface Scheduler {
  running: string[];
  queued: string[];
  max_parallel: number;
  detail?: { why?: string; bounds?: { min?: number; max?: number }; model?: { slots?: number; room?: number } };
}

interface WorkerInfo {
  id: 'local' | 'claude' | 'openai';
  label: string;
  installed: boolean;
  signed_in: boolean;
  path?: string;
  model?: string;
  effort?: string;
  problem?: string;
}

interface WorkersResult {
  chosen: string;
  active: string;
  fallback?: string;
  workers: WorkerInfo[];
}

function Workers() {
  const client = useClient();
  // Asked fresh when the panel opens: the CLIs are looked for and asked whether they are signed in.
  const workers = useQuery<WorkersResult>('agents.workers', (c) => c.call<WorkersResult>('agents.workers', { fresh: true }), { topics: ['agents.workers.changed'] });
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  if (!workers.data) {
    return workers.error ? <ServiceProblem service="agents" state={workers} /> : <p className="hint">Looking for Claude Code and Codex…</p>;
  }
  const data = workers.data;
  const choose = async (worker: string) => {
    setBusy(true);
    setError(null);
    try {
      await client.call<WorkersResult>('agents.worker.set', { worker });
      await workers.refresh();
    } catch (cause) {
      setError(errorMessage(cause));
    } finally {
      setBusy(false);
    }
  };
  const active = data.workers.find((worker) => worker.id === data.active);
  return (
    <>
      <h3 className="cx-settings-title">Who does agent work</h3>
      <div className="seg-set" role="group" aria-label="Who does agent work" data-chosen={data.chosen} data-active={data.active}>
        {data.workers.map((worker) => (
          <button
            key={worker.id}
            type="button"
            aria-pressed={data.chosen === worker.id}
            disabled={busy || (worker.id !== 'local' && !worker.signed_in)}
            title={worker.problem ? `${worker.label}: ${worker.problem}` : undefined}
            onClick={() => void choose(worker.id)}
          >
            {worker.label}
          </button>
        ))}
      </div>
      {data.active === 'local' ? (
        <p className="cx-settings-line">The local model plans, works and checks: everything stays on this computer.</p>
      ) : (
        <p className="cx-settings-line">
          {active?.label} agents do every change ({active?.model}, {active?.effort} effort), working in the project's folder through CortexCode's tools. Your conversation plans, watches and directs them; it does not change files itself.
        </p>
      )}
      {data.fallback && <p className="hint" data-fallback>{data.fallback}</p>}
      {data.workers
        .filter((worker) => worker.id !== 'local' && worker.problem)
        .map((worker) => (
          <p key={worker.id} className="hint" data-worker={worker.id}>
            {worker.label}: {worker.problem}.
          </p>
        ))}
      {error && <p className="inline-error">{error}</p>}
    </>
  );
}

function AgentDefaults() {
  const [mode, setMode] = useShared<'ask' | 'auto'>('agents.defaultMode', 'auto');
  const scheduler = useQuery<Scheduler>('agents.scheduler', (c) => c.call('agents.scheduler', {}), { topics: ['agents.changed'] });
  return (
    <div className="cx-settings-panel-body" data-build={__CORTEX_BUILD__} data-panel="agents">
      <Workers />
      <h3 className="cx-settings-title">New conversations start in</h3>
      <div className="seg-set" role="group" aria-label="Default mode">
        <button type="button" aria-pressed={mode === 'ask'} onClick={() => setMode('ask')}>Ask</button>
        <button type="button" aria-pressed={mode === 'auto'} onClick={() => setMode('auto')}>Auto</button>
      </div>
      <p className="cx-settings-line">{mode === 'ask' ? 'Ask mode asks once per kind of action that leaves this Mac or cannot be undone: pushing, publishing, deleting outside a snapshot.' : 'Auto mode never asks: every change in the project is in its step log, and can be undone.'}</p>
      <h3 className="cx-settings-title">Parallel agents</h3>
      {scheduler.data ? (
        <>
          <p className="cx-settings-line" data-limit={scheduler.data.max_parallel}>{scheduler.data.max_parallel} {scheduler.data.max_parallel === 1 ? 'agent runs' : 'agents run'} at once now · {scheduler.data.running.length} running, {scheduler.data.queued.length} queued.</p>
          <p className="hint">
            Not a setting: the limit adapts {scheduler.data.detail?.bounds?.max ? `between ${scheduler.data.detail?.bounds?.min ?? 1} and ${scheduler.data.detail.bounds.max}` : `from ${scheduler.data.detail?.bounds?.min ?? 1} up, with no ceiling`}
            {scheduler.data.detail?.model?.room ? `, starting at the ${scheduler.data.detail.model.room} full-size conversations the model's memory holds, minus one for you` : ''}. It steps down when agents measurably slow your own chat or push each other out of memory, and up while more agents raise throughput.
            {scheduler.data.detail?.why && scheduler.data.detail.why !== 'initial limit' ? <> Last change: {scheduler.data.detail.why}.</> : null}
          </p>
        </>
      ) : scheduler.error ? <ServiceProblem service="agents" state={scheduler} /> : <p className="hint">Reading the scheduler…</p>}
    </div>
  );
}

export default definePlugin({ slot: 'settings.agents@1', component: AgentDefaults });
