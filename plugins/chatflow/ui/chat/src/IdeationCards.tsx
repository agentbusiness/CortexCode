import { createContext, useContext, useState } from 'react';
import { Menu, type JournalEntry } from '@cortex/sdk';
import {
  REQUIREMENT_STATUSES,
  diffLines,
  planOf,
  planProgress,
  type Decided,
  type PlanView,
  type Requirement,
  type SpecView,
} from './ideation';
import { str } from './transcript';

/** What the cards need beyond their own entry. */
export interface IdeationContextValue {
  entries: readonly JournalEntry[];
  running: boolean;
  newestPlans: Map<string, number>;
  /** The version of each `spec` entry (its place in append order). */
  specVersions: Map<number, number>;
}
export const IdeationContext = createContext<IdeationContextValue>({ entries: [], running: false, newestPlans: new Map(), specVersions: new Map() });

export interface RequirementInput {
  id?: string;
  text: string;
  status: string;
}

export interface IdeationActions {
  /**
   * `chatflow.spec.update`: the fields given replace the spec's; `requirements` is the whole list;
   * `base_version` is the version edited (a newer spec refuses the edit with `conflict`).
   */
  updateSpec: (change: { title?: string; text?: string; requirements?: RequirementInput[]; base_version?: number }) => Promise<void>;
  /** `chatflow.spec.save`: writes docs/specs/<slug>.md; resolves with the path written. */
  saveSpec: () => Promise<string>;
  /** `chatflow.decision.answer`. */
  answerDecision: (seq: number, accept: boolean) => Promise<void>;
  /** `chatflow.knowledge.answer`: accept with the text as edited, or reject. */
  /** `chatflow.plan.update` (approved | draft). */
  setPlanStatus: (planId: string, status: 'approved' | 'draft') => Promise<void>;
  /** `chatflow.plan.implement`. */
  implementPlan: (planId: string) => Promise<void>;
  /** Put the caret in the composer: the plan is edited by writing to the chat. */
  editInChat: () => void;
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

/**
 * The session's spec, pinned at the top of the conversation: its requirements with their status
 * (changed in place), its decisions, Edit (title, text, requirements) and Save to repository.
 * Without a spec, the accepted decisions alone (the session's decision list).
 */
export function SpecCard({ spec, decisions, actions }: { spec: SpecView | null; decisions: Decided[]; actions: IdeationActions }) {
  const [busy, error, act] = useAct();
  const [editing, setEditing] = useState<{ title: string; text: string; requirements: RequirementInput[]; base: number } | null>(null);
  const [saved, setSaved] = useState<string | null>(null);
  if (!spec) {
    if (decisions.length === 0) return null;
    return (
      <section className="notice context cx-spec" data-kind="decisions" aria-label="Decisions">
        <span className="kindmark" />
        <span className="ap lbl">Decisions · {decisions.length}</span>
        <DecisionList decisions={decisions} />
      </section>
    );
  }
  const live = spec.requirements.filter((requirement) => requirement.status !== 'dropped');
  const met = live.filter((requirement) => requirement.status === 'met').length;
  const asInput = (requirement: Requirement): RequirementInput => ({ id: requirement.id, text: requirement.text, status: requirement.status });
  const setStatus = (id: string, status: string) => act(`status:${id}`, () => actions.updateSpec({
    base_version: spec.version,
    requirements: spec.requirements.filter((requirement) => requirement.status !== 'dropped').map((requirement) => (requirement.id === id ? { ...asInput(requirement), status } : asInput(requirement))),
  }));
  if (editing) {
    const change = (index: number, patch: Partial<RequirementInput>) => setEditing({ ...editing, requirements: editing.requirements.map((item, at) => (at === index ? { ...item, ...patch } : item)) });
    return (
      <section className="notice commitment cx-spec" data-kind="spec" data-editing="true" aria-label="Edit the spec">
        <span className="kindmark" />
        <span className="ap lbl">Edit the spec · version {spec.version}</span>
        <label className="cx-spec-field"><span className="ap faint">Title</span>
          <input type="text" value={editing.title} onChange={(event) => setEditing({ ...editing, title: event.target.value })} aria-label="Spec title" />
        </label>
        <label className="cx-spec-field"><span className="ap faint">Text</span>
          <textarea rows={6} value={editing.text} onChange={(event) => setEditing({ ...editing, text: event.target.value })} aria-label="Spec text" />
        </label>
        <ol className="cx-requirements" aria-label="Requirements">
          {editing.requirements.map((requirement, index) => (
            <li key={requirement.id ?? `new-${index}`} data-req={requirement.id ?? 'new'}>
              <span className="ap">{requirement.id ?? 'new'}</span>
              <input type="text" value={requirement.text} aria-label={`Requirement ${requirement.id ?? 'new'}`} onChange={(event) => change(index, { text: event.target.value })} />
              <button type="button" className="detent warn" aria-label={`Remove ${requirement.id ?? 'the new requirement'}`}
                onClick={() => setEditing({ ...editing, requirements: editing.requirements.filter((_, at) => at !== index) })}>Remove</button>
            </li>
          ))}
        </ol>
        <div className="acts">
          <button type="button" className="detent" onClick={() => setEditing({ ...editing, requirements: [...editing.requirements, { text: '', status: 'open' }] })}>Add requirement</button>
          <span className="grow" />
          <button type="button" className="detent" data-action="cancel" onClick={() => setEditing(null)}>Cancel</button>
          <button type="button" className="detent send" data-action="save" disabled={busy !== null}
            onClick={() => void act('save', async () => {
              await actions.updateSpec({ title: editing.title, text: editing.text, requirements: editing.requirements.filter((item) => item.text.trim() !== ''), base_version: editing.base });
              setEditing(null);
            })}>{busy === 'save' ? 'Saving…' : 'Save'}</button>
        </div>
        {error && <p className="inline-error" role="alert">Not saved: {error}</p>}
      </section>
    );
  }
  return (
    <section className="notice context cx-spec" data-kind="spec" data-version={spec.version} aria-label="Spec">
      <span className="kindmark" />
      <span className="ap lbl">Spec · v{spec.version}{spec.title ? ` · ${spec.title}` : ''} · {met}/{live.length} met</span>
      <details className="cx-spec-text">
        <summary className="ap faint">The request</summary>
        <p className="prose">{spec.text}</p>
      </details>
      <ol className="cx-requirements" aria-label="Requirements">
        {spec.requirements.map((requirement) => (
          <li key={requirement.id} data-req={requirement.id} data-status={requirement.status}>
            <span className="ap">{requirement.id}</span>
            <span className="cx-req-text">{requirement.text}</span>
            {/* The design's own dropdown (SDK Menu), not a native select. */}
            <Menu label="" value={requirement.status} title={`Status of ${requirement.id}`} className="cx-req-status"
              disabled={busy !== null || requirement.status === 'dropped'}
              items={REQUIREMENT_STATUSES.map((status) => ({
                key: status, label: status, selected: status === requirement.status,
                onSelect: () => { if (status !== requirement.status) void setStatus(requirement.id, status); },
              }))} />
          </li>
        ))}
      </ol>
      {spec.decisions.length > 0 && <DecisionList decisions={spec.decisions} />}
      <div className="acts">
        <button type="button" className="detent" data-action="edit"
          onClick={() => setEditing({ title: spec.title, text: spec.text, requirements: spec.requirements.filter((r) => r.status !== 'dropped').map(asInput), base: spec.version })}>Edit</button>
        <button type="button" className="detent" data-action="save-repo" disabled={busy !== null}
          onClick={() => void act('repo', async () => setSaved(await actions.saveSpec()))}>{busy === 'repo' ? 'Writing…' : 'Save to repository'}</button>
        {saved && <span className="ap faint" data-saved={saved}>Written to {saved}</span>}
      </div>
      {error && <p className="inline-error" role="alert">{error}</p>}
    </section>
  );
}

function DecisionList({ decisions }: { decisions: Decided[] }) {
  return (
    <ul className="cx-decisions" aria-label="Decisions">
      {decisions.map((decision, index) => (
        <li key={index}>
          <span className="cx-decided">{decision.decided}</span>
          {decision.because && <span className="ap faint"> because {decision.because}</span>}
          {decision.rejected && <span className="ap faint"> · rejected: {decision.rejected}</span>}
        </li>
      ))}
    </ul>
  );
}

/** A spec version in the transcript: one line saying what changed. */
export function SpecChange({ entry }: { entry: JournalEntry }) {
  const { specVersions } = useContext(IdeationContext);
  const version = specVersions.get(entry.seq) ?? entry.data.version ?? '';
  return <p className="cx-note" data-seq={entry.seq} data-kind="spec">Spec v{String(version)}: {str(entry.data.changes)}</p>;
}

/** A decision the model proposed, with Accept / Reject; answered ones keep the answer. */
export function DecisionCard({ entry, actions }: { entry: JournalEntry; actions: IdeationActions }) {
  const [busy, error, act] = useAct();
  const state = str(entry.data.state) || 'proposed';
  const pending = state === 'proposed';
  return (
    <div className={`notice cx-decision ${pending ? 'commitment' : state === 'accepted' ? 'context' : 'overdue'}`} data-seq={entry.seq} data-state={state}>
      <span className="kindmark" />
      <span className="ap lbl">{pending ? 'Decision proposed' : state === 'accepted' ? 'Decision accepted' : 'Decision rejected'}</span>
      <p className="cx-decided">{str(entry.data.decided)}</p>
      {str(entry.data.because) && <p className="ap faint">Because: {str(entry.data.because)}</p>}
      {str(entry.data.rejected) && <p className="ap faint">Rejected: {str(entry.data.rejected)}</p>}
      {pending && (
        <div className="acts">
          <button type="button" className="detent" data-action="accept" disabled={busy !== null} onClick={() => void act('accept', () => actions.answerDecision(entry.seq, true))}>{busy === 'accept' ? 'Accepting…' : 'Accept'}</button>
          <button type="button" className="detent warn" data-action="reject" disabled={busy !== null} onClick={() => void act('reject', () => actions.answerDecision(entry.seq, false))}>{busy === 'reject' ? 'Rejecting…' : 'Reject'}</button>
        </div>
      )}
      {error && <p className="inline-error" role="alert">Not answered: {error}</p>}
    </div>
  );
}

/**
 * A plan version. The newest version of a plan is the card: its document, the task graph with
 * dependencies, files, checks, requirement coverage and skills, what changed since the version before,
 * Approve / Back to draft, Edit in chat and Implement; once implementing, each task's agent and
 * verification. Older versions are one line.
 */
export function PlanCard({ entry, actions }: { entry: JournalEntry; actions: IdeationActions }) {
  const { entries, running, newestPlans } = useContext(IdeationContext);
  const [busy, error, act] = useAct();
  const plan: PlanView = planOf(entry);
  if (newestPlans.get(plan.planId) !== entry.seq) {
    return <p className="cx-note" data-seq={entry.seq} data-kind="plan" data-superseded="true">Plan v{plan.version}: replaced by a later version.</p>;
  }
  const progress = planProgress(plan, entries);
  const status = progress.status;
  const open = status === 'draft' || status === 'approved';
  const changes = diffLines(plan.diff);
  const titleOf = (n: number) => plan.tasks.find((task) => task.n === n)?.title ?? `#${n}`;
  return (
    <section className={`notice cx-plan ${open ? 'commitment' : 'context'}`} data-seq={entry.seq} data-kind="plan" data-status={status} aria-label="Plan">
      <span className="kindmark" />
      <details className="cx-plan-details">
      <summary className="cx-plan-line">
        <span className="ap lbl">Plan · {plan.tasks.length} task{plan.tasks.length === 1 ? '' : 's'}{status !== 'implementing' ? ` · ${status}` : ''}</span>
        {plan.goal && <span className="cx-plan-goal">{plan.goal}</span>}
      </summary>
      {plan.goal && <p className="prose">{plan.goal}</p>}
      {plan.note && <p className="ap faint">{plan.note}</p>}
      {plan.analysis && <p className="prose"><span className="ap faint">Thought through </span>{plan.analysis}</p>}
      {plan.approach && <p className="prose"><span className="ap faint">Approach </span>{plan.approach}</p>}
      {plan.steps.length > 0 && (
        <ol className="cx-plan-steps" aria-label="Steps">{plan.steps.map((step, index) => <li key={index}>{step}</li>)}</ol>
      )}
      {plan.architecture && <p className="prose"><span className="ap faint">Architecture </span>{plan.architecture}</p>}
      {plan.risks.length > 0 && (
        <ul className="cx-plan-risks" aria-label="Risks">{plan.risks.map((risk, index) => <li key={index}>{risk}</li>)}</ul>
      )}
      {plan.tasks.length > 0 && (
        <ol className="cx-plan-tasks" aria-label="Tasks">
          {plan.tasks.map((task, index) => {
            const state = task.task_id ? progress.tasks.get(task.task_id) : undefined;
            // A plan carried out by the conversation's own loop: the recorded steps say which
            // tasks are done or failed and which one is under way.
            const done = plan.stepsDone.find((item) => item.n === task.n);
            const stepState = done ? (done.outcome === 'done' ? 'done' : 'failed') : index === plan.stepsDone.length && status === 'implementing' && running ? 'current' : 'todo';
            return (
              <li key={task.n} data-task={task.n} data-state={stepState}>
                <span className="cx-task-title">{task.title}</span>
                {done && done.summary && <span className="cx-plan-step-summary">{done.summary}</span>}
                {task.depends_on.length > 0 && <span className="ap faint" data-depends> after {task.depends_on.map(titleOf).join(', ')}</span>}
                {task.brief && <p className="cx-task-brief">{task.brief}</p>}
                {task.touches.length > 0 && <p className="ap faint">touches {task.touches.join(', ')}</p>}
                {task.checks.length > 0 && <p className="ap faint">checks {task.checks.map((check) => `\`${check}\``).join(', ')}</p>}
                {task.covers.length > 0 && <p className="ap faint">covers {task.covers.join(', ')}</p>}
                {task.skills.length > 0 && <p className="ap faint">skills {task.skills.join(', ')}</p>}
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
              </li>
            );
          })}
        </ol>
      )}
      </details>
      {changes.length > 0 && (
        <details className="cx-plan-diff" open>
          <summary className="ap faint">Changed since v{plan.version - 1}</summary>
          <ul>{changes.map((line, index) => <li key={index}>{line}</li>)}</ul>
        </details>
      )}
      {open && (
        <div className="acts">
          {status === 'draft'
            ? <button type="button" className="detent" data-action="approve" disabled={busy !== null} onClick={() => void act('approve', () => actions.setPlanStatus(plan.planId, 'approved'))}>Approve</button>
            : <button type="button" className="detent" data-action="draft" disabled={busy !== null} onClick={() => void act('draft', () => actions.setPlanStatus(plan.planId, 'draft'))}>Back to draft</button>}
          <button type="button" className="detent" data-action="edit" onClick={actions.editInChat} title="Write the change to the chat: the plan gets a new version">Edit in chat</button>
          <button type="button" className="detent send" data-action="implement" disabled={busy !== null || running}
            title={running ? 'Wait for the current run to end' : 'Carry out the plan (approves it)'}
            onClick={() => void act('implement', () => actions.implementPlan(plan.planId))}>{busy === 'implement' ? 'Starting…' : 'Implement'}</button>
        </div>
      )}
      {error && <p className="inline-error" role="alert">{error}</p>}
    </section>
  );
}
