/**
 * Plans as the transcript draws them, read from the journal's `plan` entries (written by the chat
 * flow). Purely structural.
 */

import { kinds, type Json, type JournalEntry } from '@cortex/sdk';
import { str } from './transcript';

export const PLAN = 'plan';
/** A knowledge item the model proposed from the user's message (kept at once). */
export const KNOWLEDGE = 'knowledge';

export interface PlanTask {
  n: number;
  title: string;
  brief: string;
  /** What the conversation already established that the task needs. */
  known: string[];
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
        known: strings(task.known),
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
