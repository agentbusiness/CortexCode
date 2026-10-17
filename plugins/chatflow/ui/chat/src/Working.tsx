import { useEffect } from 'react';
import { useNow, useQuery, type FlowState, type ModelState } from '@cortex/sdk';
import { latestLine } from './transcript';

/**
 * The live run: elapsed seconds, what the model is doing now (its
 * progress through the prompt and its writing, from `model.progress`; the conductor's stage when
 * no request is in flight), the decision being written (`chatflow.live`: ROUTE's `work`, DECIDE's
 * `brief`, ...) and the newest line of its reasoning. The steps are the turn's activity line.
 */
export function Working({ since, detail, stage: statusStage, flow, reasoning, progress = '', decision = [], waiting = false, model, projectId }: {
  /** The project whose code graph the run reads (IronGraph's indexing is said while it lasts). */
  projectId?: string | null;
  /** The model's state while the run waits on it (loading, a slot to free). */
  model?: ModelState;
  /** A step waits for the person's Allow or Deny. */
  waiting?: boolean;
  since: number | null;
  detail: string;
  stage: string;
  flow: FlowState | undefined;
  reasoning: string;
  /** The model's progress on the run's current request, as one line (empty: none in flight). */
  progress?: string;
  /** The decision being written: its free-text fields so far. */
  decision?: string[];
}) {
  const now = useNow(1000, true);
  const elapsed = since ? Math.max(0, Math.floor((now - since) / 1000)) : 0;
  const current = waiting ? 'approval' : (flow?.stage ?? statusStage ?? '').toLowerCase();
  const stage = waiting ? STAGE.approval! : progress || (current ? STAGE[current] ?? current : detail || 'Working');
  const thought = latestLine(reasoning);
  const queued = (flow?.pending_steers ?? 0) + (flow?.pending_followups ?? 0);
  return (
    <div className="chat-working" data-working="true">
      <details>
        <summary>
          <span className="ap live">Working</span>
          <span className="ap faint" aria-live="off"> · {elapsed}s elapsed</span>
          {queued > 0 && <span className="ap faint"> · {queued} waiting from you</span>}
          <span className="chat-working-status ap" role="status" data-progress={progress ? 'model' : 'stage'}>{stage}</span>
          {decision.map((line, index) => <span key={index} className="chat-working-latest cx-working-decision" aria-live="off">{line}</span>)}
          {thought && <span className="chat-working-thinking" aria-live="off">{thought}</span>}
        </summary>
        <p className="ap faint">
          {flow
            ? `${flow.pending_steers} steer${flow.pending_steers === 1 ? '' : 's'} and ${flow.pending_followups} follow-up${flow.pending_followups === 1 ? '' : 's'} waiting.`
            : 'The conductor has not reported its stage.'}
        </p>
      </details>
      <ModelLine model={model} />
      <GraphLine projectId={projectId ?? null} />
    </div>
  );
}

/** The conductor's stages (`FlowState.stage`), in the person's terms. */
const STAGE: Record<string, string> = {
  starting: 'Starting',
  context: 'Reading the code graph (IronGraph)',
  route: 'Deciding what kind of request this is',
  explore: 'Looking through the code',
  read: 'Reading the code it needs',
  decide: 'Deciding the next step',
  act: 'Working on the code',
  verify: 'Checking the change',
  reply: 'Writing the answer',
  approval: 'Waiting for you to allow a step',
};

interface GraphStatus { state: string; files: number; symbols: number }

/** IronGraph still indexing the project (or its dependencies' APIs), said while the run waits on it. */
function GraphLine({ projectId }: { projectId: string | null }) {
  const status = useQuery<GraphStatus>(projectId ? `memory.project.status:${projectId}` : null, (c) => c.call<GraphStatus>('memory.project.status', { project_id: projectId }), { topics: ['memory.indexed'] });
  useEffect(() => {
    if (!projectId) return undefined;
    const timer = setInterval(() => void status.refresh(), 2000);
    return () => clearInterval(timer);
    // `refresh` is stable per key.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [projectId]);
  const s = status.data;
  if (!s) return null;
  let text: string | null = null;
  if (s.state === 'indexing') text = `IronGraph is indexing this project: ${s.files.toLocaleString()} files, ${s.symbols.toLocaleString()} symbols so far. The answer starts when the code graph is ready.`;
  if (!text) return null;
  return <p className="ap faint chat-working-graph" data-graph-state={s.state} role="status">{text}</p>;
}

/** Why the run may be slow, said while it waits: the model loading, or every slot busy. */
function ModelLine({ model }: { model?: ModelState }) {
  if (!model) return null;
  let text: string | null = null;
  let tone = 'faint';
  if (model.phase === 'loading' || model.phase === 'starting' || model.phase === 'installing') text = `The model is loading: fitting ${model.variant ?? 'it'} into memory. The answer starts when it is ready.`;
  else if (model.phase === 'failed') {
    text = `The model failed to load${model.reason ? `: ${model.reason}` : ''}.`;
    tone = 'live';
  } else if (model.queued !== null && model.queued > 0 && model.busy !== null && model.busy > 0) {
    // The model's pool is shared: a request waits while the running ones hold it.
    text = `Waiting for the model: ${model.busy} running, ${model.queued} queued.`;
  }
  if (!text) return null;
  return <p className={`ap ${tone} chat-working-model`} data-model-phase={model.phase} role="status">{text}</p>;
}
