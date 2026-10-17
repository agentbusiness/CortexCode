/**
 * workbench.sidebar@1 — projects, and under each project its conversations and its agents with
 * their live state. Everything listed runs in parallel; selecting a row only changes what the
 * other regions show. Its bar hosts `sidebar.actions@1` (open project, new chat, and any other
 * action dropped in). A notification CortexCode showed only here — because its window was in front,
 * or because macOS did not show it — marks its session's row until the session is opened.
 */

import { useLayoutEffect, useMemo, useRef, useState } from 'react';
import {
  Caret,
  ago,
  definePlugin,
  errorMessage,
  useClient,
  useHost,
  useNow,
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

function Sidebar() {
  const client = useClient();
  const host = useHost();
  const projects = useProjects();
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

  const newChat = async (projectId: string) => {
    setError(null);
    setBusy(`new:${projectId}`);
    try {
      const session = await client.call<Session>('frame.session.create', { project_id: projectId, kind: 'chat', title: '' });
      setCollapsed((current) => current.filter((id) => id !== projectId));
      selection.select(projectId, session.id);
    } catch (cause) {
      setError(errorMessage(cause));
    } finally {
      setBusy(null);
    }
  };

  const [strip, setStrip] = useShared<boolean>('layout.sidebarCollapsed', false);
  const list = useMemo(() => orderProjects(projects.data ?? [], everything.data ?? []), [projects.data, everything.data]);
  if (strip) {
    return (
      <Strip
        projects={list}
        notices={notices}
        selectedProject={selection.projectId}
        selectedSession={selection.sessionId}
        onSelect={(projectId, sessionId) => selection.select(projectId, sessionId)}
        onExpand={(projectId) => {
          if (projectId) {
            setCollapsed((current) => current.filter((id) => id !== projectId));
            if (projectId !== selection.projectId) selection.select(projectId, null);
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
        {list.map((project) => (
          <ProjectBlock
            key={project.id}
            project={project}
            notices={notices}
            open={!collapsed.includes(project.id)}
            onToggle={() => setCollapsed((current) => (current.includes(project.id) ? current.filter((id) => id !== project.id) : [...current, project.id]))}
            selectedProject={selection.projectId}
            selectedSession={selection.sessionId}
            onSelect={(sessionId) => selection.select(project.id, sessionId)}
            onNewChat={() => void newChat(project.id)}
            creating={busy === `new:${project.id}`}
          />
        ))}
      </div>
      <div className="cx-sidebar-foot">
        <button type="button" className="detent cx-sidebar-fold" data-action="collapse-sidebar"
          title="Fold the sidebar to a strip of marks; the conversation takes the width" onClick={() => setStrip(true)}>
          <Caret dir="left" /> Fold
        </button>
      </div>
    </div>
  );
}

/**
 * The sidebar folded to a rail of marks: New chat at the top, then per project (a hairline apart)
 * one compact mark per conversation and agent: a short code from its own title (an agent's: @ and
 * its name) with its state as a dot in the corner; the tooltip gives the full name, the project and
 * the state. A mark opens its conversation. Show, at the foot where the open sidebar has Fold,
 * opens the sidebar again.
 */
function Strip({ projects, notices, selectedProject, selectedSession, onSelect, onExpand }: {
  projects: Project[];
  notices: Map<string, Notice>;
  selectedProject: string | null;
  selectedSession: string | null;
  onSelect: (projectId: string, sessionId: string | null) => void;
  onExpand: (projectId: string | null) => void;
}) {
  const host = useHost();
  return (
    <div className="cx-sidebar cx-strip" data-build={__CORTEX_BUILD__} data-mode="strip">
      <host.Slots name="sidebar.actions@1" className="cx-strip-action" props={{ compact: true }} />
      <div className="cx-strip-scroll">
        {projects.map((project) => (
          <StripProject key={project.id} project={project} notices={notices} selected={selectedProject === project.id} selectedSession={selectedSession}
            onSelect={(sessionId) => onSelect(project.id, sessionId)} />
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

function StripProject({ project, notices, selected, selectedSession, onSelect }: {
  project: Project;
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
    <section className={`cx-strip-project${selected ? ' on' : ''}`} data-strip-project-block={project.id} aria-label={project.name}>
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

function ProjectBlock({ project, notices, open, onToggle, selectedProject, selectedSession, onSelect, onNewChat, creating }: {
  project: Project;
  notices: Map<string, Notice>;
  open: boolean;
  onToggle: () => void;
  selectedProject: string | null;
  selectedSession: string | null;
  onSelect: (sessionId: string | null) => void;
  onNewChat: () => void;
  creating: boolean;
}) {
  const sessions = useSessions(project.id);
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
  const isSelected = selectedProject === project.id;

  // An agent is one row: its record (status, task) joined with its session (live state), by name.
  const agentRows = mergeAgents(records, agentSessions);

  return (
    <section className={`cx-proj${isSelected ? ' on' : ''}`} data-project={project.id}>
      <div className="cx-proj-h">
        <button type="button" className="cx-proj-toggle" aria-expanded={open} onClick={onToggle} title={open ? 'Fold this project' : 'Unfold this project'}>
          <Caret dir={open ? 'down' : 'right'} className="cx-caret" />
          <span className="cx-proj-name">{project.name}</span>
        </button>
        <span className="grow" />
        {running > 0 && <span className="ap live" title={`${running} running`}>{running} running</span>}
      </div>
      <button type="button" className="cx-proj-pick" onClick={() => onSelect(null)} aria-pressed={isSelected && !selectedSession}
        title={`${project.root} — show this project without a conversation`}>
        <span className="ap faint">{project.root}</span>
      </button>
      {open && (
        <>
          <ul className="lst cx-sessions" aria-label={`Conversations in ${project.name}`}>
            {chats.map((session) => (
              <SessionRow key={session.id} session={session} now={now} notice={selectedSession === session.id ? undefined : notices.get(session.id)}
                selected={selectedSession === session.id} onSelect={() => onSelect(session.id)} />
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

function SessionRow({ session, selected, onSelect, now, notice }: { session: Session; selected: boolean; onSelect: () => void; now: number; notice?: Notice }) {
  const title = session.title.trim() || 'New conversation';
  return (
    <li className={[selected ? 'sel' : '', needsYou(session.status) ? 'cx-attn' : '', notice ? 'cx-notice' : ''].filter(Boolean).join(' ') || undefined}
      data-session={session.id} data-status={session.status} data-notice={notice?.kind}>
      <button type="button" onClick={onSelect} aria-current={selected ? 'true' : undefined} title={notice ? notice.title : undefined}>
        <div className="t"><VerdictTitle title={title} /></div>
        <div className="r">
          <StatePip status={session.status} />
          <span className={`ap${statusTone(session.status)}`}>{STATUS_LABEL[session.status] ?? session.status}</span>
          {notice && <NoticeTag notice={notice} />}
          <span className="grow" />
          <span className="ap faint">{ago(session.updated_ms, now)}</span>
        </div>
      </button>
    </li>
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

export default definePlugin({ slot: 'workbench.sidebar@1', component: Sidebar });

/** A title whose last part is a benchmark verdict the harness set (` ✓ …` passed, ` ✗ …` failed):
 * the verdict in green or red after the name. */
function VerdictTitle({ title }: { title: string }) {
  const match = /^(.*?)\s+([✓✗])(.*)$/.exec(title);
  if (!match) return <>{title}</>;
  const passed = match[2] === '✓';
  return (
    <>
      {match[1]} <span className={passed ? 'cx-verdict-pass' : 'cx-verdict-fail'}>{match[2]}{match[3]}</span>
    </>
  );
}
