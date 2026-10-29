import { createContext, useContext, useState } from 'react';
import type { JournalEntry } from '@cortex/sdk';
import { diffLines, planOf, planProgress, type PlanTask, type PlanView } from './plan';

/** What the plan card needs beyond its own entry. */
export interface PlanContextValue {
  entries: readonly JournalEntry[];
  running: boolean;
  newestPlans: Map<string, number>;
}
export const PlanContext = createContext<PlanContextValue>({ entries: [], running: false, newestPlans: new Map() });

/** One point as the editor sends it: `depends_on` are positions (1-based) in the edited list. */
export interface PlanTaskInput {
  n?: number;
  title: string;
  brief: string;
  rules: string[];
  depends_on: number[];
  touches: string[];
  checks: string[];
  covers: string[];
  skills: string[];
}

export interface PlanEditInput {
  plan_id: string;
  base_version: number;
  goal: string;
  tasks: PlanTaskInput[];
}

export interface PlanActions {
  /** `chatflow.plan.implement`: approves the plan and carries it out. */
  implementPlan: (planId: string) => Promise<void>;
  /** `chatflow.plan.edit`: the user's own version of an open plan (a new version). */
  editPlan: (change: PlanEditInput) => Promise<void>;
  openSession: (sessionId: string) => void;
  openTask: (taskId: string) => void;
}

function errorText(cause: unknown): string {
  return cause instanceof Error ? cause.message : String(cause);
}

/** A button that runs an action, shows that it is running and what went wrong. */
function useAct(): [string | null, string | null, (name: string, run: () => Promise<unknown>) => Promise<void>] {
  const [busy, setBusy] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const act = async (name: string, run: () => Promise<unknown>) => {
    setBusy(name);
    setError(null);
    try {
      await run();
    } catch (cause) {
      setError(errorText(cause));
    } finally {
      setBusy(null);
    }
  };
  return [busy, error, act];
}

/** A point as the editor holds it: `key` stays with the point when points move, `after` names keys. */
interface DraftPoint {
  key: string;
  task: PlanTask | null;
  title: string;
  brief: string;
  after: string[];
}

function draftOf(plan: PlanView): DraftPoint[] {
  const key = (n: number) => `t${n}`;
  return plan.tasks.map((task) => ({ key: key(task.n), task, title: task.title, brief: task.brief, after: task.depends_on.map(key) }));
}

/** The edited points as `chatflow.plan.edit` takes them; what the editor does not show is kept. */
function editOf(plan: PlanView, goal: string, points: DraftPoint[]): PlanEditInput {
  const kept = points.filter((point) => point.title.trim() !== '' || point.brief.trim() !== '');
  const position = new Map(kept.map((point, index) => [point.key, index + 1]));
  return {
    plan_id: plan.planId,
    base_version: plan.version,
    goal,
    tasks: kept.map((point, index) => ({
      n: point.task?.n,
      title: point.title.trim() || point.brief.trim().split('\n')[0]!,
      brief: point.brief.trim(),
      rules: point.task?.rules ?? [],
      depends_on: point.after.map((key) => position.get(key) ?? 0).filter((at) => at > 0 && at < index + 1),
      touches: point.task?.touches ?? [],
      checks: point.task?.checks ?? [],
      covers: point.task?.covers ?? [],
      skills: point.task?.skills ?? [],
    })),
  };
}

/** The plan's points as a form: each point's title and text, their order, add and remove. */
function PlanEditor({ plan, actions, onClose }: { plan: PlanView; actions: PlanActions; onClose: () => void }) {
  const [busy, error, act] = useAct();
  const [goal, setGoal] = useState(plan.goal);
  const [points, setPoints] = useState<DraftPoint[]>(() => draftOf(plan));
  const [fresh, setFresh] = useState(0);
  const setPoint = (index: number, patch: Partial<DraftPoint>) => setPoints(points.map((point, at) => (at === index ? { ...point, ...patch } : point)));
  const move = (index: number, by: number) => {
    const next = [...points];
    const [point] = next.splice(index, 1);
    next.splice(index + by, 0, point!);
    setPoints(next);
  };
  return (
    <section className="notice commitment cx-plan" data-kind="plan" data-editing="true" aria-label="Edit the plan">
      <span className="kindmark" />
      <span className="ap lbl">Edit the plan</span>
      <input type="text" className="cx-plan-edit-goal" value={goal} placeholder="What the work achieves" aria-label="Goal" onChange={(event) => setGoal(event.target.value)} />
      <ol className="cx-plan-points cx-plan-edit-points" aria-label="Points">
        {points.map((point, index) => (
          <li key={point.key} data-task={index + 1}>
            <span className="ap cx-plan-n">{index + 1}</span>
            <div className="cx-plan-point">
              <div className="cx-plan-edit-head">
                <input type="text" value={point.title} placeholder="What this point makes happen" aria-label={`Point ${index + 1}`} onChange={(event) => setPoint(index, { title: event.target.value })} />
                <button type="button" className="detent" disabled={index === 0} aria-label={`Move point ${index + 1} up`} onClick={() => move(index, -1)}>↑</button>
                <button type="button" className="detent" disabled={index === points.length - 1} aria-label={`Move point ${index + 1} down`} onClick={() => move(index, 1)}>↓</button>
                <button type="button" className="detent warn" aria-label={`Remove point ${index + 1}`}
                  onClick={() => setPoints(points.filter((_, at) => at !== index).map((other) => ({ ...other, after: other.after.filter((key) => key !== point.key) })))}>Remove</button>
              </div>
              <textarea rows={2} value={point.brief} placeholder="What must be true when it is done" aria-label={`Text of point ${index + 1}`} onChange={(event) => setPoint(index, { brief: event.target.value })} />
            </div>
          </li>
        ))}
      </ol>
      <div className="acts">
        <button type="button" className="detent" onClick={() => {
          setFresh(fresh + 1);
          const previous = points.at(-1);
          setPoints([...points, { key: `new${fresh}`, task: null, title: '', brief: '', after: previous ? [previous.key] : [] }]);
        }}>Add point</button>
        <span className="grow" />
        <button type="button" className="detent" data-action="cancel" onClick={onClose}>Cancel</button>
        <button type="button" className="detent send" data-action="save" disabled={busy !== null}
          onClick={() => void act('save', async () => { await actions.editPlan(editOf(plan, goal, points)); onClose(); })}>{busy === 'save' ? 'Saving…' : 'Save'}</button>
      </div>
      {error && <p className="inline-error" role="alert">Not saved: {error}</p>}
    </section>
  );
}

/** What a point carries beyond its text (rules, files, checks, skills, order), folded under Details. */
function PointMeta({ task, titleOf }: { task: PlanTask; titleOf: (n: number) => string }) {
  if (task.known.length === 0 && task.rules.length === 0 && task.touches.length === 0 && task.checks.length === 0 && task.skills.length === 0 && task.depends_on.length === 0) return null;
  return (
    <dl className="cx-task-meta">
      {task.depends_on.length > 0 && <><dt className="ap faint">after</dt><dd>{task.depends_on.map(titleOf).join(', ')}</dd></>}
      {task.known.length > 0 && <><dt className="ap faint">known</dt><dd>{task.known.join('; ')}</dd></>}
      {task.rules.length > 0 && <><dt className="ap faint">rules</dt><dd>{task.rules.join('; ')}</dd></>}
      {task.touches.length > 0 && <><dt className="ap faint">touches</dt><dd>{task.touches.map((file, at) => <code key={at}>{file}</code>)}</dd></>}
      {task.checks.length > 0 && <><dt className="ap faint">checks</dt><dd>{task.checks.map((check, at) => <code key={at}>{check}</code>)}</dd></>}
      {task.skills.length > 0 && <><dt className="ap faint">skills</dt><dd>{task.skills.join(', ')}</dd></>}
    </dl>
  );
}

/**
 * A plan version. The newest version of a plan is the card: its goal and its points, one by one;
 * Edit (the points as a form) and Approve, which carries it out; a change in words goes in the
 * chat. Once implementing, each point's state and agent. Older versions are one line.
 */
export function PlanCard({ entry, actions }: { entry: JournalEntry; actions: PlanActions }) {
  const { entries, running, newestPlans } = useContext(PlanContext);
  const [busy, error, act] = useAct();
  const [editing, setEditing] = useState(false);
  const plan: PlanView = planOf(entry);
  if (newestPlans.get(plan.planId) !== entry.seq) {
    return <p className="cx-note" data-seq={entry.seq} data-kind="plan" data-superseded="true">Plan v{plan.version}: replaced by a later version.</p>;
  }
  const progress = planProgress(plan, entries);
  const status = progress.status;
  const open = status === 'draft' || status === 'approved';
  if (editing && open) return <PlanEditor plan={plan} actions={actions} onClose={() => setEditing(false)} />;
  const changes = diffLines(plan.diff);
  const titleOf = (n: number) => plan.tasks.find((task) => task.n === n)?.title ?? `#${n}`;
  const details = plan.analysis || plan.approach || plan.architecture || plan.risks.length > 0 || plan.tasks.some((task) => task.known.length + task.rules.length + task.touches.length + task.checks.length + task.skills.length + task.depends_on.length > 0);
  return (
    <section className={`notice cx-plan ${open ? 'commitment' : 'context'}`} data-seq={entry.seq} data-kind="plan" data-status={status} aria-label="Plan">
      <span className="kindmark" />
      <span className="ap lbl">Plan · {plan.tasks.length} point{plan.tasks.length === 1 ? '' : 's'}{open ? '' : ` · ${status}`}{plan.by === 'user' ? ' · edited by you' : ''}</span>
      {plan.goal && <p className="cx-plan-goal">{plan.goal}</p>}
      {plan.steps.length > 0 && plan.tasks.length === 0 && (
        <ol className="cx-plan-steps" aria-label="Steps">{plan.steps.map((step, index) => <li key={index}>{step}</li>)}</ol>
      )}
      {plan.tasks.length > 0 && (
        <ol className="cx-plan-points" aria-label="Points">
          {plan.tasks.map((task, index) => {
            const state = task.task_id ? progress.tasks.get(task.task_id) : undefined;
            // A plan carried out by the conversation's own loop: the recorded steps say which
            // points are done or failed and which one is under way.
            const done = plan.stepsDone.find((item) => item.n === task.n);
            const stepState = done ? (done.outcome === 'done' ? 'done' : 'failed') : index === plan.stepsDone.length && status === 'implementing' && running ? 'current' : 'todo';
            return (
              <li key={task.n} data-task={task.n} data-state={stepState}>
                <span className="ap cx-plan-n">{stepState === 'done' ? '✓' : stepState === 'failed' ? '✗' : index + 1}</span>
                <div className="cx-plan-point">
                  <span className="cx-task-title">{task.title}</span>
                  {task.brief && <p className="cx-task-brief">{task.brief}</p>}
                  {done && done.summary && <span className="cx-plan-step-summary">{done.summary}</span>}
                  {task.task_id && (
                    <div className="cx-task-progress">
                      <button type="button" className="stream-row" onClick={() => actions.openTask(task.task_id!)}>
                        <span className="who">Task</span>
                        <span className="subject">{state?.verified === true ? 'verified' : state?.verified === false ? 'checks failing' : state?.agent ? `@${state.agent} working` : 'waiting'}</span>
                        <span className="row-end">board</span>
                      </button>
                      {state?.session && <button type="button" className="detent" onClick={() => actions.openSession(state.session!)}>@{state.agent}</button>}
                    </div>
                  )}
                </div>
              </li>
            );
          })}
        </ol>
      )}
      {plan.note && <p className="cx-aside">{plan.note}</p>}
      {details && (
        <details className="cx-plan-diff">
          <summary className="ap faint">Details</summary>
          {plan.approach && <p className="prose"><span className="ap faint">Approach </span>{plan.approach}</p>}
          {plan.architecture && <p className="prose"><span className="ap faint">Architecture </span>{plan.architecture}</p>}
          {plan.risks.length > 0 && <ul className="cx-plan-risks" aria-label="Risks">{plan.risks.map((risk, index) => <li key={index}>{risk}</li>)}</ul>}
          {plan.tasks.map((task, index) => (
            <div key={task.n} className="cx-plan-point-meta">
              <span className="cx-task-title">{index + 1}. {task.title}</span>
              <PointMeta task={task} titleOf={titleOf} />
            </div>
          ))}
          {plan.analysis && <p className="prose"><span className="ap faint">Thought through </span>{plan.analysis}</p>}
        </details>
      )}
      {changes.length > 0 && (
        <details className="cx-plan-diff">
          <summary className="ap faint">Changed since v{plan.version - 1}{plan.by === 'user' ? ' (your edit)' : ''}</summary>
          <ul>{changes.map((line, index) => <li key={index}>{line}</li>)}</ul>
        </details>
      )}
      {open && (
        <>
          <div className="acts">
            <button type="button" className="detent" data-action="edit" onClick={() => setEditing(true)} title="Change the points yourself">Edit</button>
            <span className="grow" />
            <button type="button" className="detent send" data-action="implement" disabled={busy !== null || running}
              title={running ? 'Wait for the current run to end' : 'Approve the plan and carry it out'}
              onClick={() => void act('implement', () => actions.implementPlan(plan.planId))}>{busy === 'implement' ? 'Starting…' : 'Approve'}</button>
          </div>
          <p className="cx-faint-line">Or write in the chat what to change.</p>
        </>
      )}
      {error && <p className="inline-error" role="alert">{error}</p>}
    </section>
  );
}
