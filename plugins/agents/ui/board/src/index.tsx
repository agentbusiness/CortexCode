/**
 * pane.board@1 — the project's work: each delegated goal with its steps, grouped by what needs the
 * person, what is in progress and what is done (`memory.task.list`); the named agents with their
 * state and controls (`agents.*`); and the project board where agents and the person leave
 * messages (`memory.board.*`).
 */

import { useState, type CSSProperties } from 'react';
import {
  Menu,
  ago,
  clock,
  definePlugin,
  errorMessage,
  useClient,
  useNow,
  useQuery,
  useSelection,
  useShared,
  useSlotState,
  type AgentRecord,
  type BoardMessage,
  type Task,
  ServiceProblem,
} from '@cortex/sdk';
import './board.css';
import { TaskText } from './TaskText';

type View = 'tasks' | 'agents' | 'board';

/**
 * A goal is a task with no listed parent: what the conversation delegated. Its steps are its
 * descendants, in the order they were made; its leaves (steps with no steps of their own) are the
 * work, and the goal's state is read from them, since the goal itself stays pending until they end.
 */
type GoalState = 'attention' | 'working' | 'waiting' | 'done' | 'cancelled';

interface Goal {
  goal: Task;
  steps: Array<{ task: Task; depth: number }>;
  leaves: Task[];
  state: GoalState;
}

const GOAL_LABEL: Record<GoalState, string> = {
  attention: 'needs you',
  working: 'working',
  waiting: 'not started',
  done: 'done',
  cancelled: 'cancelled',
};

/** What needs the person first, then what runs, then what waits. */
const GOAL_RANK: Record<GoalState, number> = { attention: 0, working: 1, waiting: 2, done: 3, cancelled: 4 };

const STEP_LABEL: Record<string, string> = {
  pending: 'to do',
  blocked: 'waits for a step',
  in_progress: 'working',
  verifying: 'checking',
  done: 'done',
  failed: 'failed',
  indeterminate: 'unclear',
  cancelled: 'cancelled',
};

function goalState(goal: Task, leaves: readonly Task[]): GoalState {
  if (goal.status === 'cancelled') return 'cancelled';
  const live = leaves.filter((task) => task.status !== 'cancelled');
  if (live.some((task) => task.status === 'failed' || task.status === 'indeterminate')) return 'attention';
  if (live.some((task) => task.status === 'in_progress' || task.status === 'verifying')) return 'working';
  if (live.some((task) => task.status === 'pending')) return 'waiting';
  return live.length > 0 || goal.status === 'done' ? 'done' : 'cancelled';
}

export function goalsOf(tasks: readonly Task[]): Goal[] {
  const ids = new Set(tasks.map((task) => task.id));
  const children = new Map<string, Task[]>();
  const roots: Task[] = [];
  for (const task of tasks) {
    if (task.parent_id && ids.has(task.parent_id)) {
      const list = children.get(task.parent_id) ?? [];
      list.push(task);
      children.set(task.parent_id, list);
    } else {
      roots.push(task);
    }
  }
  for (const list of children.values()) list.sort((a, b) => a.created_ms - b.created_ms);
  const seen = new Set<string>();
  const build = (goal: Task): Goal => {
    seen.add(goal.id);
    const steps: Goal['steps'] = [];
    const leaves: Task[] = [];
    const walk = (task: Task, depth: number) => {
      for (const child of children.get(task.id) ?? []) {
        if (seen.has(child.id)) continue;
        seen.add(child.id);
        steps.push({ task: child, depth });
        if (children.has(child.id)) walk(child, depth + 1);
        else leaves.push(child);
      }
    };
    walk(goal, 0);
    const work = leaves.length > 0 ? leaves : [goal];
    return { goal, steps, leaves: work, state: goalState(goal, work) };
  };
  const out = roots.map(build);
  // Tasks whose parents form a cycle have no root to hang from: each is a goal of its own.
  for (const task of tasks) if (!seen.has(task.id)) out.push(build(task));
  return out;
}

function stepState(task: Task, byId: ReadonlyMap<string, Task>): string {
  const waiting = (task.depends_on ?? []).some((id) => byId.get(id)?.status !== 'done');
  return task.status === 'pending' && waiting ? 'blocked' : task.status;
}

/** The goal's latest change: its own or any step's. */
function lastChange({ goal, steps }: Goal): number {
  return Math.max(goal.updated_ms, ...steps.map(({ task }) => task.updated_ms));
}

function Board() {
  const { projectId, project, select } = useSelection();
  const [view, setView] = useShared<View>('board.view', 'tasks');
  if (!projectId || !project) return <div className="cx-board"><p className="empty-line">Open a project to see its tasks and agents.</p></div>;
  return (
    <div className="cx-board" data-build={__CORTEX_BUILD__} data-view={view}>
      <div className="bar">
        <div className="seg-set" role="group" aria-label="Board">
          <button type="button" aria-pressed={view === 'tasks'} onClick={() => setView('tasks')}>Tasks</button>
          <button type="button" aria-pressed={view === 'agents'} onClick={() => setView('agents')}>Agents</button>
          <button type="button" aria-pressed={view === 'board'} onClick={() => setView('board')}>Messages</button>
        </div>
        <div className="grow" />
        <Scheduler />
      </div>
      <div className="cx-board-scroll">
        {view === 'tasks' && <Tasks projectId={projectId} />}
        {view === 'agents' && <Agents projectId={projectId} onOpen={(sessionId) => select(projectId, sessionId)} />}
        {view === 'board' && <Messages projectId={projectId} />}
      </div>
    </div>
  );
}

function Scheduler() {
  const scheduler = useQuery<{ running: string[]; queued: string[]; max_parallel: number }>(
    'agents.scheduler',
    (c) => c.call('agents.scheduler', {}),
    { topics: ['agents.changed'] },
  );
  if (!scheduler.data) return null;
  const { running, queued, max_parallel } = scheduler.data;
  return (
    <span className="cx-board-scheduler" title={`running: ${running.join(', ') || 'none'}\nqueued: ${queued.join(', ') || 'none'}`}>
      {max_parallel > 0 ? `${running.length} of ${max_parallel} agents running` : 'No agents can run'}{queued.length ? ` · ${queued.length} queued` : ''}
    </span>
  );
}

/**
 * The tasks view answers, in this order: what needs me, what is being done and how far along it is,
 * and what is finished. One block per goal; open goals show their steps, finished goals fold to one
 * line, cancelled goals stay out of the way until asked for.
 */
function Tasks({ projectId }: { projectId: string }) {
  const [focus, setFocus] = useShared<string | null>('board.focus', null);
  const [openIds, setOpenIds] = useSlotState<string[]>(`openTasks:${projectId}`, []);
  const [showCancelled, setShowCancelled] = useState(false);
  const tasks = useQuery<Task[]>(
    `memory.task.list:${projectId}`,
    async (c) => (await c.call<{ tasks: Task[] }>('memory.task.list', { project_id: projectId }))?.tasks ?? [],
    { topics: ['memory.task.changed', 'agents.changed'] },
  );
  if (tasks.error && !tasks.data) return <ServiceProblem service="memory" state={tasks} className="inline-error" />;
  if (!tasks.data) return <p className="empty-line">Reading tasks…</p>;
  if (tasks.data.length === 0) return <p className="empty-line">No tasks yet. Larger goals become steps for agents when the conversation delegates them.</p>;
  const byId = new Map(tasks.data.map((task) => [task.id, task]));
  const goals = goalsOf(tasks.data);
  const focused = focus ? goals.find(({ goal, steps }) => goal.id === focus || steps.some(({ task }) => task.id === focus)) : undefined;
  const isOpen = (id: string) => openIds.includes(id) || focus === id;
  const toggle = (id: string) => {
    if (focus === id) setFocus(null);
    setOpenIds((list) => (isOpen(id) ? list.filter((entry) => entry !== id) : [...list, id]));
  };
  const newest = (a: Goal, b: Goal) => lastChange(b) - lastChange(a);
  const current = goals
    .filter(({ state }) => state !== 'done' && state !== 'cancelled')
    .sort((a, b) => GOAL_RANK[a.state] - GOAL_RANK[b.state] || newest(a, b));
  const finished = goals.filter(({ state }) => state === 'done').sort(newest);
  const cancelled = goals.filter(({ state }) => state === 'cancelled').sort(newest);
  const attention = current.filter(({ state }) => state === 'attention').length;
  const cancelledShown = showCancelled || focused?.state === 'cancelled';
  const props = { byId, focus, isOpen, toggle };
  const folded = (entry: Goal) => !isOpen(entry.goal.id) && focused !== entry;
  return (
    <div className="cx-goals">
      <p className="cx-goals-summary">
        {current.length === 0 ? 'Nothing in progress' : `${current.length} goal${current.length === 1 ? '' : 's'} in progress`}
        {attention > 0 && <span className="cx-goals-attention"> · {attention} need{attention === 1 ? 's' : ''} you</span>}
        <span> · {finished.length} done</span>
      </p>
      {current.length > 0 && (
        <section aria-label="In progress">
          {current.map((entry) => <GoalBlock key={entry.goal.id} {...props} entry={entry} folded={false} />)}
        </section>
      )}
      {finished.length > 0 && (
        <section aria-label="Done">
          <h3 className="cx-goals-head">Done</h3>
          {finished.map((entry) => <GoalBlock key={entry.goal.id} {...props} entry={entry} folded={folded(entry)} />)}
        </section>
      )}
      {cancelled.length > 0 && (
        <section aria-label="Cancelled">
          <button className="cx-goals-more" type="button" aria-expanded={cancelledShown} onClick={() => setShowCancelled(!cancelledShown)}>
            {cancelledShown ? 'Hide' : 'Show'} {cancelled.length} cancelled goal{cancelled.length === 1 ? '' : 's'}
          </button>
          {cancelledShown && cancelled.map((entry) => <GoalBlock key={entry.goal.id} {...props} entry={entry} folded={folded(entry)} />)}
        </section>
      )}
    </div>
  );
}

interface GoalProps {
  entry: Goal;
  folded: boolean;
  byId: ReadonlyMap<string, Task>;
  focus: string | null;
  isOpen: (id: string) => boolean;
  toggle: (id: string) => void;
}

/**
 * One goal: its title, its state and how many of its steps are done; under it the steps, one line
 * each. A folded goal is the head alone. The goal's own brief and result open from its head.
 */
function GoalBlock({ entry, folded, byId, focus, isOpen, toggle }: GoalProps) {
  const [showCancelledSteps, setShowCancelledSteps] = useState(false);
  const { goal, steps, leaves, state } = entry;
  const live = leaves.filter((task) => task.status !== 'cancelled');
  const done = live.filter((task) => task.status === 'done').length;
  const hidden = steps.filter(({ task }) => task.status === 'cancelled' && task.id !== focus).length;
  const shown = steps.filter(({ task }) => showCancelledSteps || task.status !== 'cancelled' || task.id === focus);
  const detailsOpen = isOpen(goal.id) && !folded;
  return (
    <article className="cx-goal" data-task={goal.id} data-state={state} data-folded={folded} data-selected={focus === goal.id}>
      <button className="cx-goal-head" type="button" aria-expanded={detailsOpen} onClick={() => toggle(goal.id)}>
        <span className="cx-goal-title" data-full={detailsOpen}>{goal.title}</span>
        <span className="cx-goal-state"><i aria-hidden="true" />{GOAL_LABEL[state]}</span>
        <span className="cx-goal-meta">
          {steps.length > 0 && <span>{done} of {live.length} step{live.length === 1 ? '' : 's'} done</span>}
          <span>Updated {ago(lastChange(entry))}</span>
        </span>
        {!folded && steps.length > 0 && live.length > 0 && (
          <span className="cx-goal-bar" aria-hidden="true"><span style={{ width: `${(done / live.length) * 100}%` }} /></span>
        )}
      </button>
      {detailsOpen && <TaskDetails task={goal} byId={byId} />}
      {!folded && shown.length > 0 && (
        <ol className="cx-steps" aria-label="Steps">
          {shown.map(({ task, depth }) => {
            const open = isOpen(task.id);
            const step = stepState(task, byId);
            const panelId = `cx-task-${task.id}`;
            return (
              <li key={task.id} className="cx-step" data-task={task.id} data-status={step} data-selected={focus === task.id}
                style={{ '--step-indent': `${Math.min(depth, 2) * 20}px` } as CSSProperties}>
                <button className="cx-step-row" type="button" aria-expanded={open} aria-controls={panelId} onClick={() => toggle(task.id)}>
                  <i className="cx-step-mark" aria-hidden="true" />
                  <span className="cx-step-title">{task.title}</span>
                  <span className="cx-step-who">{task.assignee ? `@${task.assignee}` : ''}</span>
                  <span className="cx-step-state">{STEP_LABEL[step] ?? step.replace(/_/g, ' ')}</span>
                  <span className="cx-step-ago">{ago(task.updated_ms)}</span>
                </button>
                {open && <div id={panelId}><TaskDetails task={task} byId={byId} /></div>}
              </li>
            );
          })}
        </ol>
      )}
      {!folded && hidden > 0 && (
        <button className="cx-goals-more cx-steps-more" type="button" onClick={() => setShowCancelledSteps((value) => !value)}>
          {showCancelledSteps ? 'Hide' : 'Show'} {hidden} cancelled step{hidden === 1 ? '' : 's'}
        </button>
      )}
    </article>
  );
}

/** What a task says about itself: what came out first, then what was asked, how, and how it is checked. */
function TaskDetails({ task, byId }: { task: Task; byId: ReadonlyMap<string, Task> }) {
  const checks = task.contract?.checks ?? [];
  const deps = task.depends_on ?? [];
  if (!task.brief && !task.plan && !task.result && checks.length === 0 && deps.length === 0) {
    return <div className="cx-task-open"><p className="cx-task-no-details">No details yet.</p></div>;
  }
  return (
    <div className="cx-task-open">
      {task.result && (
        <details className="cx-task-section" open>
          <summary>Result</summary>
          <TaskText text={task.result} />
        </details>
      )}
      {task.brief && (
        <details className="cx-task-section" open={!task.result}>
          <summary>What was asked</summary>
          <TaskText text={task.brief} />
        </details>
      )}
      {task.plan && (
        <details className="cx-task-section">
          <summary>Plan</summary>
          <TaskText text={task.plan} />
        </details>
      )}
      {checks.length > 0 && (
        <details className="cx-task-section">
          <summary>Checks <span>{checks.length}</span></summary>
          <ul className="cx-contract">
            {checks.map((check, index) => (
              <li key={`${check.name}:${index}`}>
                <span className="cx-check-name">{check.name}</span>
                <span className="cx-check-kind">{check.kind}</span>
                <TaskText text={check.spec} />
                {check.expect && <div className="cx-check-expect"><span>Expected</span><TaskText text={check.expect} /></div>}
              </li>
            ))}
          </ul>
        </details>
      )}
      {deps.length > 0 && (
        <details className="cx-task-section">
          <summary>Waits for <span>{deps.length}</span></summary>
          <ul className="cx-task-dependencies">
            {deps.map((id) => {
              const dep = byId.get(id);
              return <li key={id}>{dep?.title ?? id}<span>{dep ? STEP_LABEL[dep.status] ?? dep.status : 'unavailable'}</span></li>;
            })}
          </ul>
        </details>
      )}
    </div>
  );
}

/** What an agent is doing right now, as the board shows it: the record's status read with the scheduler's lists. */
type AgentState = 'running' | 'waiting' | 'blocked' | 'failed' | 'done' | 'stopped';

const STATE_LABEL: Record<AgentState, string> = {
  running: 'running',
  waiting: 'waiting for a slot',
  blocked: 'blocked',
  failed: 'failed',
  done: 'done',
  stopped: 'stopped',
};

/** Active agents first (running, then waiting, then blocked), the rest after; newest first inside each. */
const STATE_RANK: Record<AgentState, number> = { running: 0, waiting: 1, blocked: 2, failed: 3, done: 4, stopped: 5 };

function agentState(agent: AgentRecord, running: readonly string[], queued: readonly string[]): AgentState {
  if (agent.status === 'working' || agent.status === 'running') {
    if (running.includes(agent.name)) return 'running';
    return queued.includes(agent.name) ? 'waiting' : 'running';
  }
  if (agent.status === 'blocked') return 'blocked';
  if (agent.status === 'failed') return 'failed';
  if (agent.status === 'done') return 'done';
  return 'stopped';
}

function Agents({ projectId, onOpen }: { projectId: string; onOpen: (sessionId: string) => void }) {
  const client = useClient();
  const now = useNow(30_000, true);
  const [busy, setBusy] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const agents = useQuery<AgentRecord[]>(
    `agents.list:${projectId}`,
    async (c) => (await c.call<{ agents: AgentRecord[] }>('agents.list', { project_id: projectId }))?.agents ?? [],
    { topics: ['agents.changed'] },
  );
  const scheduler = useQuery<{ running: string[]; queued: string[]; max_parallel: number }>(
    'agents.scheduler',
    (c) => c.call('agents.scheduler', {}),
    { topics: ['agents.changed'] },
  );
  const tasks = useQuery<Task[]>(
    `memory.task.list:${projectId}`,
    async (c) => (await c.call<{ tasks: Task[] }>('memory.task.list', { project_id: projectId }))?.tasks ?? [],
    { topics: ['memory.task.changed', 'agents.changed'] },
  );
  const act = async (key: string, method: string, params: Record<string, unknown>) => {
    setBusy(key);
    setError(null);
    try {
      await client.call(method, params);
      await agents.refresh();
    } catch (cause) {
      setError(errorMessage(cause));
    } finally {
      setBusy(null);
    }
  };
  if (agents.error && !agents.data) return <ServiceProblem service="agents" state={agents} className="inline-error" />;
  if (!agents.data) return <p className="empty-line">Reading agents…</p>;
  if (agents.data.length === 0) return <p className="empty-line">No agents in this project yet.</p>;
  const running = scheduler.data?.running ?? [];
  const queued = scheduler.data?.queued ?? [];
  const titles = new Map((tasks.data ?? []).map((task) => [task.id, task.title]));
  const rows = agents.data
    .map((agent) => ({ agent, state: agentState(agent, running, queued) }))
    .sort((a, b) => STATE_RANK[a.state] - STATE_RANK[b.state] || b.agent.updated_ms - a.agent.updated_ms);
  return (
    <>
      {error && <p className="inline-error" role="alert">{error}</p>}
      <ul className="cx-agent-list">
        {rows.map(({ agent, state }) => {
          const working = state === 'running' || state === 'waiting' || state === 'blocked';
          const title = agent.task_id ? titles.get(agent.task_id) ?? agent.task_id : null;
          return (
            <li key={agent.name} className="cx-agent-row" data-agent={agent.name} data-state={state} title={agent.charter ?? undefined}>
              <i className="cx-agent-pip" aria-hidden />
              <span className="cx-at">@{agent.name}</span>
              <span className="ap cx-agent-state">{STATE_LABEL[state]}</span>
              <span className="cx-agent-task" title={title ?? undefined}>{title ?? agent.role}</span>
              <span className="ap faint cx-agent-count">{agent.tasks_done ?? 0}✓ {agent.tasks_failed ?? 0}✗</span>
              <span className="ap faint cx-agent-ago">{ago(agent.updated_ms, now)}</span>
              <span className="cx-agent-acts">
                <button className="detent tiny" type="button" disabled={!agent.session_id} onClick={() => agent.session_id && onOpen(agent.session_id)}>Open</button>
                {agent.task_id && (
                  <button className="detent tiny" type="button" disabled={busy !== null}
                    onClick={() => void act(`verify:${agent.name}`, 'agents.verify', { task_id: agent.task_id })}>
                    {busy === `verify:${agent.name}` ? 'Verifying…' : 'Verify'}
                  </button>
                )}
                {!working && agent.workspace && (
                  <button className="detent tiny" type="button" disabled={busy !== null}
                    onClick={() => void act(`merge:${agent.name}`, 'agents.merge', { project_id: projectId, agent: agent.name })}>
                    {busy === `merge:${agent.name}` ? 'Merging…' : 'Merge'}
                  </button>
                )}
                {(agent.status === 'blocked' || agent.status === 'stopped') && agent.session_id && agent.task_id && (
                  <button className="detent tiny" type="button" disabled={busy !== null}
                    onClick={() => void act(`restart:${agent.name}`, 'agents.restart', { project_id: projectId, agent: agent.name })}>
                    {busy === `restart:${agent.name}` ? 'Restarting…' : 'Restart'}
                  </button>
                )}
                {working && (
                  <button className="detent tiny warn" type="button" disabled={busy !== null}
                    onClick={() => void act(`stop:${agent.name}`, 'agents.stop', { project_id: projectId, agent: agent.name })}>
                    {busy === `stop:${agent.name}` ? 'Stopping…' : 'Stop'}
                  </button>
                )}
              </span>
            </li>
          );
        })}
      </ul>
    </>
  );
}

const POST_KINDS = ['idea', 'question', 'decision', 'status', 'blocker'] as const;

function Messages({ projectId }: { projectId: string }) {
  const client = useClient();
  const [text, setText] = useState('');
  const [kind, setKind] = useState<(typeof POST_KINDS)[number]>('idea');
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const board = useQuery<BoardMessage[]>(
    `memory.board.list:${projectId}`,
    async (c) => (await c.call<{ messages: BoardMessage[] }>('memory.board.list', { project_id: projectId }))?.messages ?? [],
    { topics: ['memory.board.posted'] },
  );
  const post = async () => {
    if (!text.trim()) return;
    setBusy(true);
    setError(null);
    try {
      await client.call('memory.board.post', { project_id: projectId, kind, author: 'user', text: text.trim() });
      setText('');
      await board.refresh();
    } catch (cause) {
      setError(errorMessage(cause));
    } finally {
      setBusy(false);
    }
  };
  const messages = [...(board.data ?? [])].sort((a, b) => b.ts_ms - a.ts_ms).slice(0, 20);
  return (
    <>
      {board.error && !board.data && <ServiceProblem service="memory" state={board} className="inline-error" />}
      {board.data && messages.length === 0 && <p className="empty-line">Nothing on the board yet.</p>}
      <div className="reg-log">
        {messages.map((message) => (
          <div key={message.id} className={`entry ${message.author === 'user' ? 'from-you' : message.author === 'chat' ? 'from-system' : 'from-agent'}`}>
            <div className="entry-h">
              <span className="ap who">{message.author === 'user' ? 'You' : message.author === 'chat' ? 'Conversation' : `@${message.author}`}</span>
              <span className="ap faint">{message.kind.replace(/_/g, ' ')}</span>
              {message.to && <span className="ap faint">to @{message.to}</span>}
              <span className="grow" />
              <span className="off">{clock(message.ts_ms)}</span>
            </div>
            <div className="cx-board-message"><TaskText text={message.text} /></div>
          </div>
        ))}
      </div>
      <div className="composer cx-board-post">
        <textarea className="write" rows={2} value={text} placeholder="Leave a message for the agents" aria-label="Board message"
          onChange={(event) => setText(event.target.value)} />
        {error && <p className="inline-error" role="alert">{error}</p>}
        <div className="tools">
          <Menu label="Kind" value={kind} items={POST_KINDS.map((entry) => ({ key: entry, label: entry, selected: entry === kind, onSelect: () => setKind(entry) }))} />
          <div className="grow" />
          <button className="detent send" type="button" disabled={!text.trim() || busy} onClick={() => void post()}>{busy ? 'Posting…' : 'Post'}</button>
        </div>
      </div>
    </>
  );
}

export default definePlugin({ slot: 'pane.board@1', component: Board });
