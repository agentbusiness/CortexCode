/**
 * chat.composer@1 — where text is written and sent. With no run it
 * starts a turn (`chatflow.turn`, creating the conversation first when there is none); while a
 * run goes it steers it (`chatflow.steer`, taken in at the next step) or queues a follow-up; a
 * running agent can be addressed the same way (`agents.steer`). Mode (ask / auto) and Stop are
 * buttons; drafts are kept per conversation in durable state. Spec marks a new turn's message as
 * the conversation's spec (`spec: true` on `chatflow.turn`).
 *
 * Attachments (pasted screenshots, dropped or picked files and folders) are made with
 * `chatflow.attachment.put` as they are added — bytes go to the chat flow's content-addressed store
 * under CORTEX_HOME, project files and folders are referenced by path — and sent with the message
 * (`attachments` on `chatflow.turn`, `chatflow.steer`, `agents.steer`). A draft's attachments are
 * kept with the draft.
 *
 * Mentions: `@<project>` and `@<project>:<symbol>` complete from `memory.project.list` and
 * `memory.search`; every message carries them as `mentions` (`chatflow.turn`, `chatflow.steer`,
 * `agents.steer`), and the chat flow opens each project read-only and names it to the run (the
 * running one for a steer) with its root and qualified URIs.
 */

import { useEffect, useRef, useState } from 'react';
import {
  definePlugin,
  errorMessage,
  setShared,
  useClient,
  useHost,
  useJournal,
  useQuery,
  useSelection,
  useShared,
  useSlotState,
  type AgentRecord,
  type FlowState,
  type JournalEntry,
  type JournalPatch,
  type Session,
} from '@cortex/sdk';
import { Composer, type MentionSource, type Target } from './Composer';
import { mentionsOf, type KnownProject, type SymbolHit } from './mentions';
import { PASTE_AS_FILE, type AttachContext, type AttachShell, type Attachment, type PasteAsFile, type PutParams, type PutResult } from './attachments';
import './composer.css';

/** `memory.search`'s answer, as far as the completion reads it. */
interface SearchAnswer {
  hits: Array<{ symbol: SymbolHit }>;
}

/** Typing pause after which the conversation's prefix is prefilled (ms). */
export const PREWARM_AFTER_MS = 600;

/** A run is going from a `status{state:"running"}` until a later status says otherwise. */
export function isRunning(entries: readonly JournalEntry[]): boolean {
  for (let i = entries.length - 1; i >= 0; i--) {
    const entry = entries[i]!;
    if (entry.kind === 'status') return entry.data.state === 'running';
  }
  return false;
}

function ComposerPlugin() {
  const client = useClient();
  const host = useHost();
  const selection = useSelection();
  const { projectId, sessionId, session, project, workspace } = selection;
  const journal = useJournal(sessionId);
  const [drafts, setDrafts] = useSlotState<Record<string, string>>('drafts', {});
  const [attached, setAttached] = useSlotState<Record<string, Attachment[]>>('attachments', {});
  const [defaultMode] = useShared<'ask' | 'auto'>('agents.defaultMode', 'auto');
  // When pasted text goes as a file (a setting; the chat flow's defaults otherwise).
  const [pasteAsFile] = useShared<PasteAsFile>('composer.pasteAsFile', PASTE_AS_FILE);
  const [modes, setModes] = useSlotState<Record<string, 'ask' | 'auto'>>('modes', {});
  // Drafts marked as the session's spec (sent as `spec: true` with `chatflow.turn`).
  const [specMarks, setSpecMarks] = useSlotState<Record<string, boolean>>('specMarks', {});
  const [error, setError] = useState<string | null>(null);
  const [stopping, setStopping] = useState(false);
  const draftKey = sessionId ?? (projectId ? `new:${projectId}` : 'none');
  const draft = drafts[draftKey] ?? '';
  // The projects a mention can name, listed once a draft has an `@`.
  const known = useQuery<KnownProject[]>(
    draft.includes('@') ? 'memory.project.list' : null,
    async (c) => (await c.call<{ projects: KnownProject[] }>('memory.project.list', {}))?.projects ?? [],
  );
  const mode = modes[draftKey] ?? defaultMode;
  const flow = useQuery<FlowState>(
    sessionId && session?.kind !== 'agent' ? `chatflow.state:${sessionId}` : null,
    (c) => c.call<FlowState>('chatflow.state', { session_id: sessionId }),
    { topics: sessionId ? [`journal.${sessionId}`] : [], when: (_t, data) => (data as JournalPatch)?.op === 'create' && ['status', 'steer'].includes((data as { entry: JournalEntry }).entry.kind) },
  );
  const agents = useQuery<AgentRecord[]>(
    projectId ? `agents.list:${projectId}` : null,
    async (c) => (await c.call<{ agents: AgentRecord[] }>('agents.list', { project_id: projectId }))?.agents ?? [],
    { topics: ['agents.changed'] },
  );
  const runningAgents = (agents.data ?? []).filter((agent) => ['working', 'running', 'blocked', 'verifying'].includes(agent.status));
  const agentName = session?.kind === 'agent' ? session.agent ?? null : null;
  const running = isRunning(journal.entries) || flow.data?.running === true;
  if (!running && stopping) setStopping(false);

  // While the person writes, the conversation's prompt prefix is prefilled into the model's cache
  // (debounced; once per conversation state), so sending is answered without re-reading it.
  const lastSeq = journal.entries.at(-1)?.seq ?? 0;
  const warmedFor = useRef<string | null>(null);
  const writing = draft.trim() !== '';
  useEffect(() => {
    if (!writing || !sessionId || session?.kind === 'agent' || running) return undefined;
    const state = `${sessionId}:${lastSeq}`;
    if (warmedFor.current === state) return undefined;
    const timer = setTimeout(() => {
      warmedFor.current = state;
      void client.call('chatflow.prewarm', { session_id: sessionId }).catch(() => undefined);
    }, PREWARM_AFTER_MS);
    return () => clearTimeout(timer);
  }, [client, writing, draft, sessionId, session?.kind, running, lastSeq]);

  // A new conversation (created when sent): the project's chat system block, which every new
  // conversation starts with, is prefilled while it is opened and written, so its first call reads
  // only the message. The model keeps the block once; asking again is answered at once.
  const newConversation = !sessionId && projectId ? projectId : null;
  useEffect(() => {
    if (!newConversation) return undefined;
    const timer = setTimeout(() => {
      void client.call('chatflow.prewarm', { project_id: newConversation }).catch(() => undefined);
    }, writing ? PREWARM_AFTER_MS : 0);
    return () => clearTimeout(timer);
  }, [client, newConversation, writing]);

  const send = async (text: string, target: Target, attachments: Attachment[]) => {
    setError(null);
    // Older peers never see the field when there is nothing attached.
    const carry = attachments.length > 0 ? { attachments } : {};
    try {
      // Mentions against the projects memory knows now, for any target; without memory the text
      // goes as written.
      let mentioned = {};
      if (text.includes('@')) {
        const listed = await client.call<{ projects: KnownProject[] }>('memory.project.list', {}).then((answer) => answer?.projects ?? [], () => []);
        const mentions = mentionsOf(text, listed, projectId);
        if (mentions.length > 0) mentioned = { mentions };
      }
      if (target.type === 'agent') {
        await client.call('agents.steer', { project_id: projectId, agent: target.agent, text, kind: target.kind, ...carry, ...mentioned });
        return;
      }
      if (target.type === 'chat') {
        await client.call('chatflow.steer', { session_id: sessionId, text, kind: target.kind, ...carry, ...mentioned });
        return;
      }
      let id = sessionId;
      if (!id) {
        if (!projectId) throw new Error('Open a project first.');
        const created = await client.call<Session>('frame.session.create', { project_id: projectId, kind: 'chat', title: '' });
        id = created.id;
        selection.select(projectId, id);
      }
      setShared('chat.stick', Date.now());
      // Older peers never see the field when the message is not marked.
      const marked = specMarks[draftKey] ? { spec: true } : {};
      await client.call('chatflow.turn', { session_id: id, text, mode, ...carry, ...marked, ...mentioned });
      if (specMarks[draftKey]) setSpecMarks((current) => {
        const next = { ...current };
        delete next[draftKey];
        return next;
      });
    } catch (cause) {
      setError(errorMessage(cause));
      throw cause;
    }
  };

  const stop = async () => {
    if (!sessionId) return;
    setStopping(true);
    try {
      await client.call('chatflow.cancel', { session_id: sessionId });
    } catch (cause) {
      setError(errorMessage(cause));
      setStopping(false);
    }
  };

  const attach: AttachContext = {
    put: (params: PutParams) => client.call<PutResult>('chatflow.attachment.put', params),
    shell: host.shell as unknown as AttachShell | null,
    roots: [project?.root ?? '', workspace ?? ''].filter(Boolean),
    projectId,
    sessionId,
  };

  const mentions: MentionSource = {
    projects: known.data ?? null,
    own: projectId,
    search: async (target, text) => {
      const answer = await client.call<SearchAnswer>('memory.search', { project_id: target.project_id, text, limit: 8 });
      return (answer?.hits ?? []).map((hit) => hit.symbol);
    },
  };

  if (!project) return null;
  return (
    <div className="cx-composer-plugin" data-build={__CORTEX_BUILD__}>
      {error && (
        <div className="notice contradiction cx-send-error" role="alert">
          <span className="kindmark" />
          <span className="ap lbl">Not sent</span>
          <p>{error}</p>
          <div className="acts"><button className="detent" type="button" onClick={() => setError(null)}>Dismiss</button></div>
        </div>
      )}
      <Composer
        draft={draft}
        onDraft={(text) => setDrafts((current) => ({ ...current, [draftKey]: text }))}
        running={running}
        agentSession={agentName}
        agents={runningAgents}
        mode={mode}
        onMode={(next) => setModes((current) => ({ ...current, [draftKey]: next }))}
        onSend={send}
        onStop={() => void stop()}
        stopping={stopping}
        disabled={false}
        attachments={attached[draftKey] ?? []}
        onAttachments={(update) => setAttached((current) => {
          const next = { ...current, [draftKey]: update(current[draftKey] ?? []) };
          if (next[draftKey]!.length === 0) delete next[draftKey];
          return next;
        })}
        attach={attach}
        pasteAsFile={{ ...PASTE_AS_FILE, ...pasteAsFile }}
        specMarked={specMarks[draftKey] === true}
        onSpecMarked={(marked) => setSpecMarks((current) => ({ ...current, [draftKey]: marked }))}
        mentions={mentions}
        placeholder={agentName ? `Steer @${agentName}…` : running ? 'Steer the run, or queue a follow-up…' : `Ask about ${project.name}, or ask for a change…`}
      />
    </div>
  );
}

export default definePlugin({ slot: 'chat.composer@1', component: ComposerPlugin });
