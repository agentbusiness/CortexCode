/**
 * workbench.chat@1 — the conversation: the session's journal, materialized and drawn as turns
 * (streaming answers, folded reasoning, tool calls as step rows with their diffs, routes, run
 * status, verification and spawned agents) and the live working area. The composer is its own
 * plugin on `chat.composer@1`, hosted at the foot.
 */

import { useEffect, useLayoutEffect, useMemo, useRef, useState } from 'react';
import {
  kinds,
  definePlugin,
  errorMessage,
  openInCode,
  setShared,
  useClient,
  useHost,
  useJournal,
  useQuery,
  useModelState,
  reverseApply,
  useProjects,
  useSelection,
  useShared,
  useSlotState,
  type ChatFocus,
  type FlowState,
  type JournalPatch,
  showPane,
 runtime } from '@cortex/sdk';
import { RevealContext, TurnView, type Reveal, type TurnActions } from './TurnView';
import { isScaffolding } from './Activity';
import { editSteps } from './Edits';
import { displayDiffs } from './StepRow';
import { Working } from './Working';
import { IdeationContext, SpecCard, type IdeationContextValue } from './IdeationCards';
import { acceptedDecisions, currentSpec, newestPlans, specVersions } from './ideation';
import { answerStarted, buildTurns, liveReasoning, runState } from './transcript';
import { decisionLines, progressLine, useLiveRun } from './live';
import './chat.css';

/** This view's flow: its calls go to the coding flow's category. */
const FLOW = 'chatflow-coding';

function Chat() {
  const client = useClient();
  const host = useHost();
  const selection = useSelection();
  const projects = useProjects();
  const { project, projectId, sessionId, session, workspace } = selection;
  const journal = useJournal(sessionId);
  const entries = journal.entries;
  const turns = useMemo(() => buildTurns(entries), [entries]);
  const run = useMemo(() => runState(entries), [entries]);
  const [scrolls, setScrolls] = useSlotState<Record<string, number>>('scroll', {});
  const [error, setError] = useState<string | null>(null);
  const [stick] = useShared<number>('chat.stick', 0);
  // The mode a turn started from a card runs in (the composer's default for new work).
  const [defaultMode] = useShared<'ask' | 'auto'>('agents.defaultMode', 'auto');
  const [focus, setFocus] = useShared<ChatFocus | null>('chat.focus', null);
  // What was last asked to be shown: folded steps holding it stay open; the element is marked.
  const [reveal, setReveal] = useState<Reveal & { sessionId: string | null; landed: boolean }>({ seq: null, sessionId: null, landed: false });

  const flow = useQuery<FlowState>(
    sessionId && session?.kind !== 'agent' ? `chatflow.state:${sessionId}` : null,
    (c) => c.call<FlowState>(`${FLOW}.state`, { session_id: sessionId }),
    {
      topics: sessionId ? [`journal.${sessionId}`] : [],
      when: (_topic, data) => {
        const patch = data as JournalPatch;
        return patch?.op === 'create' && (patch.entry.kind === 'status' || patch.entry.kind === 'steer' || patch.entry.kind === kinds.APPROVAL);
      },
    },
  );
  const running = run.running || flow.data?.running === true;
  // While a run is on, the model's state (loading, slots busy) is part of what it shows.
  const model = useModelState({ watchSlots: running });
  // What the model does for this conversation now (its requests' progress, a decision's text).
  const liveRun = useLiveRun(session?.kind === 'agent' ? null : sessionId, running);

  // Opening a conversation loads nothing into the model: only writing in it does (the composer
  // prefills while the person writes, the turn reads the rest when it is sent).

  useEffect(() => {
    setError(null);
  }, [sessionId]);

  // ─── scrolling: follow the end while the reader is there; remember where they were ──────────
  const scroller = useRef<HTMLDivElement>(null);
  const atEnd = useRef(true);
  const [away, setAway] = useState(false);
  const restoredFor = useRef<string | null>(null);
  useLayoutEffect(() => {
    const element = scroller.current;
    if (!element || journal.status !== 'ready' || !sessionId) return;
    if (restoredFor.current !== sessionId) {
      restoredFor.current = sessionId;
      const saved = scrolls[sessionId];
      if (saved === undefined || saved < 0) {
        element.scrollTop = element.scrollHeight;
        atEnd.current = true;
      } else {
        element.scrollTop = saved;
        atEnd.current = element.scrollHeight - element.scrollTop - element.clientHeight < 48;
      }
      setAway(!atEnd.current);
      return;
    }
    if (atEnd.current) element.scrollTop = element.scrollHeight;
  }, [entries, journal.status, sessionId, scrolls, running]);
  const saveTimer = useRef<ReturnType<typeof setTimeout> | null>(null);
  const onScroll = () => {
    const element = scroller.current;
    if (!element || !sessionId) return;
    const end = element.scrollHeight - element.scrollTop - element.clientHeight < 48;
    atEnd.current = end;
    setAway(!end);
    if (saveTimer.current) clearTimeout(saveTimer.current);
    const top = element.scrollTop;
    saveTimer.current = setTimeout(() => setScrolls((current) => ({ ...current, [sessionId]: end ? -1 : top })), 300);
  };
  // The composer sent something: follow the end again.
  useEffect(() => {
    if (!stick) return;
    atEnd.current = true;
    const element = scroller.current;
    if (element) element.scrollTop = element.scrollHeight;
  }, [stick]);

  // Another region asked to show an entry (a notification click, an attribution in the code view):
  // open what folds it, then bring it into view and mark it. An approval shows its Allow / Deny, a
  // verification its failing check.
  useEffect(() => {
    if (!focus || focus.sessionId !== sessionId || journal.status !== 'ready') return;
    if (focus.seq === null) {
      // The conversation itself (a notification about it): its latest.
      const element = scroller.current;
      if (element) element.scrollTop = element.scrollHeight;
      atEnd.current = true;
      setFocus(null);
      return;
    }
    if (!entries.some((entry) => entry.seq === focus.seq)) return; // not loaded yet
    setReveal({ seq: focus.seq, kind: focus.kind ?? null, check: focus.check ?? null, sessionId: focus.sessionId, landed: false });
    setFocus(null);
    // `setFocus` is a stable store setter.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [focus, sessionId, journal.status, entries.length]);
  useEffect(() => {
    if (reveal.seq === null || reveal.landed || reveal.sessionId !== sessionId) return;
    const root = scroller.current;
    const entry = root?.querySelector(`[data-seq="${reveal.seq}"], [data-call-seq="${reveal.seq}"], [data-step-seqs~="${reveal.seq}"]`) as HTMLElement | null;
    if (!root || !entry) return;
    const detail = reveal.check
      ? ([...entry.querySelectorAll<HTMLElement>('[data-check]')].find((element) => element.getAttribute('data-check') === reveal.check) ?? null)
      : (entry.querySelector('.acts') as HTMLElement | null);
    const shown = detail ?? entry;
    shown.scrollIntoView({ block: 'center' });
    atEnd.current = false;
    for (const marked of root.querySelectorAll('[data-focused]')) marked.removeAttribute('data-focused');
    entry.setAttribute('data-focused', 'entry');
    if (detail) detail.setAttribute('data-focused', 'detail');
    setReveal((current) => ({ ...current, landed: true }));
  }, [reveal, sessionId, entries]);

  const toEnd = () => {
    const element = scroller.current;
    if (!element) return;
    element.scrollTo({ top: element.scrollHeight, behavior: 'smooth' });
    atEnd.current = true;
    setAway(false);
  };

  // ─── actions ─────────────────────────────────────────────────────────────────────────────
  const actions = useMemo<TurnActions>(() => ({
    approve: async (seq, approve) => {
      if (!sessionId) return;
      await client.call(`${FLOW}.approve`, { session_id: sessionId, seq, approve });
    },
    openFile: (path) => {
      if (workspace) openInCode({ root: workspace, path });
    },
    openLink: (url) => {
      showPane('browser');
      void client.call('browser.open', { url, session_id: sessionId }).catch((cause) => host.notify(`The browser could not open ${url}: ${errorMessage(cause)}`));
    },
    // Undo a turn's edits: each file's current text with the turn's own hunks reverse-applied,
    // newest edit first, written back through the editor's save. Nothing else is touched.
    undoSteps: workspace ? async (note) => {
      const result = await client.call<{ restored: string[]; skipped: string[]; blocked_by: number[] }>('tools.undo', { workspace: note.workspace || workspace, seqs: note.seqs, cascade: true, session_id: sessionId, actor: 'user' });
      if (result.skipped.length) throw new Error(result.skipped.join('; '));
      runtime().queries.invalidate(`tools.timeline:${workspace}`);
      runtime().queries.invalidate(`vcs.status:${workspace}`);
      runtime().queries.invalidate('code.diff:');
    } : null,
    stepDiff: workspace ? async (note, path) => {
      const result = await client.call<{ diff: string }>('tools.timeline.diff', { workspace: note.workspace || workspace, path, from_seq: note.fromSeq, to_seq: note.toSeq });
      return result.diff;
    } : null,
    undoEdits: workspace ? async (turn, files) => {
      const steps = editSteps(turn).reverse();
      for (const file of files) {
        const read = await client.call<{ text: string }>('tools.read', { workspace, target: file.path });
        let text = read.text;
        for (const step of steps) {
          for (const diff of displayDiffs(step.result!.data.display)) {
            if (diff.path === file.path) text = reverseApply(text, diff.hunks);
          }
        }
        await client.call('tools.save', { workspace, path: file.path, content: text });
      }
    } : null,
    openSession: (id) => selection.select(projectId, id),
    openTask: (taskId) => {
      setShared('board.focus', taskId);
      showPane('board');
    },
    updateSpec: async (change) => {
      if (!sessionId) return;
      await client.call(`${FLOW}.spec.update`, { session_id: sessionId, ...change });
    },
    saveSpec: async () => {
      const saved = await client.call<{ path: string; relative?: string }>(`${FLOW}.spec.save`, { session_id: sessionId });
      return saved?.relative || saved?.path || '';
    },
    answerDecision: async (seq, accept) => {
      if (!sessionId) return;
      await client.call(`${FLOW}.decision.answer`, { session_id: sessionId, seq, accept });
    },
    setPlanStatus: async (planId, status) => {
      if (!sessionId) return;
      await client.call(`${FLOW}.plan.update`, { session_id: sessionId, plan_id: planId, status });
    },
    implementPlan: async (planId) => {
      if (!sessionId) return;
      setShared('chat.stick', Date.now());
      await client.call(`${FLOW}.plan.implement`, { session_id: sessionId, plan_id: planId });
    },
    editPlan: async (change) => {
      if (!sessionId) return;
      await client.call(`${FLOW}.plan.edit`, { session_id: sessionId, ...change });
    },
    commentPlan: async (planId, task, text) => {
      if (!sessionId) return;
      await client.call(`${FLOW}.plan.comment`, { session_id: sessionId, plan_id: planId, text, ...(task === null ? {} : { task }) });
    },
    removeComment: async (planId, seq) => {
      if (!sessionId) return;
      await client.call(`${FLOW}.plan.comment`, { session_id: sessionId, plan_id: planId, remove: seq });
    },
    revisePlan: async (planId) => {
      if (!sessionId) return;
      setShared('chat.stick', Date.now());
      await client.call(`${FLOW}.plan.revise`, { session_id: sessionId, plan_id: planId, mode: defaultMode });
    },
    chooseOption: async (seq, option) => {
      if (!sessionId) return;
      await client.call(`${FLOW}.ideation.choose`, { session_id: sessionId, seq, option });
    },
    sendMessage: async (text) => {
      if (!sessionId) return;
      setShared('chat.stick', Date.now());
      await client.call(`${FLOW}.turn`, { session_id: sessionId, text, mode: defaultMode });
    },
  }), [workspace, client, sessionId, projectId, selection, host, defaultMode]);
  const spec = useMemo(() => currentSpec(entries), [entries]);
  const decisions = useMemo(() => acceptedDecisions(entries), [entries]);
  const ideation = useMemo<IdeationContextValue>(() => ({ entries, running, newestPlans: newestPlans(entries), specVersions: specVersions(entries) }), [entries, running]);

  const lastTurn = turns.at(-1);
  // One place says what happens now: the turn's own step list once it has steps or reasoning
  // (the running step last), the working box only before that.
  const showWorking = running && !answerStarted(lastTurn) && !lastTurn?.blocks.some(isScaffolding);
  const liveLines = running && liveRun.decision && !liveRun.decision.done ? decisionLines(liveRun.decision) : [];

  return (
    <RevealContext.Provider value={reveal.sessionId === sessionId ? reveal : NO_REVEAL}>
    <IdeationContext.Provider value={ideation}>
    <div className="cx-chat conversation" data-build={__CORTEX_BUILD__} data-session={sessionId ?? ''} data-running={running ? 'true' : 'false'}
      data-focus-seq={reveal.sessionId === sessionId && reveal.landed && reveal.seq !== null ? reveal.seq : undefined}>
      {/* No header row: the project, title, agent and run state are in the brand band (chatflow.band-session). */}
      <div className="conversation-scroll" ref={scroller} onScroll={onScroll} aria-live="polite">
        <div className="conversation-feed">
          {!project ? (
            (projects.data ?? []).length > 0 ? (
              <div className="empty">
                <h1 className="pagekey sm">Pick a conversation</h1>
                <p className="prose cx-lead">Choose one under a project in the sidebar, or start one there with New chat.</p>
              </div>
            ) : (
              <div className="empty">
                <h1 className="pagekey sm">Open a project</h1>
                <p className="prose cx-lead">Open a folder from the sidebar. CortexCode indexes it, and every conversation and agent works inside it.</p>
              </div>
            )
          ) : !sessionId ? (
            <div className="empty">
              <h1 className="pagekey sm">What should we work on?</h1>
              <p className="prose cx-lead">Write below to start a conversation in {project.name}. Small changes happen here; larger goals go to agents that work in parallel.</p>
            </div>
          ) : journal.status === 'loading' && entries.length === 0 ? (
            <p className="empty-line">Loading the conversation…</p>
          ) : (
            <>
              {journal.status === 'error' && (
                <div className="notice contradiction" role="alert">
                  <span className="kindmark" />
                  <span className="ap lbl">The journal could not be read</span>
                  <p>{journal.error}</p>
                </div>
              )}
              {entries.length === 0 && journal.status === 'ready' && (
                <div className="empty">
                  <h1 className="pagekey sm">What should we work on?</h1>
                  <p className="prose cx-lead">This conversation is empty.</p>
                </div>
              )}
              <SpecCard spec={spec} decisions={decisions} actions={actions} />
              {turns.map((turn, index) => (
                <TurnView key={turn.key} turn={turn} actions={actions} live={running && index === turns.length - 1}
                  progress={running && index === turns.length - 1 && !showWorking ? progressLine(liveRun.progress) : ''}
                  liveLines={running && index === turns.length - 1 && !showWorking ? liveLines : []} />
              ))}
              {showWorking && (
                <Working
                  since={run.since}
                  detail={run.detail}
                  stage={run.stage}
                  waiting={entries.some((entry) => entry.kind === kinds.APPROVAL && entry.data.state === 'pending')}
                  model={model}
                  projectId={projectId}
                  flow={flow.data}
                  reasoning={liveReasoning(turns)}
                  progress={progressLine(liveRun.progress)}
                  decision={decisionLines(liveRun.decision)}
                />
              )}
            </>
          )}
        </div>
      </div>
      <div className="conversation-end">
        <button type="button" className="detent conversation-toend" data-shown={away} onClick={toEnd}>
          Latest
          <svg viewBox="0 0 10 10" aria-hidden="true"><path d="M2 3.5 5 6.5 8 3.5" /></svg>
        </button>
      </div>
      {error && <p className="inline-error" role="alert">{error}</p>}
      {project && <host.Slot name="chat.composer@1" className="cx-composer-slot" />}
    </div>
    </IdeationContext.Provider>
    </RevealContext.Provider>
  );
}

const NO_REVEAL: Reveal = { seq: null };

export default definePlugin({ slot: 'workbench.chat@1', component: Chat });
