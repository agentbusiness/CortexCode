/**
 * The workbench's selection: which project and session the person is looking at. It lives in the
 * shared durable store, so every region (and every version of every region) agrees on it.
 */

import { runtime, useQuery, useShared } from './runtime';
import type { Client } from './rpc';

/** Bring a right-pane view to the front and open the pane (see `showPane`). */
function showView(state: ReturnType<typeof runtime>['state'], view: string): void {
  state.set('workbench', 'pane.tab', view);
  state.set('workbench', 'layout.paneCollapsed', false);
}
import type { Project, Session } from './types';

export async function listProjects(client: Client): Promise<Project[]> {
  const result = await client.call<Project[] | { projects: Project[] }>('frame.project.list', {});
  return Array.isArray(result) ? result : result?.projects ?? [];
}

export async function listSessions(client: Client, projectId: string): Promise<Session[]> {
  const result = await client.call<Session[] | { sessions: Session[] }>('frame.session.list', { project_id: projectId });
  return Array.isArray(result) ? result : result?.sessions ?? [];
}

export function useProjects() {
  return useQuery<Project[]>('frame.project.list', listProjects, { topics: ['project.changed'] });
}

export function useSessions(projectId: string | null) {
  return useQuery<Session[]>(
    projectId ? `frame.session.list:${projectId}` : null,
    (client) => listSessions(client, projectId as string),
    {
      topics: ['session.changed'],
      when: (_topic, data) => !data || typeof data !== 'object' || !('project_id' in data) || (data as Session).project_id === projectId,
    },
  );
}

export interface Selection {
  projectId: string | null;
  sessionId: string | null;
  project: Project | undefined;
  session: Session | undefined;
  /** The directory the selected session works in (an agent's clone, or the project root). */
  workspace: string | null;
  select(projectId: string | null, sessionId: string | null): void;
}

export function useSelection(): Selection {
  const [projectId, setProjectId] = useShared<string | null>('projectId', null);
  const [sessionId, setSessionId] = useShared<string | null>('sessionId', null);
  const projects = useProjects();
  const sessions = useSessions(projectId);
  const project = projects.data?.find((entry) => entry.id === projectId);
  const session = sessions.data?.find((entry) => entry.id === sessionId);
  const workspace = session?.workspace || project?.root || null;
  return {
    projectId,
    sessionId,
    project,
    session,
    workspace,
    select(nextProject, nextSession) {
      setProjectId(nextProject);
      setSessionId(nextSession);
    },
  };
}

/** Something to show: a project, a session in it, a right-pane view, a journal entry. */
/**
 * Something to show, from a notification click (`notify.activate {target}`), a shell module
 * (`api.navigate`) or a plugin. `open` says what it is:
 *   session, approval  the conversation, scrolled to the entry `seq` and highlighted (an approval:
 *                      its Allow / Deny), the chat focused
 *   task, agent        the board, on the task or the agent; with a `session_id`, also that
 *                      conversation at `seq` (a verification: the failing `check`'s output)
 *   conflict           the conversation at `seq`, and the conflicting file (`path`, relative to the
 *                      session's workspace) in the code view at `line`
 *   job                the job's conversation and the terminal
 *   plugins, settings  the settings window (`panel`, default plugins for `plugins`)
 * Without `open`, a target selects its project and session and may name a right-pane `view`.
 */
export interface Target {
  open?: 'session' | 'approval' | 'task' | 'agent' | 'job' | 'plugins' | 'settings' | string | null;
  project_id?: string | null;
  session_id?: string | null;
  view?: string | null;
  seq?: number | null;
  task_id?: string | null;
  agent?: string | null;
  job_id?: string | null;
  category?: string | null;
  panel?: string | null;
  /** The notification kind the target came from (done, failed, approval, verify_failed, …). */
  kind?: string | null;
  /** The failing check of a verification. */
  check?: string | null;
  /** A file (relative to the session's workspace) and a 1-based line. */
  path?: string | null;
  line?: number | null;
}

/** What the conversation brings into view (`chat.focus`). */
export interface ChatFocus {
  sessionId: string;
  seq: number | null;
  kind?: string | null;
  check?: string | null;
}

/** Show a target: select, bring forward, scroll. */
export async function openTarget(target: Target): Promise<void> {
  const { client, state } = runtime();
  let projectId = target.project_id ?? null;
  let session: Session | null = null;
  if (target.session_id && (!projectId || target.path)) {
    try {
      session = await client.call<Session>('frame.session.get', { id: target.session_id });
      projectId = projectId ?? session.project_id;
    } catch {
      session = null;
    }
  }
  if (projectId) state.set('workbench', 'projectId', projectId);
  const open = target.open ?? null;
  if (open === 'plugins' || open === 'settings') {
    state.set('workbench', 'settings.open', target.panel ?? (open === 'plugins' ? 'plugins' : null) ?? 'model');
    return;
  }
  const focusChat = () => {
    if (!target.session_id) return;
    const focus: ChatFocus = { sessionId: target.session_id, seq: typeof target.seq === 'number' ? target.seq : null, kind: target.kind ?? null, check: target.check ?? null };
    state.set('workbench', 'chat.focus', focus);
  };
  if (open === 'task' || open === 'agent') {
    state.set('workbench', 'board.view', open === 'task' ? 'tasks' : 'agents');
    const focus = open === 'task' ? target.task_id : target.agent;
    if (focus) state.set('workbench', 'board.focus', focus);
    showView(state, 'board');
    if (target.session_id) {
      state.set('workbench', 'sessionId', target.session_id);
      focusChat();
    }
    return;
  }
  if (target.session_id) state.set('workbench', 'sessionId', target.session_id);
  if (open === 'conflict' && target.path && session?.workspace) {
    state.set('workbench', 'code.open', { root: session.workspace, path: target.path, line: typeof target.line === 'number' ? target.line : null });
    showView(state, 'code');
  } else if (open === 'job') showView(state, 'terminal');
  else if (target.view) showView(state, target.view);
  if (target.session_id && (open === 'session' || open === 'approval' || open === 'conflict' || typeof target.seq === 'number')) {
    focusChat();
    if (open === 'session' || open === 'approval') state.set('workbench', 'composer.focus', Date.now());
  }
}
