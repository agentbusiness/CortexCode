/**
 * workbench.sidebar@1 — projects, and under each project its conversations and its agents with
 * their live state. Everything listed runs in parallel; selecting a row only changes what the
 * other regions show. The projects are the whole network's: this computer's, then the joined
 * computers' (`mesh.projects`), each with its description; another computer's name is set in blue
 * and its rows talk to that computer, where the project's sessions, files and memory live. Its bar hosts `sidebar.actions@1` (open project, new chat, and any other
 * action dropped in). A notification CortexCode showed only here — because its window was in front,
 * or because macOS did not show it — marks its session's row until the session is opened.
 */

import { useLayoutEffect, useMemo, useRef, useState } from 'react';
import {
  Caret,
  ComputerScope,
  ago,
  definePlugin,
  errorMessage,
  useEvent,
  useHost,
  useNow,
  runtimeFor,
  useProjects,
  useQuery,
  useSelection,
  useSessions,
  useShared,
  useSlotState,
  type AgentRecord,
  type Project,
  type Session,
} from '@cortex/sdk';
import { createPortal } from 'react-dom';
import { markCode } from './mark';
import './sidebar.css';

const STATUS_LABEL: Record<string, string> = {
  idle: 'Idle',
  running: 'Running',
  waiting: 'Needs you',
  done: 'Done',
  failed: 'Failed',
  cancelled: 'Stopped',
  working: 'Working',
  blocked: 'Blocked',
  stopped: 'Stopped',
};

/** What a notification mark says, by notification kind (notify@1 `kinds`). */
const NOTICE_LABEL: Record<string, string> = {
  done: 'Finished',
  failed: 'Failed',
  cancelled: 'Stopped',
  approval: 'Needs you',
  blocked: 'Needs you',
  verified: 'Verified',
  verify_failed: 'Checks failed',
  conflict: 'Conflict',
  spawned: 'Started',
  job: 'Job ended',
  swap_failed: 'Swap failed',
  report: 'Report ready',
};
const NOTICE_URGENT = new Set(['failed', 'approval', 'blocked', 'verify_failed', 'conflict', 'swap_failed']);
/** The notify plugin's method for "shown in the CortexCode window instead of macOS". */
const IN_WINDOW = 'window';
const NOTICE_LOOKBACK = 200;

/** A notification item as `notify.history` returns it (the fields the marks read). */
interface NoticeItem {
  id: string;
  notification: { kind: string; title: string; session_id?: string | null };
  ts_ms: number;
  delivery: string;
  method: string;
  activated_ms?: number | null;
}

/** The newest unseen notification shown only inside CortexCode, for one session. */
interface Notice {
  kind: string;
  title: string;
  label: string;
  urgent: boolean;
}

/**
 * The session marks: per session, the newest notification that reached the person only inside
 * CortexCode (`ui` by `window`: the window was in front; `failed`: macOS did not show it) and was
 * not seen yet. cc-notify clears one (`activated_ms`) when its session is opened in the focused
 * window, and says so on `notify.changed`. Without the notify plugin there are no marks.
 */
function noticesBySession(items: NoticeItem[]): Map<string, Notice> {
  const marks = new Map<string, Notice>();
  for (const item of [...items].sort((a, b) => b.ts_ms - a.ts_ms)) {
    const session = item.notification.session_id;
    if (!session || item.activated_ms || marks.has(session)) continue;
    if (!((item.delivery === 'ui' && item.method === IN_WINDOW) || item.delivery === 'failed')) continue;
    const kind = item.notification.kind;
    marks.set(session, { kind, title: item.notification.title, label: NOTICE_LABEL[kind] ?? 'New', urgent: NOTICE_URGENT.has(kind) });
  }
  return marks;
}

function useNotices(): Map<string, Notice> {
  const history = useQuery<NoticeItem[]>(
    'notify.history:marks',
    async (client) => (await client.call<{ items: NoticeItem[] }>('notify.history', { limit: NOTICE_LOOKBACK }))?.items ?? [],
    { topics: ['notify.changed'] },
  );
  return useMemo(() => noticesBySession(history.data ?? []), [history.data]);
}

/** Sessions that need the person's eye or are at work come first (`running`, `working`, `waiting`, `blocked`). */
function activeRank(status: string): number {
  return isActive(status) ? 0 : 1;
}

/** A project's conversations: the active ones first, then the most recently touched. */
export function orderSessions(sessions: readonly Session[]): Session[] {
  return [...sessions].sort((a, b) => activeRank(a.status) - activeRank(b.status) || b.updated_ms - a.updated_ms);
}

/**
 * The projects: one with an active session first, then by their latest activity (the newest
 * session's time, else when the project was opened), so new work is at the top, never the bottom.
 */
export function orderProjects(projects: readonly Project[], sessions: readonly Session[]): Project[] {
  const latest = new Map<string, number>();
  const active = new Set<string>();
  for (const session of sessions) {
    latest.set(session.project_id, Math.max(latest.get(session.project_id) ?? 0, session.updated_ms));
    if (isActive(session.status)) active.add(session.project_id);
  }
  const time = (project: Project) => Math.max(latest.get(project.id) ?? 0, project.created_ms);
  return [...projects].sort((a, b) => Number(active.has(b.id)) - Number(active.has(a.id)) || time(b) - time(a));
}

/** A project of the network as `mesh.projects` answers it (`node` empty: this computer). */
interface NetworkProject {
  id: string;
  name: string;
  description: string;
  created_ms: number;
  node: string;
  computer: string;
  reachable: boolean;
}

/** One project in the list: this computer's (`node` null) or a joined computer's. */
interface ProjectRow {
  key: string;
  project: Project;
  node: string | null;
  computer: string | null;
  description: string;
  reachable: boolean;
}

/** This computer's projects in their order, then the joined computers' (newest first). */
export function projectRows(own: readonly Project[], network: readonly NetworkProject[]): ProjectRow[] {
  const described = new Map(network.filter((entry) => !entry.node).map((entry) => [entry.id, entry.description]));
  const rows: ProjectRow[] = own.map((project) => ({ key: project.id, project, node: null, computer: null, description: described.get(project.id) ?? '', reachable: true }));
  const remote = network.filter((entry) => entry.node).sort((a, b) => b.created_ms - a.created_ms);
  for (const entry of remote) {
    rows.push({
      key: `${entry.node}:${entry.id}`,
      project: { id: entry.id, name: entry.name, root: '', created_ms: entry.created_ms } as Project,
      node: entry.node,
      computer: entry.computer,
      description: entry.description,
      reachable: entry.reachable,
    });
  }
  return rows;
}

/** The sidebar talks to this computer; each joined computer's project row to its own computer. */
function SidebarRoot() {
  return (
    <ComputerScope node={null}>
      <Sidebar />
    </ComputerScope>
  );
}

function Sidebar() {
  const host = useHost();
  const projects = useProjects();
  const network = useQuery<NetworkProject[]>(
    'mesh.projects',
    async (c) => (await c.call<{ projects: NetworkProject[] }>('mesh.projects', {}))?.projects ?? [],
    { topics: ['mesh.changed'] },
  );
  // Every session once, for the order of the projects (a project with work going on is on top).
  const everything = useQuery<Session[]>(
    'frame.session.list',
    async (c) => {
      const result = await c.call<Session[] | { sessions: Session[] }>('frame.session.list', {});
      return Array.isArray(result) ? result : (result?.sessions ?? []);
    },
    { topics: ['session.changed'] },
  );
  const selection = useSelection();
  const notices = useNotices();
  const [collapsed, setCollapsed] = useSlotState<string[]>('collapsed', []);
  const [scrollTop, setScrollTop] = useSlotState<number>('scrollTop', 0);
  const [busy, setBusy] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const scroller = useRef<HTMLDivElement>(null);
  const restored = useRef(false);

  // Put the list back where it was (after a hot-swap or a restart), once there is a list.
  useLayoutEffect(() => {
    if (restored.current || !scroller.current || !projects.data) return;
    scroller.current.scrollTop = scrollTop;
    restored.current = true;
  }, [projects.data, scrollTop]);

  const newChat = async (row: ProjectRow) => {
    const projectId = row.project.id;
    setError(null);
    setBusy(`new:${row.key}`);
    try {
      const session = await runtimeFor(row.node).client.call<Session>('frame.session.create', { project_id: projectId, kind: 'chat', title: '' });
      setCollapsed((current) => current.filter((key) => key !== row.key));
      selection.select(projectId, session.id, row.node);
    } catch (cause) {
      setError(errorMessage(cause));
    } finally {
      setBusy(null);
    }
  };

  // Deleting: the frame deletes the conversation (and the ones started from it) or the project with
  // all of its conversations; each plugin drops its own data about them. The folder stays on disk.
  const deleteSession = async (row: ProjectRow, sessionId: string) => {
    setError(null);
    try {
      await runtimeFor(row.node).client.call('frame.session.delete', { id: sessionId });
      if (selection.sessionId === sessionId) selection.select(row.project.id, null, row.node);
      await everything.refresh();
    } catch (cause) {
      setError(errorMessage(cause));
      throw cause;
    }
  };
  const deleteProject = async (row: ProjectRow) => {
    setError(null);
    try {
      await runtimeFor(row.node).client.call('frame.project.delete', { id: row.project.id });
      if (isSelected(row)) selection.select(null, null, null);
      await Promise.all([projects.refresh(), everything.refresh(), network.refresh()]);
    } catch (cause) {
      setError(errorMessage(cause));
      throw cause;
    }
  };
  // Deleted elsewhere (another window, a plugin): the lists follow.
  useEvent('project.deleted', () => void projects.refresh());
  useEvent('session.deleted', () => void everything.refresh());

  const [strip, setStrip] = useShared<boolean>('layout.sidebarCollapsed', false);
  const list = useMemo(
    () => projectRows(orderProjects(projects.data ?? [], everything.data ?? []), network.data ?? []),
    [projects.data, everything.data, network.data],
  );
  /** A row is selected when both the project and its computer are. */
  function isSelected(row: ProjectRow): boolean {
    return selection.projectId === row.project.id && (selection.node ?? null) === row.node;
  }
  if (strip) {
    return (
      <Strip
        rows={list.filter((row) => row.reachable)}
        notices={notices}
        isSelected={isSelected}
        selectedSession={selection.sessionId}
        onSelect={(row, sessionId) => selection.select(row.project.id, sessionId, row.node)}
        onExpand={(row) => {
          if (row) {
            setCollapsed((current) => current.filter((key) => key !== row.key));
            if (!isSelected(row)) selection.select(row.project.id, null, row.node);
          }
          restored.current = false;
          setStrip(false);
        }}
      />
    );
  }
  return (
    <div className="cx-sidebar" data-build={__CORTEX_BUILD__} data-mode="open">
      <div className="bar cx-sidebar-actions">
        <host.Slots name="sidebar.actions@1" className="cx-action" />
      </div>
      {error && <p className="inline-error" role="alert">{error}</p>}
      <div className="cx-sidebar-scroll" ref={scroller} onScroll={(event) => setScrollTop(event.currentTarget.scrollTop)}>
        {projects.error && !projects.data && <p className="hint cx-pad">The frame did not list the projects: {projects.error}</p>}
        {projects.data && list.length === 0 && (
          <div className="cx-pad">
            <p className="empty-line cx-flush">No project yet. Open a folder: CortexCode indexes it into the code graph, and conversations and agents work inside it.</p>
          </div>
        )}
        {list.map((row) => row.reachable ? (
          <ComputerScope key={row.key} node={row.node}>
            <ProjectBlock
              row={row}
              notices={row.node ? NO_NOTICES : notices}
              open={!collapsed.includes(row.key)}
              onToggle={() => setCollapsed((current) => (current.includes(row.key) ? current.filter((key) => key !== row.key) : [...current, row.key]))}
              selected={isSelected(row)}
              selectedSession={isSelected(row) ? selection.sessionId : null}
              onSelect={(sessionId) => selection.select(row.project.id, sessionId, row.node)}
              onNewChat={() => void newChat(row)}
              creating={busy === `new:${row.key}`}
              onDeleteSession={(sessionId) => deleteSession(row, sessionId)}
              onDelete={() => deleteProject(row)}
            />
          </ComputerScope>
        ) : <AwayProject key={row.key} row={row} />)}
      </div>
      <div className="cx-sidebar-foot">
        <button type="button" className="detent cx-sidebar-fold" data-action="collapse-sidebar" aria-label="Fold the sidebar"
          title="Fold the sidebar to a strip of marks; the conversation takes the width" onClick={() => setStrip(true)}>
          <Caret dir="left" />
        </button>
      </div>
    </div>
  );
}

const NO_NOTICES = new Map<string, Notice>();

/** A joined computer's project while that computer is away: greyed, nothing to open. */
function AwayProject({ row }: { row: ProjectRow }) {
  return (
    <section className="cx-proj cx-proj-remote cx-proj-away" data-project={row.project.id} data-node={row.node ?? undefined}
      title={`${row.computer} is not reachable now; its projects open again when it is back`}>
      <div className="cx-proj-h">
        <span className="cx-proj-toggle cx-proj-fold"><Caret dir="right" className="cx-caret" /></span>
        <span className="cx-proj-toggle"><span className="cx-proj-name">{row.project.name}</span></span>
      </div>
      <ProjectAbout row={row} away />
    </section>
  );
}

/** Under the name: the computer it lives on (another one's), then its description. */
function ProjectAbout({ row, away = false }: { row: ProjectRow; away?: boolean }) {
  if (!row.computer && !row.description) return null;
  const host = row.computer ? (away ? `${row.computer}, not reachable` : row.computer) : null;
  return (
    <p className="cx-proj-about" title={[host, row.description].filter(Boolean).join(' — ')}>
      {host && <span className="cx-proj-host">{host}</span>}
      {host && row.description && <span className="cx-proj-sep"> · </span>}
      {row.description}
    </p>
  );
}

/**
 * The sidebar folded to a rail of marks: New chat at the top, then per project (a hairline apart)
 * one compact mark per conversation and agent: a short code from its own title (an agent's: @ and
 * its name) with its state as a dot in the corner; the tooltip gives the full name, the project and
 * the state. A mark opens its conversation. Show, at the foot where the open sidebar has Fold,
 * opens the sidebar again.
 */
function Strip({ rows, notices, isSelected, selectedSession, onSelect, onExpand }: {
  rows: ProjectRow[];
  notices: Map<string, Notice>;
  isSelected: (row: ProjectRow) => boolean;
  selectedSession: string | null;
  onSelect: (row: ProjectRow, sessionId: string | null) => void;
  onExpand: (row: ProjectRow | null) => void;
}) {
  const host = useHost();
  return (
    <div className="cx-sidebar cx-strip" data-build={__CORTEX_BUILD__} data-mode="strip">
      <host.Slots name="sidebar.actions@1" className="cx-strip-action" props={{ compact: true }} />
      <div className="cx-strip-scroll">
        {rows.map((row) => (
          <ComputerScope key={row.key} node={row.node}>
            <StripProject project={row.project} remote={Boolean(row.node)} notices={row.node ? NO_NOTICES : notices} selected={isSelected(row)}
              selectedSession={isSelected(row) ? selectedSession : null} onSelect={(sessionId) => onSelect(row, sessionId)} />
          </ComputerScope>
        ))}
      </div>
      <div className="cx-sidebar-foot cx-strip-foot">
        <button type="button" className="detent cx-sidebar-fold" data-action="expand-sidebar" aria-label="Show the sidebar"
          title="Show the sidebar" onClick={() => onExpand(null)}>
          <Caret dir="right" />
        </button>
      </div>
    </div>
  );
}

function StripProject({ project, remote, notices, selected, selectedSession, onSelect }: {
  project: Project;
  remote: boolean;
  notices: Map<string, Notice>;
  selected: boolean;
  selectedSession: string | null;
  onSelect: (sessionId: string) => void;
}) {
  const sessions = useSessions(project.id);
  const agents = useQuery<AgentRecord[]>(
    `agents.list:${project.id}`,
    async (client) => (await client.call<{ agents: AgentRecord[] }>('agents.list', { project_id: project.id }))?.agents ?? [],
    { topics: ['agents.changed'] },
  );
  const all = sessions.data ?? [];
  const chats = orderSessions(all.filter((session) => session.kind !== 'agent'));
  const agentRows = mergeAgents(agents.data ?? [], all.filter((session) => session.kind === 'agent'));
  return (
    <section className={`cx-strip-project${selected ? ' on' : ''}${remote ? ' cx-strip-remote' : ''}`} data-strip-project-block={project.id} aria-label={project.name}>
      {chats.map((session) => {
        const title = session.title.trim() || 'New conversation';
        const notice = selectedSession === session.id ? undefined : notices.get(session.id);
        const state = notice ? `${STATUS_LABEL[session.status] ?? session.status}; ${notice.label}: ${notice.title}` : STATUS_LABEL[session.status] ?? session.status;
        return (
          <button key={session.id} type="button" className={`cx-strip-mark cx-strip-session${selectedSession === session.id ? ' sel' : ''}${needsYou(session.status) ? ' cx-attn' : ''}${notice ? ' cx-notice' : ''}`}
            data-strip-session={session.id} data-status={session.status} data-notice={notice?.kind} aria-label={`${title}, ${state}`}
            aria-current={selectedSession === session.id ? 'true' : undefined} title={`${title} · ${project.name} — ${state}`} onClick={() => onSelect(session.id)}>
            <span className="cx-strip-code">{markCode(title)}</span>
            <StatePip status={session.status} />
          </button>
        );
      })}
      {agentRows.map((row) => {
        const notice = row.sessionId && selectedSession !== row.sessionId ? notices.get(row.sessionId) : undefined;
        const state = notice ? `${STATUS_LABEL[row.status] ?? row.status}; ${notice.label}: ${notice.title}` : STATUS_LABEL[row.status] ?? row.status;
        return (
          <button key={row.name} type="button" className={`cx-strip-mark cx-strip-agent${row.sessionId && selectedSession === row.sessionId ? ' sel' : ''}${needsYou(row.status) ? ' cx-attn' : ''}${notice ? ' cx-notice' : ''}`}
            data-strip-agent={row.name} data-status={row.status} data-notice={notice?.kind} disabled={!row.sessionId} aria-label={`@${row.name}, ${state}`}
            title={`@${row.name}${row.title ? ` · ${row.title}` : ''} · ${project.name} — ${state}`} onClick={() => row.sessionId && onSelect(row.sessionId)}>
            <span className="cx-strip-code"><span className="cx-strip-at">@</span>{markCode(row.name).slice(0, 2)}</span>
            <StatePip status={row.status} />
          </button>
        );
      })}
    </section>
  );
}

function ProjectBlock({ row, notices, open, onToggle, selected, selectedSession, onSelect, onNewChat, creating, onDeleteSession, onDelete }: {
  row: ProjectRow;
  notices: Map<string, Notice>;
  open: boolean;
  onToggle: () => void;
  selected: boolean;
  selectedSession: string | null;
  onSelect: (sessionId: string | null) => void;
  onNewChat: () => void;
  creating: boolean;
  onDeleteSession: (sessionId: string) => Promise<void>;
  onDelete: () => Promise<void>;
}) {
  const { project } = row;
  const sessions = useSessions(project.id);
  useEvent('session.deleted', (_topic, data) => {
    if ((data as { project_id?: string } | null)?.project_id === project.id) void sessions.refresh();
  });
  const agents = useQuery<AgentRecord[]>(
    `agents.list:${project.id}`,
    async (client) => {
      const result = await client.call<{ agents: AgentRecord[] }>('agents.list', { project_id: project.id });
      return result?.agents ?? [];
    },
    { topics: ['agents.changed'], when: (_topic, data) => !data || (data as { project_id?: string }).project_id === undefined || (data as { project_id?: string }).project_id === project.id },
  );
  const all = sessions.data ?? [];
  const chats = orderSessions(all.filter((session) => session.kind !== 'agent'));
  const agentSessions = all.filter((session) => session.kind === 'agent');
  const records = agents.data ?? [];
  const running = all.filter((session) => session.status === 'running').length;
  const now = useNow(30_000, true);
  const isSelected = selected;
  const where = row.computer ? `on ${row.computer}` : project.root;

  // An agent is one row: its record (status, task) joined with its session (live state), by name.
  const agentRows = mergeAgents(records, agentSessions);

  return (
    <section className={`cx-proj${isSelected ? ' on' : ''}${row.node ? ' cx-proj-remote' : ''}`} data-project={project.id} data-node={row.node ?? undefined}>
      <div className="cx-proj-h">
        <button type="button" className="cx-proj-toggle cx-proj-fold" aria-expanded={open} onClick={onToggle} aria-label={`${open ? 'Fold' : 'Unfold'} ${project.name}`} title={open ? 'Fold this project' : 'Unfold this project'}>
          <Caret dir={open ? 'down' : 'right'} className="cx-caret" />
        </button>
        <button type="button" className="cx-proj-toggle cx-proj-open" onClick={() => onSelect(null)} aria-pressed={isSelected && !selectedSession}
          title={`${where} — show this project without a conversation`}>
          <span className="cx-proj-name">{project.name}</span>
        </button>
        <span className="grow" />
        {running > 0 && <span className="cx-run-badge" title={`${running} running`} aria-label={`${running} running`}>{running}</span>}
        <DeleteButton what="project" detail="its conversations, agents, code graph and caches go; the folder stays on disk"
          confirm={{
            title: `Delete ${project.name}?`,
            body: row.computer
              ? `Its conversations, agents, code graph and caches are deleted from CortexCode on ${row.computer}. The folder stays on its disk.`
              : `Its conversations, agents, code graph and caches are deleted from CortexCode. The folder stays on disk: ${project.root}`,
            action: 'Delete project',
          }}
          onDelete={onDelete} />
      </div>
      <ProjectAbout row={row} />
      {open && (
        <>
          <ul className="lst cx-sessions" aria-label={`Conversations in ${project.name}`}>
            {chats.map((session) => (
              <SessionRow key={session.id} session={session} now={now} notice={selectedSession === session.id ? undefined : notices.get(session.id)}
                selected={selectedSession === session.id} onSelect={() => onSelect(session.id)}
                onDelete={async () => {
                  await onDeleteSession(session.id);
                  await sessions.refresh();
                }} />
            ))}
            {sessions.data && chats.length === 0 && (
              <li className="cx-none">
                <button type="button" onClick={onNewChat} disabled={creating}>
                  <span className="t">{creating ? 'Starting…' : 'Start the first conversation'}</span>
                </button>
              </li>
            )}
          </ul>
          {agentRows.length > 0 && (
            <>
              <div className="cx-sub">
                <span className="ap faint">Agents</span>
                <span className="ap faint">{agentRows.filter((row) => isActive(row.status)).length} active</span>
              </div>
              <ul className="lst cx-agents" aria-label={`Agents in ${project.name}`}>
                {agentRows.map((row) => {
                  const notice = row.sessionId && selectedSession !== row.sessionId ? notices.get(row.sessionId) : undefined;
                  return (
                    <li key={row.name} className={[row.sessionId && selectedSession === row.sessionId ? 'sel' : '', needsYou(row.status) ? 'cx-attn' : '', notice ? 'cx-notice' : ''].filter(Boolean).join(' ') || undefined}
                      data-agent={row.name} data-status={row.status} data-notice={notice?.kind}>
                      <button type="button" disabled={!row.sessionId} onClick={() => row.sessionId && onSelect(row.sessionId)}
                        title={row.sessionId ? `Show @${row.name}'s session${notice ? ` — ${notice.title}` : ''}` : `@${row.name} has no session yet`}>
                        <div className="t"><span className="cx-at">@{row.name}</span>{row.title ? <span className="cx-task"> · {row.title}</span> : null}</div>
                        <div className="r">
                          <StatePip status={row.status} />
                          <span className={`ap${statusTone(row.status)}`}>{STATUS_LABEL[row.status] ?? row.status}</span>
                          {row.role && <span className="ap faint">{row.role}</span>}
                          {notice && <NoticeTag notice={notice} />}
                          <span className="grow" />
                          {row.updated ? <span className="ap faint">{ago(row.updated, now)}</span> : null}
                        </div>
                      </button>
                    </li>
                  );
                })}
              </ul>
            </>
          )}
        </>
      )}
    </section>
  );
}

function SessionRow({ session, selected, onSelect, onDelete, now, notice }: {
  session: Session;
  selected: boolean;
  onSelect: () => void;
  onDelete: () => Promise<void>;
  now: number;
  notice?: Notice;
}) {
  const title = session.title.trim() || 'New conversation';
  return (
    <li className={['cx-row', selected ? 'sel' : '', needsYou(session.status) ? 'cx-attn' : '', notice ? 'cx-notice' : ''].filter(Boolean).join(' ')}
      data-session={session.id} data-status={session.status} data-notice={notice?.kind}>
      <button type="button" onClick={onSelect} aria-current={selected ? 'true' : undefined} title={notice ? notice.title : undefined}>
        <div className="t">{title}</div>
        <div className="r">
          <StatePip status={session.status} />
          <span className={`ap${statusTone(session.status)}`}>{STATUS_LABEL[session.status] ?? session.status}</span>
          {notice && <NoticeTag notice={notice} />}
          <span className="grow" />
          <span className="ap faint">{ago(session.updated_ms, now)}</span>
        </div>
      </button>
      <DeleteButton what="chat" detail="the conversation and its history go; this cannot be undone"
        confirm={{ title: 'Delete this chat?', body: `“${title}” and its history are deleted. This cannot be undone.`, action: 'Delete chat' }}
        onDelete={onDelete} />
    </li>
  );
}

/** What a delete asks before it goes ahead (a project: it takes all of its conversations). */
interface Confirm {
  title: string;
  body: string;
  action: string;
}

/** An × shown on hover; a click asks in the theme's confirm dialog, and Delete there deletes. */
function DeleteButton({ what, detail, confirm, onDelete }: { what: string; detail: string; confirm: Confirm; onDelete: () => Promise<void> }) {
  const [busy, setBusy] = useState(false);
  const [asking, setAsking] = useState(false);
  const run = async () => {
    setBusy(true);
    try {
      await onDelete();
    } catch {
      // The sidebar shows the error.
    } finally {
      setBusy(false);
      setAsking(false);
    }
  };
  return (
    <>
      <button type="button" className="cx-del" data-action={`delete-${what}`} aria-label={`Delete ${what}`} title={`Delete this ${what}: ${detail}`} disabled={busy}
        onClick={(event) => {
          event.stopPropagation();
          setAsking(true);
        }}>
        ×
      </button>
      {asking && <ConfirmDialog confirm={confirm} busy={busy} onConfirm={() => void run()} onCancel={() => setAsking(false)} />}
    </>
  );
}

/** The theme's modal: a question, what it does, and the two answers (Escape cancels). */
function ConfirmDialog({ confirm, busy, onConfirm, onCancel }: { confirm: Confirm; busy: boolean; onConfirm: () => void; onCancel: () => void }) {
  const confirmButton = useRef<HTMLButtonElement>(null);
  useLayoutEffect(() => confirmButton.current?.focus(), []);
  // Over the whole window, inside the theme's scope (a list row or a scrolled column would clip it).
  return createPortal(
    <div className="modal-backdrop" role="presentation" onClick={(event) => event.target === event.currentTarget && !busy && onCancel()}
      onKeyDown={(event) => event.key === 'Escape' && !busy && onCancel()}>
      <div className="modal-card cx-confirm" role="alertdialog" aria-modal="true" aria-labelledby="cx-confirm-title" aria-describedby="cx-confirm-body">
        <h2 id="cx-confirm-title" className="cx-confirm-title">{confirm.title}</h2>
        <p id="cx-confirm-body" className="cx-confirm-body">{confirm.body}</p>
        <div className="modal-actions cx-confirm-actions">
          <button type="button" className="detent" disabled={busy} onClick={onCancel}>Cancel</button>
          <button type="button" className="detent warn" ref={confirmButton} disabled={busy} onClick={onConfirm}>{busy ? 'Deleting…' : confirm.action}</button>
        </div>
      </div>
    </div>,
    document.getElementById('cc') ?? document.body,
  );
}

/** The mark of a notification that is waiting for the person on this row. */
function NoticeTag({ notice }: { notice: Notice }) {
  return <span className={`ap cx-notice-tag ${notice.urgent ? 'live' : 'origin'}`} data-notice-tag={notice.kind}>{notice.label}</span>;
}

function StatePip({ status }: { status: string }) {
  return <span className={`cx-pip cx-pip-${status}`} aria-hidden />;
}

function isActive(status: string): boolean {
  return status === 'running' || status === 'working' || needsYou(status);
}

/** Waiting (an ask-mode call to allow) and blocked need the person; rubric is spent on them. */
function needsYou(status: string): boolean {
  return status === 'waiting' || status === 'blocked';
}

/** Rubric for what needs the person, counter for work in progress, faint for the rest. */
function statusTone(status: string): string {
  if (needsYou(status)) return ' live';
  if (status === 'running' || status === 'working') return ' origin';
  if (status === 'failed') return ' live';
  return ' faint';
}

interface AgentRow {
  name: string;
  status: string;
  role: string | null;
  sessionId: string | null;
  title: string | null;
  updated: number;
}

function mergeAgents(records: AgentRecord[], sessions: Session[]): AgentRow[] {
  const rows = new Map<string, AgentRow>();
  for (const record of records) {
    rows.set(record.name, {
      name: record.name,
      status: record.status,
      role: record.role,
      sessionId: record.session_id ?? null,
      title: record.task_id ?? null,
      updated: record.updated_ms,
    });
  }
  for (const session of sessions) {
    const name = session.agent ?? session.title;
    const existing = rows.get(name);
    if (existing) {
      existing.sessionId = existing.sessionId ?? session.id;
      existing.title = session.title || existing.title;
      // The session's own state is the live one while it runs.
      if (session.status === 'running') existing.status = 'running';
      existing.updated = Math.max(existing.updated, session.updated_ms);
    } else {
      rows.set(name, { name, status: session.status, role: null, sessionId: session.id, title: session.title || null, updated: session.updated_ms });
    }
  }
  return [...rows.values()].sort((a, b) => Number(needsYou(b.status)) - Number(needsYou(a.status)) || Number(isActive(b.status)) - Number(isActive(a.status)) || b.updated - a.updated);
}

export default definePlugin({ slot: 'workbench.sidebar@1', component: SidebarRoot });
