import { createContext, memo, useState, type ReactNode } from 'react';
import { clock, duration, kinds, useQuery, useSelection, type Json, type JournalEntry, type Task } from '@cortex/sdk';
import { Activity, activityItems, isScaffolding } from './Activity';
import { Attachments } from './Attachments';
import { ChangeCard } from './Change';
import { Edits, type ChangedNote, type EditedFile } from './Edits';
import { DECISION, IDEATION, KNOWLEDGE, PLAN, SPEC } from './ideation';
import { DecisionCard, IdeasCard, OptionsCard, PlanCard, SpecChange, type IdeationActions } from './IdeationCards';
import { MarkdownBlock } from './Markdown';
import { routeText, str, type Block, type Step, type Turn } from './transcript';

/**
 * The entry the conversation was asked to bring into view (a notification click, an attribution):
 * folded steps that hold it open, and the step itself opens.
 */
export interface Reveal {
  seq: number | null;
  kind?: string | null;
  check?: string | null;
}
export const RevealContext = createContext<Reveal>({ seq: null });

/** Whether a step holds the revealed entry (its call or its result). */
export function holds(step: Step, seq: number | null): boolean {
  return seq !== null && (step.call.seq === seq || step.result?.seq === seq);
}

export interface TurnActions extends IdeationActions {
  /** Answer an ask-mode approval (`chatflow.approve`); resolves when the flow took the answer. */
  approve: (seq: number, approve: boolean) => Promise<void>;
  /** Answer a code change to CortexCode (`source.decide`); resolves when the source manager took it. */
  decide: (id: string, approve: boolean) => Promise<void>;
  openFile: (path: string) => void;
  openLink: (url: string) => void;
  /** Put the files a turn edited back as they were before it, from the edit tool's own results
   * (turns without a Changed note; null: not possible here). */
  undoEdits: ((turn: Turn, files: EditedFile[]) => Promise<void>) | null;
  /** Undo a turn's steps through the step log (null: not possible here). */
  undoSteps: ((note: ChangedNote) => Promise<void>) | null;
  /** The net diff of one path over a turn's steps, from the step log. */
  stepDiff: ((note: ChangedNote, path: string) => Promise<string>) | null;
}

/** A turn's blocks in reading order: each run of scaffolding as one group, everything else as itself. */
export type Segment = { kind: 'group'; key: string; blocks: Block[] } | { kind: 'entry'; key: string; block: Block };

/** The model step a block belongs to (`run_id:step` of its reasoning, prose or call), when tagged. */
function stepKey(block: Block): string | null {
  const data = block.kind === 'steps' ? block.steps[0]?.call.data : block.entry.data;
  if (!data || data.host === true || typeof data.step !== 'number') return null;
  return `${str(data.run_id)}:${data.step}`;
}

/**
 * The flow of a step is what the model said, then what it did. Its reasoning is journaled first,
 * so a step's reasoning is held back and folded into the group of that step's calls, after the
 * prose it led to; a step without calls (the answer) keeps its reasoning before the prose.
 */
export function segments(blocks: readonly Block[]): Segment[] {
  const withCalls = new Set<string>();
  for (const block of blocks) {
    const key = block.kind === 'steps' ? stepKey(block) : null;
    if (key) withCalls.add(key);
  }
  const out: Segment[] = [];
  let held: Block[] = [];
  let heldKey: string | null = null;
  const group = (first: Block, ...rest: Block[]) => out.push({ kind: 'group', key: `a${first.key}`, blocks: [first, ...rest] });
  // Scaffolding joins the group before it only within the same step: a later step's reasoning
  // starts its own group, so the list reads step by step (blocks without a step join either way).
  const append = (block: Block) => {
    const last = out.at(-1);
    const key = stepKey(block);
    const lastKey = last?.kind === 'group' ? stepKey(last.blocks[0]!) : null;
    if (last?.kind === 'group' && (key === null || lastKey === null || key === lastKey)) last.blocks.push(block);
    else group(block);
  };
  const releaseHeld = () => {
    if (held.length) {
      const [first, ...rest] = held as [Block, ...Block[]];
      group(first, ...rest);
      held = [];
      heldKey = null;
    }
  };
  for (const block of blocks) {
    const key = stepKey(block);
    if (block.kind === 'entry' && block.entry.kind === kinds.REASONING && key && withCalls.has(key)) {
      if (heldKey !== key) releaseHeld();
      held.push(block);
      heldKey = key;
      continue;
    }
    if (isScaffolding(block)) {
      if (block.kind === 'steps' && key && key === heldKey) {
        const [first, ...rest] = held as [Block, ...Block[]];
        group(first, ...rest, block);
        held = [];
        heldKey = null;
      } else {
        releaseHeld();
        append(block);
      }
      continue;
    }
    // Prose, a notice, a card: the step's prose comes before its calls, so the held reasoning stays held.
    if (!(key && key === heldKey)) releaseHeld();
    out.push({ kind: 'entry', key: block.key, block });
  }
  releaseHeld();
  return out;
}

/**
 * One exchange: what was asked, then the work and the answer in the order they happened. Each run
 * of the work's scaffolding (steps, reasoning, notes for the model) is one activity line where it
 * happened, and what the model wrote for the person between them (its prose, the answer, notices,
 * approvals, cards) follows the line it came after, so the turn reads in the order it ran. The last
 * line carries the turn's cost; once the turn is done, a card lists what it edited.
 */
export const TurnView = memo(function TurnView({ turn, actions, live, progress = '', liveLines = [] }: { turn: Turn; actions: TurnActions; live: boolean; progress?: string; liveLines?: string[] }) {
  const route = turn.routes.map((entry) => str(entry.data.route)).filter(Boolean).join(' → ');
  const last = turn.routes.at(-1);
  const routeDetail = last ? routeText(last) : '';
  const parts = segments(turn.blocks);
  const closingAt = parts.map((part) => part.kind).lastIndexOf('group');
  // Scaffolding, then the answer still streaming: the model's progress is shown after the answer.
  const trailing = closingAt >= 0 && live && progress !== '' && parts.at(-1)?.kind !== 'group';
  // The turn's task rows (`task_link`, in the order the plan made them) are one list; a
  // `task_start` note (the host writes one where a task's work begins) makes that task's row the
  // open one, and everything after it nests under that row until the next task starts. The root
  // task (the plan's goal) is not drawn when its tasks are. What comes before any task stays at
  // the top level.
  type Section = { kind: 'section'; key: string; taskId: string; title: string; number: number; children: ReactNode[] };
  const body: Array<ReactNode | Section> = [];
  const sections = new Map<string, Section>();
  let current: Section | null = null;
  const links = turn.blocks.filter((block): block is Extract<Block, { kind: 'entry' }> => block.kind === 'entry' && block.entry.kind === kinds.TASK_LINK);
  const hasChildren = links.some((block) => str(block.entry.data.parent_id) !== '');
  const place = (node: ReactNode) => (current ? current.children : body).push(node);
  const section = (key: string, taskId: string, title: string): Section => {
    // Numbered in the plan's order: Task 1, Task 2, ...
    const made: Section = { kind: 'section', key, taskId, title, number: sections.size + 1, children: [] };
    sections.set(taskId, made);
    body.push(made);
    return made;
  };
  parts.forEach((part, index) => {
    if (part.kind === 'group') {
      const closing = index === closingAt;
      place(<Activity key={part.key} turn={turn} items={activityItems(part.blocks)} live={live && closing && !trailing} progress={closing && !trailing ? progress : ''} liveLines={closing && !trailing ? liveLines : []} closing={closing} onOpenFile={actions.openFile} />);
    } else if (part.block.kind === 'entry') {
      const entry = part.block.entry;
      if (entry.kind === kinds.TASK_LINK) {
        if (hasChildren && str(entry.data.parent_id) === '') return;
        if (!sections.has(str(entry.data.task_id))) section(part.key, str(entry.data.task_id), str(entry.data.title) || str(entry.data.task_id));
        return;
      }
      if (entry.kind === kinds.NOTE && str(entry.data.purpose) === 'task_start') {
        const taskId = str(entry.data.task_id);
        current = sections.get(taskId) ?? section(part.key, taskId, str(entry.data.text));
        return;
      }
      place(<EntryView key={part.key} entry={entry} actions={actions} />);
    }
  });
  // No scaffolding: the line (what the turn cost) closes the turn.
  if (closingAt < 0 || trailing) {
    place(<Activity key="activity" turn={turn} items={closingAt < 0 ? activityItems(turn.blocks) : []} live={live} progress={progress} liveLines={liveLines} onOpenFile={actions.openFile} />);
  }
  const isSection = (item: ReactNode | Section): item is Section => typeof item === 'object' && item !== null && (item as Section).kind === 'section';
  const rendered: ReactNode[] = body.map((item) => (isSection(item)
    ? (
      <section key={item.key} className="cx-task-run" data-task={item.taskId}>
        <TaskLinkRow taskId={item.taskId} title={item.title} number={item.number} actions={actions} />
        {item.children.length > 0 && <div className="cx-task-run-body">{item.children}</div>}
      </section>
    )
    : item));
  if (!live) rendered.push(<Edits key="edits" turn={turn} actions={{ onOpenFile: actions.openFile, onUndoSteps: actions.undoSteps, onUndoLegacy: actions.undoEdits ? (files) => actions.undoEdits!(turn, files) : null, stepDiff: actions.stepDiff }} />);
  return (
    <>
      {turn.user && (
        <article className="turn you" data-seq={turn.user.seq} aria-label="Your message">
          <div className="turn-h">
            <span className="ap">{typeof turn.user.data.from_steer === 'number' ? 'You steered' : 'You asked'}</span>
            {typeof turn.user.data.from_steer === 'number' && <span className="ap faint">arrived as the last run ended, so it started this one</span>}
            <span className="grow" />
            <time className="ap faint">{clock(turn.user.ts_ms)}</time>
          </div>
          {str(turn.user.data.text) && <p className="say">{str(turn.user.data.text)}</p>}
          <Attachments data={turn.user.data} />
        </article>
      )}
      {(turn.blocks.length > 0 || turn.routes.length > 0) && (
        <article className="turn model" aria-label="CortexCode's work">
          <div className="turn-h">
            <span className="ap live">Cortex</span>
            {route && <span className="ap origin">{route}</span>}
            <span className="grow" />
            <time className="ap faint">{clock(turn.blocks[0]?.kind === 'entry' ? turn.blocks[0].entry.ts_ms : turn.startedMs)}</time>
          </div>
          {routeDetail && <p className="cx-route-detail">{routeDetail}</p>}
          <div className="message-body">{rendered}</div>
        </article>
      )}
    </>
  );
});

function EntryView({ entry, actions }: { entry: JournalEntry; actions: TurnActions }) {
  const data = entry.data;
  switch (entry.kind) {
    case kinds.ASSISTANT_MESSAGE: {
      const text = str(data.text);
      const streaming = data.streaming === true;
      return (
        <div className={`prose markdown cx-answer${streaming ? ' stream' : ''}`} data-seq={entry.seq} data-streaming={streaming ? 'true' : 'false'}>
          <MarkdownBlock text={text} onLink={actions.openLink} />
        </div>
      );
    }
    case kinds.STEER: {
      // A steer that arrived too late for its run became the next turn (`as_turn`): it is drawn
      // once, as that turn's message (the user_message with `from_steer`).
      if (data.as_turn === true) return null;
      const kind = str(data.kind) || 'steer';
      const consumed = data.consumed === true;
      const target = str(data.target);
      const state = consumed
        ? kind === 'followup' ? 'Sent after the run' : 'Taken in before the next step'
        : kind === 'followup' ? 'Queued for after the run' : 'Waiting for the next step';
      return (
        <div className="cx-steer" data-seq={entry.seq} data-kind={kind} data-consumed={consumed ? 'true' : 'false'}>
          <div className="turn-h">
            <span className="ap">{kind === 'followup' ? 'You followed up' : 'You steered'}</span>
            {target && target !== 'chat' && <span className="ap origin">to @{target}</span>}
            <span className="grow" />
            <span className={`ap ${consumed ? 'faint' : 'hot'}`}>{state}</span>
          </div>
          {str(data.text) && <p className="say">{str(data.text)}</p>}
          <Attachments data={data} />
        </div>
      );
    }
    case kinds.STATUS: {
      const state = str(data.state);
      // A turn that ended waiting for the person (`blocked`: it needs something only they can give;
      // `stopped`: a guard ended work that repeated itself) says so as a card, with what it needs;
      // the session list shows "needs you", and this is where the answer is asked for.
      if (state === 'blocked' || state === 'stopped') {
        return (
          <div className="notice commitment cx-needs-you" data-seq={entry.seq} role="alert">
            <span className="kindmark" />
            <span className="ap lbl">{state === 'blocked' ? 'Needs you' : 'Stopped · needs you'}</span>
            <p>{str(data.detail) || 'The work cannot go on without you.'}</p>
            <p className="cx-needs-you-hint">Answer in the message box below; the work goes on from your answer.</p>
          </div>
        );
      }
      if (state !== 'failed' && state !== 'cancelled') return null;
      return (
        <div className={`notice ${state === 'failed' ? 'contradiction' : 'context'}`} data-seq={entry.seq}>
          <span className="kindmark" />
          <span className="ap lbl">{state === 'failed' ? 'The run failed' : 'Stopped'}</span>
          <p>{str(data.detail) || (state === 'failed' ? 'No detail was given.' : 'The run was stopped.')}</p>
        </div>
      );
    }
    case kinds.ERROR:
      return (
        <div className="notice contradiction" role="alert" data-seq={entry.seq}>
          <span className="kindmark" />
          <span className="ap lbl">Trouble{str(data.code) ? ` · ${str(data.code)}` : ''}</span>
          <p>{str(data.message)}</p>
        </div>
      );
    case kinds.TASK_LINK:
      return <TaskLinkRow taskId={str(data.task_id)} title={str(data.title)} actions={actions} />;
    case kinds.AGENT_SPAWNED: {
      const session = str(data.session_id);
      return (
        <ol className="sources-list cx-single">
          <li>
            <button type="button" className="stream-row" disabled={!session} onClick={() => session && actions.openSession(session)}
              title={session ? 'Show the agent\'s session' : undefined}>
              <span className="dot" aria-hidden />
              <span className="who">Agent</span>
              <span className="subject">@{str(data.agent)} {str(data.task_id) ? `· ${str(data.task_id)}` : ''}</span>
              <span className="row-end">{session ? 'open' : ''}</span>
            </button>
          </li>
        </ol>
      );
    }
    case kinds.VERIFICATION:
      return <Verification entry={entry} />;
    case kinds.APPROVAL:
      return <Approval entry={entry} onAnswer={actions.approve} />;
    case kinds.CHANGE:
      return <ChangeCard entry={entry} onDecide={actions.decide} />;
    case kinds.NOTE:
      // Bookkeeping notes without text (a stage closed) are not drawn; notes written for the model
      // (a skill's text, referenced projects) are one line, opened on demand. A task's start is
      // the header of its section (TurnView), not a note.
      if (!str(data.text) || str(data.purpose) === 'task_start') return null;
      return <p className="cx-note" data-seq={entry.seq}>{str(data.text)}</p>;
    case SPEC:
      return <SpecChange entry={entry} />;
    case PLAN:
      return <PlanCard entry={entry} actions={actions} />;
    case DECISION:
      // A decision made by choosing an option is shown by the options card.
      if (typeof data.ideation_seq === 'number') return null;
      return <DecisionCard entry={entry} actions={actions} />;
    case IDEATION:
      return str(data.phase) === 'options' ? <OptionsCard entry={entry} actions={actions} /> : <IdeasCard entry={entry} />;
    // Knowledge is kept at once; the run's learned line counts it.
    case KNOWLEDGE:
      return null;
    default:
      return null;
  }
}

/**
 * An ask-mode tool call waiting for the person: what it would do and why it needs a yes, with
 * Allow and Deny. Answered approvals stay in the transcript with the answer.
 */
function Approval({ entry, onAnswer }: { entry: JournalEntry; onAnswer: (seq: number, approve: boolean) => Promise<void> }) {
  const data = entry.data;
  const state = str(data.state) || 'pending';
  const [busy, setBusy] = useState<'allow' | 'deny' | null>(null);
  const [error, setError] = useState<string | null>(null);
  const classes = Array.isArray(data.classes) ? (data.classes as Json[]).map((c) => (typeof c === 'string' ? c : JSON.stringify(c))) : [];
  const args = data.args && typeof data.args === 'object' ? JSON.stringify(data.args, null, 2) : str(data.args);
  const answer = async (approve: boolean) => {
    setBusy(approve ? 'allow' : 'deny');
    setError(null);
    try {
      await onAnswer(entry.seq, approve);
    } catch (cause) {
      setError(cause instanceof Error ? cause.message : String(cause));
    } finally {
      setBusy(null);
    }
  };
  const pending = state === 'pending';
  return (
    <div className={`notice cx-approval${pending ? ' commitment' : state === 'approved' ? ' context' : ' overdue'}`} data-seq={entry.seq} data-state={state} role={pending ? 'alert' : undefined}>
      <span className="kindmark" />
      <span className="ap lbl">{pending ? 'Needs you' : state === 'approved' ? 'Allowed' : 'Denied'} · {str(data.tool) || 'tool'}</span>
      {str(data.question) && <p className="cx-approval-q">{str(data.question)}</p>}
      {classes.length > 0 && (
        <p className="cx-approval-classes">
          <span className="ap faint">Asks for</span>
          {classes.map((name) => <span key={name} className="ap origin">{name}</span>)}
        </p>
      )}
      {args && (
        <details className="cx-approval-args">
          <summary className="ap faint">What it would run</summary>
          <pre className="run-output">{args}</pre>
        </details>
      )}
      {pending && (
        <div className="acts">
          <button className="detent" type="button" data-action="allow" disabled={busy !== null} onClick={() => void answer(true)}>{busy === 'allow' ? 'Allowing…' : 'Allow'}</button>
          <button className="detent warn" type="button" data-action="deny" disabled={busy !== null} onClick={() => void answer(false)}>{busy === 'deny' ? 'Denying…' : 'Deny'}</button>
        </div>
      )}
      {error && <p className="inline-error">Not answered: {error}</p>}
    </div>
  );
}

function Verification({ entry }: { entry: JournalEntry }) {
  const report = entry.data.report as Record<string, Json> | undefined;
  const ok = report?.ok === true;
  const checks = Array.isArray(report?.checks) ? (report!.checks as Array<Record<string, Json>>) : [];
  return (
    <div className={`notice ${ok ? 'context' : 'contradiction'}`} data-seq={entry.seq}>
      <span className="kindmark" />
      <span className="ap lbl">{ok ? 'Contract verified' : 'Contract not met'} · {str(entry.data.task_id)}</span>
      <ul className="cx-checks">
        {checks.map((check, index) => (
          <li key={index} className={check.ok === true ? 'ok' : 'bad'} data-check={str(check.name)}>
            <span className="ap">{check.ok === true ? 'Pass' : 'Fail'}</span>
            <span className="cx-check-name">{str(check.name)}</span>
            {typeof check.duration_ms === 'number' && <span className="ap faint">{duration(check.duration_ms)}</span>}
            {check.ok !== true && str(check.output_tail) && <pre className="run-output">{str(check.output_tail)}</pre>}
          </li>
        ))}
      </ul>
    </div>
  );
}

/** A task's state in words, from the memory task it links to. */
export function taskState(task: Task | undefined): string {
  if (!task) return 'pending';
  const waits = (task.depends_on ?? []).length > 0 && task.status === 'pending';
  return waits ? 'pending' : task.status.replace(/_/g, ' ');
}

/**
 * A task the turn made (`task_link`): its title and, from the task record, its state (pending,
 * in progress, done, failed), live as the loop works it. Opens the task on the board.
 */
function TaskLinkRow({ taskId, title, number, actions }: { taskId: string; title: string; number?: number; actions: IdeationActions }) {
  const { projectId } = useSelection();
  const tasks = useQuery<Task[]>(projectId ? `memory.task.list:${projectId}` : null, async (c) => (await c.call<{ tasks: Task[] }>('memory.task.list', { project_id: projectId }))?.tasks ?? [], { topics: ['memory.task.changed', 'agents.changed'] });
  const task = tasks.data?.find((item) => item.id === taskId);
  const state = taskState(task);
  return (
    <ol className="sources-list cx-single">
      <li>
        <button type="button" className="stream-row cx-task-link" data-task={taskId} data-state={state.replace(/ /g, '_')} onClick={() => actions.openTask(taskId)} title="Open the task on the board">
          <span className="dot" aria-hidden />
          <span className="who">{number ? `Task ${number}` : 'Task'}</span>
          <span className="subject">{title || taskId}</span>
          <span className="row-end cx-task-state">{state}</span>
        </button>
      </li>
    </ol>
  );
}
