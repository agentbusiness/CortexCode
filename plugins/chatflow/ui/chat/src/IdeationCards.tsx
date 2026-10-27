import { createContext, useContext, useState } from 'react';
import { Menu, type JournalEntry } from '@cortex/sdk';
import {
  REQUIREMENT_STATUSES,
  diffLines,
  ideationOf,
  planComments,
  planOf,
  planProgress,
  type ChoiceView,
  type Decided,
  type FoundView,
  type PlanCommentView,
  type PlanTask,
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

/** One task as the plan editor sends it: `depends_on` are positions (1-based) in the edited list. */
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
  approach: string;
  architecture: string;
  risks: string[];
  tasks: PlanTaskInput[];
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
  /** `chatflow.plan.edit`: the user's own version of an open plan (a new version, back to draft). */
  editPlan: (change: PlanEditInput) => Promise<void>;
  /** `chatflow.plan.comment`: a comment on the plan (`task` null) or on one task. */
  commentPlan: (planId: string, task: number | null, text: string) => Promise<void>;
  /** `chatflow.plan.comment` with `remove`: takes an open comment back. */
  removeComment: (planId: string, seq: number) => Promise<void>;
  /** `chatflow.plan.revise`: the open comments go to the planner as one change request. */
  revisePlan: (planId: string) => Promise<void>;
  /** `chatflow.ideation.choose`: a letter or `radical`. */
  chooseOption: (seq: number, option: string) => Promise<void>;
  /** Send a message as a new turn (the options card's Plan it). */
  sendMessage: (text: string) => Promise<void>;
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
          {decision.because && <span className="cx-aside"> <span className="ap faint">because</span> {decision.because}</span>}
          {decision.rejected && <span className="cx-aside"> <span className="ap faint">rejected</span> {decision.rejected}</span>}
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
      {(str(entry.data.because) || str(entry.data.rejected)) && (
        <dl className="cx-option-meta">
          {str(entry.data.because) && <><dt className="ap faint">because</dt><dd>{str(entry.data.because)}</dd></>}
          {str(entry.data.rejected) && <><dt className="ap faint">rejected</dt><dd>{str(entry.data.rejected)}</dd></>}
        </dl>
      )}
      {pending && (
        <div className="acts">
          <button type="button" className="detent" data-action="accept" disabled={busy !== null} title="Adds the decision to the spec; it starts no work" onClick={() => void act('accept', () => actions.answerDecision(entry.seq, true))}>{busy === 'accept' ? 'Accepting…' : 'Accept'}</button>
          <button type="button" className="detent warn" data-action="reject" disabled={busy !== null} onClick={() => void act('reject', () => actions.answerDecision(entry.seq, false))}>{busy === 'reject' ? 'Rejecting…' : 'Reject'}</button>
        </div>
      )}
      {error && <p className="inline-error" role="alert">Not answered: {error}</p>}
    </div>
  );
}

/** Lines of a textarea as a list (blank lines dropped). */
function lines(text: string): string[] {
  return text.split('\n').map((line) => line.trim()).filter(Boolean);
}

/** Comma-separated items as a list. */
function items(text: string): string[] {
  return text.split(',').map((item) => item.trim()).filter(Boolean);
}

/** A task as the editor holds it: `key` stays with the task when tasks move, `after` names keys. */
interface DraftTask {
  key: string;
  n?: number;
  title: string;
  brief: string;
  rules: string;
  touches: string;
  after: string[];
  keep: Pick<PlanTask, 'checks' | 'covers' | 'skills'>;
}

interface Draft {
  goal: string;
  approach: string;
  architecture: string;
  risks: string;
  tasks: DraftTask[];
}

function draftOf(plan: PlanView): Draft {
  const key = (n: number) => `t${n}`;
  return {
    goal: plan.goal,
    approach: plan.approach,
    architecture: plan.architecture,
    risks: plan.risks.join('\n'),
    tasks: plan.tasks.map((task) => ({
      key: key(task.n), n: task.n, title: task.title, brief: task.brief, rules: task.rules.join('\n'), touches: task.touches.join(', '),
      after: task.depends_on.map(key), keep: { checks: task.checks, covers: task.covers, skills: task.skills },
    })),
  };
}

/** The editor's draft as `chatflow.plan.edit` takes it: dependencies as positions of earlier tasks. */
function editOf(plan: PlanView, draft: Draft): PlanEditInput {
  const kept = draft.tasks.filter((task) => task.title.trim() !== '');
  const position = new Map(kept.map((task, index) => [task.key, index + 1]));
  return {
    plan_id: plan.planId,
    base_version: plan.version,
    goal: draft.goal,
    approach: draft.approach,
    architecture: draft.architecture,
    risks: lines(draft.risks),
    tasks: kept.map((task, index) => ({
      n: task.n,
      title: task.title.trim(),
      brief: task.brief.trim(),
      rules: lines(task.rules),
      depends_on: task.after.map((key) => position.get(key) ?? 0).filter((at) => at > 0 && at < index + 1),
      touches: items(task.touches),
      ...task.keep,
    })),
  };
}

/** The plan as a form: its document and every task (title, brief, rules, files, order, dependencies). */
function PlanEditor({ plan, actions, onClose }: { plan: PlanView; actions: IdeationActions; onClose: () => void }) {
  const [busy, error, act] = useAct();
  const [draft, setDraft] = useState<Draft>(() => draftOf(plan));
  const [fresh, setFresh] = useState(0);
  const setTask = (index: number, patch: Partial<DraftTask>) => setDraft({ ...draft, tasks: draft.tasks.map((task, at) => (at === index ? { ...task, ...patch } : task)) });
  const move = (index: number, by: number) => {
    const tasks = [...draft.tasks];
    const [task] = tasks.splice(index, 1);
    tasks.splice(index + by, 0, task!);
    setDraft({ ...draft, tasks });
  };
  const field = (label: string, value: string, change: (value: string) => void, rows = 2) => (
    <label className="cx-spec-field"><span className="ap faint">{label}</span>
      <textarea rows={rows} value={value} aria-label={label} onChange={(event) => change(event.target.value)} />
    </label>
  );
  return (
    <section className="notice commitment cx-plan" data-kind="plan" data-editing="true" aria-label="Edit the plan">
      <span className="kindmark" />
      <span className="ap lbl">Edit the plan · version {plan.version}</span>
      {field('Goal', draft.goal, (goal) => setDraft({ ...draft, goal }))}
      {field('Approach', draft.approach, (approach) => setDraft({ ...draft, approach }), 3)}
      {field('Architecture', draft.architecture, (architecture) => setDraft({ ...draft, architecture }))}
      {field('Risks, one per line', draft.risks, (risks) => setDraft({ ...draft, risks }), 3)}
      <ol className="cx-plan-tasks cx-plan-edit-tasks" aria-label="Tasks">
        {draft.tasks.map((task, index) => {
          const earlier = draft.tasks.slice(0, index);
          return (
            <li key={task.key} data-task={index + 1}>
              <div className="cx-plan-edit-head">
                <span className="ap">{index + 1}</span>
                <input type="text" value={task.title} placeholder="What this task makes happen" aria-label={`Title of task ${index + 1}`} onChange={(event) => setTask(index, { title: event.target.value })} />
                <button type="button" className="detent" disabled={index === 0} aria-label={`Move task ${index + 1} up`} onClick={() => move(index, -1)}>↑</button>
                <button type="button" className="detent" disabled={index === draft.tasks.length - 1} aria-label={`Move task ${index + 1} down`} onClick={() => move(index, 1)}>↓</button>
                <button type="button" className="detent warn" aria-label={`Remove task ${index + 1}`}
                  onClick={() => setDraft({ ...draft, tasks: draft.tasks.filter((_, at) => at !== index).map((other) => ({ ...other, after: other.after.filter((key) => key !== task.key) })) })}>Remove</button>
              </div>
              <textarea rows={3} value={task.brief} placeholder="What must be true when it is done, and the check that shows it" aria-label={`Brief of task ${index + 1}`} onChange={(event) => setTask(index, { brief: event.target.value })} />
              <textarea rows={2} value={task.rules} placeholder="Rules it makes true, one per line" aria-label={`Rules of task ${index + 1}`} onChange={(event) => setTask(index, { rules: event.target.value })} />
              <input type="text" value={task.touches} placeholder="Files or symbols it changes, comma-separated" aria-label={`Files of task ${index + 1}`} onChange={(event) => setTask(index, { touches: event.target.value })} />
              {earlier.length > 0 && (
                <div className="cx-plan-edit-after" role="group" aria-label={`Task ${index + 1} comes after`}>
                  <span className="ap faint">after</span>
                  {earlier.map((other, at) => {
                    const on = task.after.includes(other.key);
                    return (
                      <button key={other.key} type="button" className="detent" aria-pressed={on} data-on={on ? 'true' : 'false'}
                        onClick={() => setTask(index, { after: on ? task.after.filter((key) => key !== other.key) : [...task.after, other.key] })}>{at + 1}. {other.title || 'untitled'}</button>
                    );
                  })}
                </div>
              )}
            </li>
          );
        })}
      </ol>
      <div className="acts">
        <button type="button" className="detent" onClick={() => {
          setFresh(fresh + 1);
          const previous = draft.tasks.at(-1);
          setDraft({ ...draft, tasks: [...draft.tasks, { key: `new${fresh}`, title: '', brief: '', rules: '', touches: '', after: previous ? [previous.key] : [], keep: { checks: [], covers: [], skills: [] } }] });
        }}>Add task</button>
        <span className="grow" />
        <button type="button" className="detent" data-action="cancel" onClick={onClose}>Cancel</button>
        <button type="button" className="detent send" data-action="save" disabled={busy !== null}
          onClick={() => void act('save', async () => { await actions.editPlan(editOf(plan, draft)); onClose(); })}>{busy === 'save' ? 'Saving…' : 'Save as new version'}</button>
      </div>
      {error && <p className="inline-error" role="alert">Not saved: {error}</p>}
    </section>
  );
}

/** A comment box: one line, sent with Enter or the button. */
function CommentBox({ label, onSend, onClose }: { label: string; onSend: (text: string) => Promise<void>; onClose: () => void }) {
  const [busy, error, act] = useAct();
  const [text, setText] = useState('');
  const send = () => void act('send', async () => { if (text.trim()) { await onSend(text.trim()); setText(''); onClose(); } });
  return (
    <div className="cx-plan-comment-box">
      <input type="text" autoFocus value={text} placeholder={label} aria-label={label}
        onChange={(event) => setText(event.target.value)}
        onKeyDown={(event) => { if (event.key === 'Enter') send(); if (event.key === 'Escape') onClose(); }} />
      <button type="button" className="detent" disabled={busy !== null || !text.trim()} onClick={send}>{busy ? 'Adding…' : 'Add'}</button>
      <button type="button" className="detent" onClick={onClose}>Cancel</button>
      {error && <p className="inline-error" role="alert">{error}</p>}
    </div>
  );
}

function CommentList({ comments, onRemove }: { comments: PlanCommentView[]; onRemove: (seq: number) => void }) {
  if (comments.length === 0) return null;
  return (
    <ul className="cx-plan-comments" aria-label="Comments">
      {comments.map((comment) => (
        <li key={comment.seq} data-seq={comment.seq}>
          <span className="cx-plan-comment-text">{comment.text}</span>
          <button type="button" className="detent" aria-label="Remove the comment" onClick={() => onRemove(comment.seq)}>Remove</button>
        </li>
      ))}
    </ul>
  );
}

/**
 * A plan version. The newest version of a plan is the card: its document, the task graph with
 * dependencies, files, checks, requirement coverage and skills, what changed since the version
 * before, comments on the plan and its tasks, Approve / Back to draft, Edit (the plan as a form),
 * Revise with the comments and Implement; once implementing, each task's agent and verification.
 * Older versions are one line.
 */
export function PlanCard({ entry, actions }: { entry: JournalEntry; actions: IdeationActions }) {
  const { entries, running, newestPlans } = useContext(IdeationContext);
  const [busy, error, act] = useAct();
  const [editing, setEditing] = useState(false);
  const [commenting, setCommenting] = useState<number | 'plan' | null>(null);
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
  const comments = open ? planComments(entries, plan.planId).filter((comment) => comment.state === 'open') : [];
  const onTask = (task: PlanTask) => comments.filter((comment) => comment.task === task.n || (comment.task !== null && comment.taskTitle === task.title && !plan.tasks.some((other) => other.n === comment.task)));
  const onPlan = comments.filter((comment) => comment.task === null);
  const remove = (seq: number) => void act(`remove:${seq}`, () => actions.removeComment(plan.planId, seq));
  return (
    <section className={`notice cx-plan ${open ? 'commitment' : 'context'}`} data-seq={entry.seq} data-kind="plan" data-status={status} aria-label="Plan">
      <span className="kindmark" />
      <details className="cx-plan-details" open={open && comments.length > 0 ? true : undefined}>
      <summary className="cx-plan-line">
        <span className="ap lbl">Plan · {plan.tasks.length} task{plan.tasks.length === 1 ? '' : 's'}{status !== 'implementing' ? ` · ${status}` : ''}{plan.by === 'user' ? ' · edited by you' : ''}{comments.length > 0 ? ` · ${comments.length} comment${comments.length === 1 ? '' : 's'}` : ''}</span>
        {plan.goal && <span className="cx-plan-goal">{plan.goal}</span>}
      </summary>
      {plan.goal && <p className="prose">{plan.goal}</p>}
      {plan.note && <p className="cx-aside">{plan.note}</p>}
      {plan.analysis && <p className="prose"><span className="ap faint">Thought through </span>{plan.analysis}</p>}
      {plan.approach && <p className="prose"><span className="ap faint">Approach </span>{plan.approach}</p>}
      {plan.steps.length > 0 && plan.tasks.length === 0 && (
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
                {task.depends_on.length > 0 && <span className="cx-aside" data-depends> <span className="ap faint">after</span> {task.depends_on.map(titleOf).join(', ')}</span>}
                {task.brief && <p className="cx-task-brief">{task.brief}</p>}
                {task.rules.length > 0 && <ul className="cx-plan-rules" aria-label="Rules">{task.rules.map((rule, at) => <li key={at}>{rule}</li>)}</ul>}
                {(task.touches.length > 0 || task.checks.length > 0 || task.covers.length > 0 || task.skills.length > 0) && (
                  <dl className="cx-task-meta">
                    {task.touches.length > 0 && <><dt className="ap faint">touches</dt><dd>{task.touches.map((file, at) => <code key={at}>{file}</code>)}</dd></>}
                    {task.checks.length > 0 && <><dt className="ap faint">checks</dt><dd>{task.checks.map((check, at) => <code key={at}>{check}</code>)}</dd></>}
                    {task.covers.length > 0 && <><dt className="ap faint">covers</dt><dd>{task.covers.join(', ')}</dd></>}
                    {task.skills.length > 0 && <><dt className="ap faint">skills</dt><dd>{task.skills.join(', ')}</dd></>}
                  </dl>
                )}
                <CommentList comments={onTask(task)} onRemove={remove} />
                {open && (commenting === task.n
                  ? <CommentBox label={`Comment on task ${task.n}`} onSend={(text) => actions.commentPlan(plan.planId, task.n, text)} onClose={() => setCommenting(null)} />
                  : <button type="button" className="detent cx-plan-comment-add" data-action="comment-task" onClick={() => setCommenting(task.n)}>Comment</button>)}
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
          <summary className="ap faint">Changed since v{plan.version - 1}{plan.by === 'user' ? ' (your edit)' : ''}</summary>
          <ul>{changes.map((line, index) => <li key={index}>{line}</li>)}</ul>
        </details>
      )}
      {open && <CommentList comments={onPlan} onRemove={remove} />}
      {open && commenting === 'plan' && <CommentBox label="Comment on the plan" onSend={(text) => actions.commentPlan(plan.planId, null, text)} onClose={() => setCommenting(null)} />}
      {open && (
        <div className="acts">
          {status === 'draft'
            ? <button type="button" className="detent" data-action="approve" disabled={busy !== null} onClick={() => void act('approve', () => actions.setPlanStatus(plan.planId, 'approved'))}>Approve</button>
            : <button type="button" className="detent" data-action="draft" disabled={busy !== null} onClick={() => void act('draft', () => actions.setPlanStatus(plan.planId, 'draft'))}>Back to draft</button>}
          <button type="button" className="detent" data-action="edit" onClick={() => setEditing(true)} title="Change the plan yourself: it gets a new version">Edit</button>
          {commenting !== 'plan' && <button type="button" className="detent" data-action="comment" onClick={() => setCommenting('plan')} title="Comment on the plan as a whole; comment on a task under the task">Comment</button>}
          {comments.length > 0 && (
            <button type="button" className="detent" data-action="revise" disabled={busy !== null || running}
              title={running ? 'Wait for the current run to end' : 'The planner writes the next version from your comments'}
              onClick={() => void act('revise', () => actions.revisePlan(plan.planId))}>{busy === 'revise' ? 'Sending…' : `Revise with ${comments.length} comment${comments.length === 1 ? '' : 's'}`}</button>
          )}
          <span className="grow" />
          <button type="button" className="detent send" data-action="implement" disabled={busy !== null || running}
            title={running ? 'Wait for the current run to end' : 'Carry out the plan (approves it)'}
            onClick={() => void act('implement', () => actions.implementPlan(plan.planId))}>{busy === 'implement' ? 'Starting…' : 'Implement'}</button>
        </div>
      )}
      {error && <p className="inline-error" role="alert">{error}</p>}
    </section>
  );
}

/** A finding as a short line: a source opens as a link, code and notes are text. */
function Finding({ found }: { found: FoundView }) {
  const where = found.kind === 'code' ? 'code' : found.kind === 'note' ? 'note' : 'elsewhere';
  const linked = /^https?:\/\//.test(found.ref);
  return (
    <li data-kind={found.kind}>
      <span className="ap faint">{where}</span>
      <span className="cx-finding-body">
        {linked ? <a href={found.ref} target="_blank" rel="noreferrer">{found.text || found.ref}</a> : <code>{found.ref}</code>}
        {!linked && found.text && <span className="cx-finding-text">{found.text}</span>}
      </span>
    </li>
  );
}

/** The ideas an ideation weighed, with what the host found for each: one folded line. */
export function IdeasCard({ entry }: { entry: JournalEntry }) {
  const view = ideationOf(entry);
  return (
    <details className="cx-ideas" data-seq={entry.seq} data-kind="ideas">
      <summary className="ap faint">Weighed {view.ideas.length} ideas{view.question ? ` for: ${view.question}` : ''}</summary>
      {view.constraints.length > 0 && <p className="cx-labelled"><span className="ap faint">Constraints</span>{view.constraints.join('; ')}</p>}
      <ol className="cx-ideas-list">
        {view.ideas.map((idea) => (
          <li key={idea.n} data-idea={idea.n}>
            <span className="cx-idea-name">{idea.name}</span> <span className="cx-idea-principle">{idea.principle}</span>
            <p className="cx-idea-mechanism">{idea.mechanism}</p>
            {idea.found.length > 0 ? <ul className="cx-findings">{idea.found.map((found, at) => <Finding key={at} found={found} />)}</ul> : <p className="cx-faint-line">Nothing found.</p>}
          </li>
        ))}
      </ol>
    </details>
  );
}

function OptionBody({ choice }: { choice: ChoiceView }) {
  return (
    <>
      <p className="cx-option-mechanism">{choice.mechanism}</p>
      {(choice.why || choice.priorArt || choice.cost || choice.risk || choice.settleBy) && (
        <dl className="cx-option-meta">
          {choice.why && <><dt className="ap faint">why</dt><dd>{choice.why}</dd></>}
          {choice.priorArt && <><dt className="ap faint">done before</dt><dd>{choice.priorArt}</dd></>}
          {choice.cost && <><dt className="ap faint">cost</dt><dd>{choice.cost}</dd></>}
          {choice.risk && <><dt className="ap faint">risk</dt><dd>{choice.risk}</dd></>}
          {choice.settleBy && <><dt className="ap faint">settle early</dt><dd>{choice.settleBy}</dd></>}
        </dl>
      )}
      {choice.grounding.length > 0 && <ul className="cx-findings" aria-label="Builds on">{choice.grounding.map((found, at) => <Finding key={at} found={found} />)}</ul>}
    </>
  );
}

/**
 * An ideation's options: the question and criteria, two or three options with their grounding,
 * the radical one, the recommendation and what was set aside. Choose records the decision; once
 * chosen, Plan it asks for a plan of that option.
 */
export function OptionsCard({ entry, actions }: { entry: JournalEntry; actions: IdeationActions }) {
  const { running } = useContext(IdeationContext);
  const [busy, error, act] = useAct();
  const view = ideationOf(entry);
  const chosen = view.chosen;
  const dropped = view.critique.filter((item) => item.verdict === 'drop');
  const choose = (option: string) => void act(`choose:${option}`, () => actions.chooseOption(entry.seq, option));
  const planIt = (name: string, letter: string) => void act('plan', () => actions.sendMessage(letter === 'radical' ? `Plan the radical option: ${name}.` : `Plan option ${letter}: ${name}.`));
  const chosenName = chosen === 'radical' ? view.radical?.name ?? '' : view.options.find((choice) => choice.letter === chosen)?.name ?? '';
  return (
    <section className={`notice cx-options ${chosen ? 'context' : 'commitment'}`} data-seq={entry.seq} data-kind="options" data-chosen={chosen ?? ''} aria-label="Options">
      <span className="kindmark" />
      <span className="ap lbl">Options{chosen ? ` · chose ${chosen === 'radical' ? 'the radical one' : chosen}` : ''}</span>
      {view.question && <p className="cx-plan-goal">{view.question}</p>}
      {view.criteria.length > 0 && (
        <div className="cx-criteria">
          <span className="ap faint">Compared by</span>
          <ul>{view.criteria.map((criterion, at) => <li key={at}>{criterion}</li>)}</ul>
        </div>
      )}
      {view.options.length === 0 && <p className="prose">No idea held up; the reasons are below.</p>}
      <ol className="cx-option-list">
        {view.options.map((choice) => (
          <li key={choice.letter} data-letter={choice.letter} data-recommended={view.recommend === choice.letter ? 'true' : 'false'} data-chosen={chosen === choice.letter ? 'true' : 'false'}>
            <div className="cx-option-head">
              <span className="ap">{choice.letter}</span>
              <span className="cx-option-name">{choice.name}</span>
              {view.recommend === choice.letter && <span className="ap cx-option-badge">recommended</span>}
              {!chosen && <button type="button" className="detent" data-action="choose" disabled={busy !== null} onClick={() => choose(choice.letter)}>{busy === `choose:${choice.letter}` ? 'Choosing…' : 'Choose'}</button>}
            </div>
            <OptionBody choice={choice} />
          </li>
        ))}
        {view.radical && (
          <li data-letter="radical" data-chosen={chosen === 'radical' ? 'true' : 'false'}>
            <div className="cx-option-head">
              <span className="ap">Radical</span>
              <span className="cx-option-name">{view.radical.name}</span>
              {!chosen && <button type="button" className="detent" data-action="choose" disabled={busy !== null} onClick={() => choose('radical')}>{busy === 'choose:radical' ? 'Choosing…' : 'Choose'}</button>}
            </div>
            <dl className="cx-option-meta">
              <dt className="ap faint">drops</dt><dd>{view.radical.drops}</dd>
              <dt className="ap faint">buys</dt><dd>{view.radical.buys}</dd>
              <dt className="ap faint">wins if</dt><dd>{view.radical.wins_if}</dd>
            </dl>
          </li>
        )}
      </ol>
      {view.recommend && (
        <dl className="cx-option-meta cx-recommend">
          <dt className="ap live">recommended {view.recommend}</dt><dd>{view.because}</dd>
          {view.wouldChangeIf && <><dt className="ap faint">would change if</dt><dd>{view.wouldChangeIf}</dd></>}
        </dl>
      )}
      {dropped.length > 0 && (
        <details className="cx-plan-diff">
          <summary className="ap faint">Set aside · {dropped.length}</summary>
          <ul>{dropped.map((item) => <li key={item.idea}><span className="ap faint">idea {item.idea}</span> {item.why}</li>)}</ul>
        </details>
      )}
      {chosen && chosenName && (
        <div className="acts">
          <button type="button" className="detent send" data-action="plan" disabled={busy !== null || running} title={running ? 'Wait for the current run to end' : 'Ask for a plan of the chosen option'}
            onClick={() => planIt(chosenName, chosen === 'radical' ? 'radical' : chosen)}>{busy === 'plan' ? 'Sending…' : 'Plan it'}</button>
        </div>
      )}
      {!chosen && <p className="cx-faint-line">Choose one, or write what to change.</p>}
      {error && <p className="inline-error" role="alert">{error}</p>}
    </section>
  );
}
