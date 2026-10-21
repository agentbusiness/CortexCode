/**
 * The spec, the decision log and plans as the transcript draws them, read from the journal's
 * `spec`, `decision` and `plan` entries (written by the chat flow). Purely structural.
 */

import { kinds, type Json, type JournalEntry } from '@cortex/sdk';
import { str } from './transcript';

export const SPEC = 'spec';
export const PLAN = 'plan';
export const DECISION = 'decision';
/** A knowledge item the model proposed from the user's message (`chatflow.knowledge.answer`). */
export const KNOWLEDGE = 'knowledge';
/** An open design question worked through: its `ideas` phase, then its `options` phase. */
export const IDEATION = 'ideation';
/** A comment on an open plan, sent with `chatflow.plan.revise`. */
export const PLAN_COMMENT = 'plan_comment';
export const REQUIREMENT_STATUSES = ['open', 'met', 'dropped'] as const;

export interface Requirement {
  id: string;
  text: string;
  status: string;
}

export interface Decided {
  decided: string;
  because: string;
  rejected: string;
}

export interface SpecView {
  seq: number;
  version: number;
  title: string;
  text: string;
  requirements: Requirement[];
  decisions: Decided[];
  changes: string;
  by: string;
}

export interface PlanTask {
  n: number;
  title: string;
  brief: string;
  /** The request's rules this task makes true. */
  rules: string[];
  depends_on: number[];
  touches: string[];
  checks: string[];
  covers: string[];
  /** Skills the task chose: only these reach the run that implements it. */
  skills: string[];
  task_id: string | null;
}

export interface PlanDiff {
  changed: string[];
  steps_added: string[];
  steps_removed: string[];
  risks_added: string[];
  risks_removed: string[];
  tasks_added: string[];
  tasks_removed: string[];
  tasks_changed: string[];
}

export interface StepDoneView {
  n: number;
  outcome: string;
  summary: string;
  changedFiles: string[];
}

export interface PlanView {
  seq: number;
  planId: string;
  version: number;
  analysis: string;
  goal: string;
  approach: string;
  steps: string[];
  /** Steps the foreground loop has carried out (a plan made for a decided change), in order. */
  stepsDone: StepDoneView[];
  architecture: string;
  risks: string[];
  tasks: PlanTask[];
  status: string;
  diff: PlanDiff | null;
  note: string;
  rootTask: string | null;
  /** `user` when the user edited this version by hand. */
  by: string;
}

function record(value: Json | undefined): Record<string, Json> {
  return value && typeof value === 'object' && !Array.isArray(value) ? (value as Record<string, Json>) : {};
}

function list(value: Json | undefined): Json[] {
  return Array.isArray(value) ? value : [];
}

function strings(value: Json | undefined): string[] {
  return list(value).filter((item): item is string => typeof item === 'string');
}

function num(value: Json | undefined): number {
  return typeof value === 'number' ? value : 0;
}

function decided(value: Json): Decided {
  const data = record(value);
  return { decided: str(data.decided), because: str(data.because), rejected: str(data.rejected) };
}

/**
 * The version of every `spec` entry by its seq: its place among the `spec` entries in append order
 * (as the chat flow numbers them, so a user edit and a model amendment that land together never
 * share a number; the number stored on the entry may lag until the flow corrects it).
 */
export function specVersions(entries: readonly JournalEntry[]): Map<number, number> {
  const seqs = entries.filter((entry) => entry.kind === SPEC).map((entry) => entry.seq).sort((a, b) => a - b);
  return new Map(seqs.map((seq, index) => [seq, index + 1]));
}

/** A spec entry as drawn; `version` from [`specVersions`] when known, else the stored number. */
export function specOf(entry: JournalEntry, version?: number): SpecView {
  const data = entry.data;
  return {
    seq: entry.seq,
    version: version ?? num(data.version),
    title: str(data.title),
    text: str(data.text),
    requirements: list(data.requirements).map((item) => {
      const requirement = record(item);
      return { id: str(requirement.id), text: str(requirement.text), status: str(requirement.status) || 'open' };
    }),
    decisions: list(data.decisions).map(decided),
    changes: str(data.changes),
    by: str(data.by),
  };
}

/** The current spec: the newest `spec` entry, numbered by its place in append order. */
export function currentSpec(entries: readonly JournalEntry[]): SpecView | null {
  const specs = entries.filter((entry) => entry.kind === SPEC);
  if (specs.length === 0) return null;
  const newest = specs.reduce((a, b) => (b.seq > a.seq ? b : a));
  return specOf(newest, specs.length);
}

/** Accepted decisions (the session's decision list when there is no spec). */
export function acceptedDecisions(entries: readonly JournalEntry[]): Decided[] {
  return entries.filter((entry) => entry.kind === DECISION && entry.data.state === 'accepted').map((entry) => decided(entry.data));
}

export function planOf(entry: JournalEntry): PlanView {
  const data = entry.data;
  const doc = record(data.doc);
  const diff = data.diff ? record(data.diff) : null;
  return {
    seq: entry.seq,
    planId: str(data.plan_id),
    version: num(data.version),
    analysis: str(doc.analysis),
    goal: str(doc.goal),
    approach: str(doc.approach),
    steps: strings(doc.steps),
    stepsDone: list(data.steps_done).map((item) => { const done = record(item); return { n: num(done.n), outcome: str(done.outcome), summary: str(done.summary), changedFiles: strings(done.changed_files) }; }),
    architecture: str(doc.architecture),
    risks: strings(doc.risks),
    tasks: list(data.tasks).map((item) => {
      const task = record(item);
      return {
        n: num(task.n),
        title: str(task.title),
        brief: str(task.brief),
        rules: strings(task.rules),
        depends_on: list(task.depends_on).filter((n): n is number => typeof n === 'number'),
        touches: strings(task.touches),
        checks: strings(task.checks),
        covers: strings(task.covers),
        skills: strings(task.skills),
        task_id: str(task.task_id) || null,
      };
    }),
    status: str(data.status) || 'draft',
    diff: diff
      ? {
        changed: strings(diff.changed),
        steps_added: strings(diff.steps_added),
        steps_removed: strings(diff.steps_removed),
        risks_added: strings(diff.risks_added),
        risks_removed: strings(diff.risks_removed),
        tasks_added: strings(diff.tasks_added),
        tasks_removed: strings(diff.tasks_removed),
        tasks_changed: strings(diff.tasks_changed),
      }
      : null,
    note: str(data.note),
    rootTask: str(data.root_task) || null,
    by: str(data.by),
  };
}

/** Seq of the newest version of each plan. */
export function newestPlans(entries: readonly JournalEntry[]): Map<string, number> {
  const newest = new Map<string, number>();
  for (const entry of entries) {
    if (entry.kind === PLAN) newest.set(str(entry.data.plan_id), entry.seq);
  }
  return newest;
}

/** Whether a version's difference says anything. */
export function diffLines(diff: PlanDiff | null): string[] {
  if (!diff) return [];
  const lines: string[] = [];
  for (const field of diff.changed) lines.push(`${field} changed`);
  for (const step of diff.steps_added) lines.push(`step added: ${step}`);
  for (const step of diff.steps_removed) lines.push(`step removed: ${step}`);
  for (const risk of diff.risks_added) lines.push(`risk added: ${risk}`);
  for (const risk of diff.risks_removed) lines.push(`risk removed: ${risk}`);
  for (const task of diff.tasks_added) lines.push(`task added: ${task}`);
  for (const task of diff.tasks_removed) lines.push(`task removed: ${task}`);
  for (const task of diff.tasks_changed) lines.push(`task changed: ${task}`);
  return lines;
}

export interface TaskProgress {
  agent: string | null;
  session: string | null;
  /** The newest verification of the task: true, false, or null when none yet. */
  verified: boolean | null;
}

/**
 * How a plan's work is going, from the journal: agents started for its tasks, their
 * verifications, and the agents plugin's note that every delegated task is merged (then an
 * implementing plan is done).
 */
export function planProgress(plan: PlanView, entries: readonly JournalEntry[]): { status: string; tasks: Map<string, TaskProgress> } {
  const tasks = new Map<string, TaskProgress>();
  for (const task of plan.tasks) {
    if (task.task_id) tasks.set(task.task_id, { agent: null, session: null, verified: null });
  }
  let status = plan.status;
  for (const entry of entries) {
    const taskId = str(entry.data.task_id);
    const progress = tasks.get(taskId);
    if (entry.kind === kinds.AGENT_SPAWNED && progress) {
      progress.agent = str(entry.data.agent) || null;
      progress.session = str(entry.data.session_id) || null;
    } else if (entry.kind === kinds.VERIFICATION && progress) {
      progress.verified = record(entry.data.report).ok === true;
    } else if (entry.kind === kinds.NOTE && entry.data.purpose === 'delegation_done' && plan.rootTask && taskId === plan.rootTask && status === 'implementing') {
      status = 'done';
    }
  }
  return { status, tasks };
}

export interface PlanCommentView {
  seq: number;
  /** The task it is about (absent: the plan). */
  task: number | null;
  taskTitle: string;
  text: string;
  /** open | sent | removed */
  state: string;
}

/** The comments on a plan that are not taken back, oldest first. */
export function planComments(entries: readonly JournalEntry[], planId: string): PlanCommentView[] {
  return entries
    .filter((entry) => entry.kind === PLAN_COMMENT && str(entry.data.plan_id) === planId && str(entry.data.state) !== 'removed')
    .map((entry) => ({
      seq: entry.seq,
      task: typeof entry.data.task === 'number' ? entry.data.task : null,
      taskTitle: str(entry.data.task_title),
      text: str(entry.data.text),
      state: str(entry.data.state) || 'open',
    }));
}

export interface FoundView {
  kind: string;
  ref: string;
  text: string;
}

export interface IdeaView {
  n: number;
  principle: string;
  name: string;
  mechanism: string;
  found: FoundView[];
}

export interface CritiqueView {
  idea: number;
  verdict: string;
  why: string;
}

export interface ChoiceView {
  letter: string;
  idea: number;
  name: string;
  mechanism: string;
  why: string;
  grounding: FoundView[];
  priorArt: string;
  cost: string;
  risk: string;
  settleBy: string;
}

export interface IdeationView {
  seq: number;
  phase: string;
  question: string;
  constraints: string[];
  criteria: string[];
  ideas: IdeaView[];
  critique: CritiqueView[];
  options: ChoiceView[];
  radical: { name: string; drops: string; buys: string; wins_if: string } | null;
  recommend: string;
  because: string;
  wouldChangeIf: string;
  chosen: string | null;
}

function found(value: Json): FoundView {
  const data = record(value);
  return { kind: str(data.kind), ref: str(data.ref), text: str(data.text) };
}

export function ideationOf(entry: JournalEntry): IdeationView {
  const data = entry.data;
  const radical = data.radical ? record(data.radical) : null;
  return {
    seq: entry.seq,
    phase: str(data.phase),
    question: str(data.question),
    constraints: strings(data.constraints),
    criteria: strings(data.criteria),
    ideas: list(data.ideas).map((item) => {
      const idea = record(item);
      return { n: num(idea.n), principle: str(idea.principle).replace(/_/g, ' '), name: str(idea.name), mechanism: str(idea.mechanism), found: list(idea.found).map(found) };
    }),
    critique: list(data.critique).map((item) => {
      const critique = record(item);
      return { idea: num(critique.idea), verdict: str(critique.verdict), why: str(critique.why) };
    }),
    options: list(data.options).map((item) => {
      const option = record(item);
      return {
        letter: str(option.letter), idea: num(option.idea), name: str(option.name), mechanism: str(option.mechanism), why: str(option.why),
        grounding: list(option.grounding).map(found), priorArt: str(option.prior_art), cost: str(option.cost), risk: str(option.risk), settleBy: str(option.settle_by),
      };
    }),
    radical: radical && str(radical.name) ? { name: str(radical.name), drops: str(radical.drops), buys: str(radical.buys), wins_if: str(radical.wins_if) } : null,
    recommend: str(data.recommend),
    because: str(data.because),
    wouldChangeIf: str(data.would_change_if),
    chosen: str(data.chosen) || null,
  };
}
