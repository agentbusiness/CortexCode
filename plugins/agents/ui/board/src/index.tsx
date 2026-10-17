/**
 * pane.board@1 — the project's work: the task tree with contracts (`memory.task.list`), the named
 * agents with their state and controls (`agents.*`), and the project board where agents and the
 * person leave messages (`memory.board.*`).
 */

import { useState } from 'react';
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

type View = 'tasks' | 'agents' | 'board';

/** Arrange tasks under their parents; tasks whose parent is not listed are roots. */
export function taskTree(tasks: readonly Task[]): Array<{ task: Task; depth: number }> {
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
  const out: Array<{ task: Task; depth: number }> = [];
  const seen = new Set<string>();
  const walk = (task: Task, depth: number) => {
    if (seen.has(task.id)) return;
    seen.add(task.id);
    out.push({ task, depth });
    for (const child of (children.get(task.id) ?? []).sort((a, b) => a.created_ms - b.created_ms)) walk(child, depth + 1);
  };
  for (const root of roots.sort((a, b) => b.created_ms - a.created_ms)) walk(root, 0);
  // Tasks whose parents form a cycle have no root to hang from: they are listed on their own.
  for (const task of tasks) if (!seen.has(task.id)) walk(task, 0);
  return out;
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
    <span className="ap faint" title={`running: ${running.join(', ') || 'none'}\nqueued: ${queued.join(', ') || 'none'}`}>
      {running.length} of {max_parallel} slots busy{queued.length ? ` · ${queued.length} queued` : ''}
    </span>
  );
}

function Tasks({ projectId }: { projectId: string }) {
  const [focus] = useShared<string | null>('board.focus', null);
  const [openIds, setOpenIds] = useSlotState<string[]>('openTasks', []);
  const tasks = useQuery<Task[]>(
    `memory.task.list:${projectId}`,
    async (c) => (await c.call<{ tasks: Task[] }>('memory.task.list', { project_id: projectId }))?.tasks ?? [],
    { topics: ['memory.task.changed', 'agents.changed'] },
  );
  if (tasks.error && !tasks.data) return <ServiceProblem service="memory" state={tasks} className="inline-error" />;
  if (!tasks.data) return <p className="empty-line">Reading tasks…</p>;
  if (tasks.data.length === 0) return <p className="empty-line">No tasks yet. Larger goals become task trees when the conversation delegates them.</p>;
  const rows = taskTree(tasks.data);
  return (
    <ul className="lst cx-tasks">
      {rows.map(({ task, depth }) => {
        const open = openIds.includes(task.id) || focus === task.id;
        return (
          <li key={task.id} className={focus === task.id ? 'sel' : undefined} data-task={task.id} data-status={task.status}>
            <button type="button" style={{ paddingLeft: 18 + depth * 16 }} aria-expanded={open}
              onClick={() => setOpenIds((list) => (list.includes(task.id) ? list.filter((id) => id !== task.id) : [...list, task.id]))}>
              <div className="t">{task.title}</div>
              <div className="r">
                <span className={`ap ${task.status === 'in_progress' || task.status === 'verifying' ? 'live' : task.status === 'failed' ? 'hot' : 'faint'}`}>{task.status.replace(/_/g, ' ')}</span>
                {task.assignee && <span className="ap origin">@{task.assignee}</span>}
                {(task.contract?.checks.length ?? 0) > 0 && <span className="ap faint">{task.contract!.checks.length} check{task.contract!.checks.length === 1 ? '' : 's'}</span>}
                <span className="grow" />
                <span className="ap faint">{ago(task.updated_ms)}</span>
              </div>
            </button>
            {open && (
              <div className="cx-task-open" style={{ marginLeft: 18 + depth * 16 }}>
                {task.brief && <p className="cx-task-brief">{task.brief}</p>}
                {task.plan && (
                  <>
                    <span className="ap faint">Plan</span>
                    <pre className="cx-task-plan">{task.plan}</pre>
                  </>
                )}
                {(task.contract?.checks.length ?? 0) > 0 && (
                  <>
                    <span className="ap faint">Contract</span>
                    <ul className="cx-contract">
                      {task.contract!.checks.map((check) => (
                        <li key={check.name}>
                          <span className="ap">{check.kind}</span>
                          <span className="cx-check-name">{check.name}</span>
                          <code>{check.spec}</code>
                        </li>
                      ))}
                    </ul>
                  </>
                )}
                {task.result && (
                  <>
                    <span className="ap faint">Result</span>
                    <p className="cx-task-brief">{task.result}</p>
                  </>
                )}
                {(task.depends_on?.length ?? 0) > 0 && <p className="ap faint">Waits for {task.depends_on!.join(', ')}</p>}
              </div>
            )}
          </li>
        );
      })}
    </ul>
  );
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
  return (
    <>
      {error && <p className="inline-error" role="alert">{error}</p>}
      <ul className="lst cx-agent-list">
        {agents.data.map((agent) => {
          const working = agent.status === 'working' || agent.status === 'running' || agent.status === 'blocked';
          return (
            <li key={agent.name} data-agent={agent.name} data-status={agent.status}>
              <div className="cx-agent-row">
                <div className="t"><span className="cx-at">@{agent.name}</span> <span className="ap faint">{agent.role}</span></div>
                <div className="r">
                  <span className={`ap ${working ? 'live' : 'faint'}`}>{agent.status}</span>
                  {agent.task_id && <span className="ap faint">{agent.task_id}</span>}
                  <span className="ap faint">{agent.tasks_done ?? 0} done · {agent.tasks_failed ?? 0} failed</span>
                  <span className="grow" />
                  <span className="ap faint">{ago(agent.updated_ms, now)}</span>
                </div>
                {agent.charter && <p className="cx-task-brief">{agent.charter}</p>}
                <div className="cx-agent-acts">
                  <button className="detent" type="button" disabled={!agent.session_id} onClick={() => agent.session_id && onOpen(agent.session_id)}>Open session</button>
                  {agent.task_id && (
                    <button className="detent" type="button" disabled={busy !== null}
                      onClick={() => void act(`verify:${agent.name}`, 'agents.verify', { task_id: agent.task_id })}>
                      {busy === `verify:${agent.name}` ? 'Verifying…' : 'Verify'}
                    </button>
                  )}
                  {!working && agent.workspace && (
                    <button className="detent" type="button" disabled={busy !== null}
                      onClick={() => void act(`merge:${agent.name}`, 'agents.merge', { project_id: projectId, agent: agent.name })}>
                      {busy === `merge:${agent.name}` ? 'Merging…' : 'Merge'}
                    </button>
                  )}
                  {working && (
                    <button className="detent warn" type="button" disabled={busy !== null}
                      onClick={() => void act(`stop:${agent.name}`, 'agents.stop', { project_id: projectId, agent: agent.name })}>
                      {busy === `stop:${agent.name}` ? 'Stopping…' : 'Stop'}
                    </button>
                  )}
                </div>
              </div>
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
  const messages = [...(board.data ?? [])].sort((a, b) => a.ts_ms - b.ts_ms);
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
            <p>{message.text}</p>
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
