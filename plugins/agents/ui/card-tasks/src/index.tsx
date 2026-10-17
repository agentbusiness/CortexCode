/**
 * card.tasks@1 — the project's planned tasks, one line each: its mark, title and state in words
 * (pending, in progress, verifying, done, failed, or blocked when it waits for unfinished
 * prerequisites, which its tooltip names). Only the tasks that are work are listed, flat: a task
 * with sub-tasks is just their summary and is left out. A line opens the task on the board (brief,
 * plan, contract, last verification). No tasks: nothing.
 */

import { definePlugin, setShared, showPane, useQuery, useSelection, type Task, ServiceProblem } from '@cortex/sdk';
import './tasks.css';

export interface Row { task: Task; waitsFor: string[] }

/** The work tasks (those without sub-tasks), newest plan first and each plan's tasks in order;
 * unmet prerequisites per task. */
export function taskRows(tasks: readonly Task[]): Row[] {
  const byId = new Map(tasks.map((task) => [task.id, task]));
  const summaries = new Set(tasks.map((task) => task.parent_id).filter((id): id is string => Boolean(id && byId.has(id))));
  const planned = (task: Task) => (task.parent_id && byId.get(task.parent_id)?.created_ms) || task.created_ms;
  return tasks
    .filter((task) => !summaries.has(task.id))
    .sort((a, b) => planned(b) - planned(a) || a.created_ms - b.created_ms)
    .map((task) => ({ task, waitsFor: (task.depends_on ?? []).filter((id) => byId.get(id)?.status !== 'done').map((id) => byId.get(id)?.title ?? id) }));
}

/** The most lines the card shows; the rest are on the board. */
const SHOWN = 8;

function TasksSection() {
  const { projectId } = useSelection();
  const tasks = useQuery<Task[]>(projectId ? `memory.task.list:${projectId}` : null, async (c) => (await c.call<{ tasks: Task[] }>('memory.task.list', { project_id: projectId }))?.tasks ?? [], { topics: ['memory.task.changed', 'agents.changed'] });
  const list = tasks.data ?? [];
  const problem = tasks.error && !tasks.data;
  if (!problem && list.length === 0) return null;
  const rows = taskRows(list);
  const open = (id: string | null) => {
    if (id) setShared('board.focus', id);
    setShared('board.view', 'tasks');
    showPane('board');
  };
  return (
    <section className="cx-cs" data-section="tasks" data-build={__CORTEX_BUILD__}>
      <div className="cx-cs-l">
        <span className="ap">Tasks</span>
      </div>
      <div className="cx-cs-b">
        {problem && <ServiceProblem service="memory" state={tasks} className="hint" />}
        {rows.slice(0, SHOWN).map((row) => {
          const blocked = row.waitsFor.length > 0 && row.task.status !== 'done';
          const state = blocked ? 'blocked' : row.task.status;
          return (
            <button key={row.task.id} type="button" className="cx-cs-row" data-task={row.task.id} data-status={row.task.status} data-blocked={blocked ? 'true' : 'false'}
              onClick={() => open(row.task.id)}
              title={[row.task.title, row.task.assignee ? `@${row.task.assignee}` : '', blocked ? `waits for ${row.waitsFor.join(', ')}` : ''].filter(Boolean).join('\n')}>
              <span className={`cx-pip cx-pip-${state}`} />
              <span className={`t${row.task.status === 'done' ? ' cx-task-done' : ''}`}>{row.task.title}</span>
              <span className="grow" />
              <span className={`ap ${state === 'failed' ? 'live' : state === 'blocked' ? 'hot' : state === 'done' ? 'faint' : state === 'pending' ? 'faint' : 'origin'}`} data-state={state}>{state.replace(/_/g, ' ')}</span>
            </button>
          );
        })}
        {rows.length > SHOWN && (
          <button type="button" className="cx-cs-row" data-action="all-tasks" onClick={() => open(null)}>
            <span className="ap faint">+{rows.length - SHOWN} more on the board</span>
          </button>
        )}
      </div>
    </section>
  );
}

export default definePlugin({ slot: 'card.tasks@1', component: TasksSection });
