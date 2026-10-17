import { useEffect, useLayoutEffect, useRef, useState, type ClipboardEvent, type DragEvent } from 'react';
import { Menu, useShared, type AgentRecord, type MenuItem } from '@cortex/sdk';
import {
  MAX_ATTACHMENTS,
  PASTE_AS_FILE,
  attachOne,
  baseName,
  fromDataTransfer,
  humanSize,
  isLongPaste,
  type AttachContext,
  type Attachment,
  type Incoming,
  type PasteAsFile,
} from './attachments';
import { complete, completingAt, projectChoices, symbolText, type KnownProject, type SymbolHit } from './mentions';

export type Target =
  | { type: 'turn' }
  | { type: 'chat'; kind: 'steer' | 'followup' }
  | { type: 'agent'; agent: string; kind: 'steer' | 'followup' };

/** Where `@` mentions come from: the projects memory knows, the session's own, symbol search. */
export interface MentionSource {
  /** `memory.project.list` (null until listed). */
  projects: readonly KnownProject[] | null;
  /** The session's project: never offered, never a reference. */
  own: string | null;
  /** `memory.search` scoped to one project. */
  search: (project: KnownProject, text: string) => Promise<SymbolHit[]>;
}

/** One line of the mention completion. */
interface Choice {
  key: string;
  label: string;
  note: string;
  insert: string;
}

export function targetKey(target: Target): string {
  if (target.type === 'turn') return 'turn';
  if (target.type === 'chat') return `chat:${target.kind}`;
  return `agent:${target.agent}:${target.kind}`;
}

export function targetLabel(target: Target): string {
  if (target.type === 'turn') return 'New turn';
  const who = target.type === 'chat' ? 'this chat' : `@${target.agent}`;
  return `${target.kind === 'steer' ? 'Steer' : 'Follow-up'} · ${who}`;
}

/**
 * Where text is written and sent. While a run is going, what is sent
 * steers it — injected at its next step — or waits as a follow-up for after it; a running agent
 * can be addressed the same way. Enter sends, as in any text field; every action is a button.
 *
 * Attachments: paste an image (a screenshot straight from the clipboard), drop files or folders on
 * the composer, or pick them with Attach; each becomes a chip (a preview for images) that can be
 * removed before sending (its × button, or Backspace in an empty field removes the last one).
 * Pasted text over the paste-as-file limit becomes a text-file chip ("Pasted text · N lines"): the
 * model reads its path and a head/tail preview instead of the whole text; View shows it, Inline
 * puts it back into the message.
 *
 * Mentions (any target): `@` opens a completion of the projects memory knows (`@<project>`), and
 * `@<project>:` one of that project's symbols (`memory.search` scoped to it); arrows move, Enter or Tab takes
 * one, Escape closes it. The flow opens each mentioned project read-only when the message is sent.
 */
export function Composer({
  draft,
  onDraft,
  running,
  agentSession,
  agents,
  mode,
  onMode,
  onSend,
  onStop,
  stopping,
  disabled,
  placeholder,
  attachments,
  onAttachments,
  attach,
  pasteAsFile = PASTE_AS_FILE,
  specMarked = false,
  onSpecMarked,
  mentions,
}: {
  draft: string;
  onDraft: (text: string) => void;
  running: boolean;
  /** The agent this session belongs to, when it is an agent's session. */
  agentSession: string | null;
  agents: AgentRecord[];
  mode: 'ask' | 'auto';
  onMode: (mode: 'ask' | 'auto') => void;
  onSend: (text: string, target: Target, attachments: Attachment[]) => Promise<void>;
  onStop: () => void;
  stopping: boolean;
  disabled: boolean;
  placeholder: string;
  /** The draft's attachments (made, ready to send). */
  attachments: Attachment[];
  onAttachments: (update: (current: Attachment[]) => Attachment[]) => void;
  /** How attachments are made (`chatflow.attachment.put`) and where the project is. */
  attach: AttachContext;
  /** When pasted text goes as a file instead of message text. */
  pasteAsFile?: PasteAsFile;
  /** The next new turn's message is marked as the session's spec (the Spec toggle). */
  specMarked?: boolean;
  onSpecMarked?: (marked: boolean) => void;
  /** `@` mentions of other projects (no completion without it). */
  mentions?: MentionSource;
}) {
  const base: Target = agentSession
    ? { type: 'agent', agent: agentSession, kind: 'steer' }
    : running ? { type: 'chat', kind: 'steer' } : { type: 'turn' };
  const [chosen, setChosen] = useState<Target | null>(null);
  const [sending, setSending] = useState(false);
  const area = useRef<HTMLTextAreaElement>(null);
  const picker = useRef<HTMLInputElement>(null);
  // Attachments being made (stored or checked by the chat flow), and what could not be attached.
  const [making, setMaking] = useState<Array<{ id: number; name: string }>>([]);
  const [problems, setProblems] = useState<string[]>([]);
  const [dragging, setDragging] = useState(false);
  const nextId = useRef(0);
  // The text of long pastes made into files, while this composer lives (View and Inline use it).
  const pastedTexts = useRef(new Map<string, string>());
  const [viewing, setViewing] = useState<string | null>(null);
  // The mention completion: where the caret is, the highlighted line, a completion closed with
  // Escape (by where its `@` is), and the symbols found for the query being completed.
  const [caret, setCaret] = useState(0);
  const [active, setActive] = useState(0);
  const [dismissed, setDismissed] = useState<number | null>(null);
  const [symbols, setSymbols] = useState<{ key: string; hits: SymbolHit[] | null; error?: string }>({ key: '', hits: null });
  const nextCaret = useRef<number | null>(null);

  const add = (incoming: Incoming[]) => {
    if (incoming.length === 0) return;
    const room = MAX_ATTACHMENTS - attachments.length - making.length;
    const accepted = incoming.slice(0, Math.max(0, room));
    const refused = incoming.length - accepted.length;
    if (refused > 0) {
      setProblems((current) => [...current, `${refused === 1 ? 'One item was' : `${refused} items were`} not attached: a message may carry at most ${MAX_ATTACHMENTS} attachments.`]);
    }
    for (const item of accepted) {
      const id = nextId.current++;
      const name = item.type === 'file' ? item.file.name || 'pasted image' : item.name;
      setMaking((current) => [...current, { id, name: item.type === 'text' ? `${name} (pasted text)` : name }]);
      attachOne(item, attach)
        .then((made) => {
          if (item.type === 'text') pastedTexts.current.set(made.path, item.text);
          onAttachments((current) => [...current, made]);
        })
        .catch((cause: unknown) => setProblems((current) => [...current, cause instanceof Error ? cause.message : String(cause)]))
        .finally(() => setMaking((current) => current.filter((entry) => entry.id !== id)));
    }
  };

  const onPaste = (event: ClipboardEvent<HTMLTextAreaElement>) => {
    const data = event.clipboardData;
    const files = data ? Array.from(data.files ?? []) : [];
    if (files.length === 0) {
      // A long text (a log) goes as a file: the model reads its head and tail, and the rest with its tools.
      const text = data?.getData('text/plain') ?? '';
      if (!isLongPaste(text, pasteAsFile)) return;
      event.preventDefault();
      const number = attachments.filter((attachment) => attachment.pasted_text).length + making.length + 1;
      add([{ type: 'text', text, name: `pasted-${number}.txt` }]);
      return;
    }
    // An image alone (a screenshot) is attached; text that comes with it is still pasted.
    const types = data ? Array.from(data.types ?? []) : [];
    if (!types.includes('text/plain')) event.preventDefault();
    add(files.map((file) => ({ type: 'file', file, pasted: true })));
  };

  const carriesFiles = (event: DragEvent) => Array.from(event.dataTransfer?.types ?? []).includes('Files');
  const onDragOver = (event: DragEvent<HTMLDivElement>) => {
    if (!carriesFiles(event)) return;
    event.preventDefault();
    if (event.dataTransfer) event.dataTransfer.dropEffect = 'copy';
    if (!dragging) setDragging(true);
  };
  const onDrop = (event: DragEvent<HTMLDivElement>) => {
    setDragging(false);
    if (!event.dataTransfer || !carriesFiles(event)) return;
    event.preventDefault();
    add(fromDataTransfer(event.dataTransfer, attach.shell));
  };

  const pickFolder = async () => {
    const path = await attach.shell?.pickFolder('Attach a folder');
    if (path) add([{ type: 'folder', name: baseName(path), path }]);
  };

  const remove = (index: number) => onAttachments((current) => current.filter((_, at) => at !== index));

  /** Put a long paste back into the message as text (where the caret is). */
  const inline = (index: number) => {
    const attachment = attachments[index];
    const text = attachment ? pastedTexts.current.get(attachment.path) : undefined;
    if (!attachment || text === undefined) return;
    const element = area.current;
    const at = element ? element.selectionStart ?? draft.length : draft.length;
    onDraft(draft.slice(0, at) + text + draft.slice(at));
    remove(index);
    if (viewing === attachment.path) setViewing(null);
  };

  // Opening a conversation from a notification puts the caret here.
  const [focusAsk] = useShared<number>('composer.focus', 0);
  const seenFocusAsk = useRef(focusAsk);
  useEffect(() => {
    if (focusAsk === seenFocusAsk.current) return;
    seenFocusAsk.current = focusAsk;
    area.current?.focus({ preventScroll: true });
  }, [focusAsk]);

  // A choice made for one run does not outlive it.
  useEffect(() => {
    setChosen(null);
  }, [running, agentSession]);

  // The field grows with what is written, up to the design's limit (`.write` max-height).
  useLayoutEffect(() => {
    const element = area.current;
    if (!element) return;
    element.style.height = 'auto';
    element.style.height = `${element.scrollHeight + 2}px`;
    // A completed mention leaves the caret after it.
    if (nextCaret.current !== null) {
      element.setSelectionRange(nextCaret.current, nextCaret.current);
      setCaret(nextCaret.current);
      nextCaret.current = null;
    }
  }, [draft]);

  const options: Target[] = [];
  if (agentSession) {
    options.push({ type: 'agent', agent: agentSession, kind: 'steer' }, { type: 'agent', agent: agentSession, kind: 'followup' });
  } else if (running) {
    options.push({ type: 'chat', kind: 'steer' }, { type: 'chat', kind: 'followup' });
  } else {
    options.push({ type: 'turn' });
  }
  for (const agent of agents) {
    if (agent.name === agentSession) continue;
    options.push({ type: 'agent', agent: agent.name, kind: 'steer' }, { type: 'agent', agent: agent.name, kind: 'followup' });
  }
  const target = chosen && options.some((option) => targetKey(option) === targetKey(chosen)) ? chosen : base;

  // Mentions go with any target: a new turn, a steer or follow-up, a message to an agent.
  const projects = mentions?.projects ?? null;
  const own = mentions?.own ?? null;
  const found = projects ? completingAt(draft, caret, projects, own) : null;
  const completing = found && found.from !== dismissed ? found : null;
  // A completion closed with Escape stays closed until its mention is left.
  if (dismissed !== null && found?.from !== dismissed) setDismissed(null);
  const symbolKey = completing?.kind === 'symbol' ? `${completing.project.project_id}:${completing.query}` : '';
  const searchable = completing?.kind === 'symbol' && completing.query !== '' && completing.project.state !== 'indexing';

  // Symbols of the mentioned project for what is typed after `:` (the newest query's answer wins).
  useEffect(() => {
    if (!searchable || !mentions || completing?.kind !== 'symbol') return;
    let current = true;
    setSymbols({ key: symbolKey, hits: null });
    mentions.search(completing.project, completing.query)
      .then((hits) => { if (current) setSymbols({ key: symbolKey, hits }); })
      .catch((cause: unknown) => { if (current) setSymbols({ key: symbolKey, hits: [], error: cause instanceof Error ? cause.message : String(cause) }); });
    return () => { current = false; };
    // The key names the project and the query; the source's identity changes every render.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [symbolKey, searchable]);

  let choices: Choice[] = [];
  let info = '';
  if (completing?.kind === 'project') {
    choices = projectChoices(projects ?? [], completing.query, own).map((project) => ({
      key: project.project_id,
      label: `@${project.name}`,
      note: `${project.root}${project.reference ? ' · read-only' : ''}${project.state === 'indexing' ? ' · indexing' : ''}`,
      insert: `@${project.name}`,
    }));
    if (choices.length === 0) info = completing.query ? `No known project starts with “${completing.query}”.` : 'No other project is known yet: attach a repository folder to add one.';
  } else if (completing?.kind === 'symbol') {
    const name = completing.project.name;
    if (completing.project.state === 'indexing') info = `${name} is still being indexed; its symbols come when it is done.`;
    else if (completing.query === '') info = `Type a symbol name in ${name}.`;
    else if (symbols.key !== symbolKey || symbols.hits === null) info = 'Searching…';
    else if (symbols.error) info = `Symbols of ${name} could not be searched: ${symbols.error}`;
    else if (symbols.hits.length === 0) info = `No symbol in ${name} matches “${completing.query}”.`;
    else {
      choices = symbols.hits.map((hit, index) => ({
        key: `${hit.path}:${hit.start_line}:${index}`,
        label: `@${name}:${symbolText(hit)}`,
        note: `${hit.kind} · ${hit.path}:${hit.start_line}`,
        insert: `@${name}:${symbolText(hit)}`,
      }));
    }
  }
  const open = completing !== null && (choices.length > 0 || info !== '');
  const highlighted = Math.min(active, Math.max(0, choices.length - 1));

  const take = (choice: Choice) => {
    if (!completing) return;
    const next = complete(draft, completing, choice.insert);
    nextCaret.current = next.caret;
    setActive(0);
    onDraft(next.text);
  };

  const ready = making.length === 0;
  const canSend = (draft.trim() !== '' || attachments.length > 0) && ready && !sending && !disabled;
  const send = async () => {
    const text = draft.trim();
    if (!canSend) return;
    setSending(true);
    try {
      await onSend(text, target, attachments);
      onDraft('');
      onAttachments(() => []);
      setProblems([]);
    } catch {
      // The conversation shows why it was not sent; the draft stays for another try.
    } finally {
      setSending(false);
      area.current?.focus({ preventScroll: true });
    }
  };

  const items: MenuItem[] = options.map((option) => ({
    key: targetKey(option),
    label: targetLabel(option),
    note: option.type === 'turn'
      ? 'Start a new turn in this chat'
      : option.kind === 'steer' ? 'Taken in at the next step of the run' : 'Runs after the current work ends',
    selected: targetKey(option) === targetKey(target),
    onSelect: () => setChosen(option),
  }));

  return (
    <div className="composer cx-composer" data-target={targetKey(target)} data-dragging={dragging ? 'true' : undefined}
      onDragOver={onDragOver} onDragLeave={() => setDragging(false)} onDrop={onDrop}>
      {(attachments.length > 0 || making.length > 0) && (
        <ul className="attached cx-attached" aria-label="Attachments">
          {attachments.map((attachment, index) => attachment.pasted_text ? (
            <li key={`${attachment.path}:${index}`} className="chip cx-chip" data-kind="pasted-text"
              title={`${attachment.name}: ${attachment.path}`}>
              <span className="cx-chip-name">Pasted text · {(attachment.lines ?? 0).toLocaleString('en-US')} lines</span>
              <span className="cx-chip-size">{humanSize(attachment.bytes)}</span>
              {pastedTexts.current.has(attachment.path) && (
                <>
                  <button type="button" aria-expanded={viewing === attachment.path} aria-label={`View ${attachment.name}`}
                    onClick={() => setViewing(viewing === attachment.path ? null : attachment.path)}>View</button>
                  <button type="button" aria-label={`Put ${attachment.name} back into the message`} title="Put the text back into the message"
                    onClick={() => inline(index)}>Inline</button>
                </>
              )}
              <button type="button" aria-label={`Remove ${attachment.name}`} onClick={() => remove(index)}>×</button>
            </li>
          ) : (
            <li key={`${attachment.path}:${index}`} className="chip cx-chip" data-kind={attachment.kind}
              title={`${attachment.path}${attachment.stored ? '' : ' (your file, not copied)'}`}>
              {attachment.thumb && <img className="cx-thumb" src={attachment.thumb} alt="" />}
              <span className="cx-chip-kind">{attachment.kind}</span>
              <span className="cx-chip-name">{attachment.name}</span>
              {attachment.kind !== 'folder' && <span className="cx-chip-size">{humanSize(attachment.bytes)}</span>}
              <button type="button" aria-label={`Remove ${attachment.name}`} onClick={() => remove(index)}>×</button>
            </li>
          ))}
          {making.map((entry) => (
            <li key={`making-${entry.id}`} className="chip cx-chip" data-making="true" aria-busy="true">
              <span className="cx-chip-name">{entry.name}</span>
              <span className="cx-chip-size">attaching…</span>
            </li>
          ))}
        </ul>
      )}
      {viewing !== null && pastedTexts.current.has(viewing) && (
        <pre className="run-output cx-pasted-view" aria-label="Pasted text">{pastedTexts.current.get(viewing)}</pre>
      )}
      {problems.length > 0 && (
        <div className="cx-attach-problems" role="alert">
          {problems.map((problem, index) => <p key={index} className="inline-error">{problem}</p>)}
          <button className="detent" type="button" onClick={() => setProblems([])}>Dismiss</button>
        </div>
      )}
      {open && (
        <div className="menu open cx-mentions" id="cx-mentions" role="listbox" aria-label="Mention">
          {choices.map((choice, index) => (
            <button key={choice.key} type="button" role="option" aria-selected={index === highlighted ? 'true' : 'false'}
              onMouseDown={(event) => event.preventDefault()} onClick={() => take(choice)}>
              <span className="mk" />
              <span className="ml">{choice.label}</span>
              <span className="mh">{choice.note}</span>
            </button>
          ))}
          {info && <p className="hint cx-mentions-info">{info}</p>}
        </div>
      )}
      <textarea
        ref={area}
        className="write"
        rows={1}
        value={draft}
        disabled={disabled}
        placeholder={placeholder}
        aria-label="Message"
        onChange={(event) => {
          setCaret(event.target.selectionStart ?? event.target.value.length);
          setActive(0);
          onDraft(event.target.value);
        }}
        onSelect={(event) => setCaret(event.currentTarget.selectionStart ?? draft.length)}
        onPaste={onPaste}
        aria-autocomplete={mentions ? 'list' : undefined}
        aria-expanded={mentions ? open : undefined}
        aria-controls={open ? 'cx-mentions' : undefined}
        onKeyDown={(event) => {
          // The mention completion takes the keys it uses while it is open.
          if (open && completing && !event.nativeEvent.isComposing) {
            if (event.key === 'Escape') {
              event.preventDefault();
              setDismissed(completing.from);
              return;
            }
            if (choices.length > 0 && (event.key === 'ArrowDown' || event.key === 'ArrowUp')) {
              event.preventDefault();
              setActive((highlighted + (event.key === 'ArrowDown' ? 1 : choices.length - 1)) % choices.length);
              return;
            }
            if (choices.length > 0 && (event.key === 'Tab' || (event.key === 'Enter' && !event.shiftKey))) {
              event.preventDefault();
              take(choices[highlighted]!);
              return;
            }
          }
          if (event.key === 'Enter' && !event.shiftKey && !event.nativeEvent.isComposing) {
            event.preventDefault();
            void send();
            return;
          }
          // Backspace in an empty field takes back the last attachment.
          if (event.key === 'Backspace' && draft === '' && attachments.length > 0) {
            event.preventDefault();
            remove(attachments.length - 1);
          }
        }}
      />
      <input ref={picker} type="file" multiple hidden aria-hidden="true" tabIndex={-1} className="cx-attach-input"
        onChange={(event) => {
          const files = Array.from(event.currentTarget.files ?? []);
          event.currentTarget.value = '';
          add(files.map((file) => ({ type: 'file', file, pasted: false })));
        }} />
      <div className="tools" role="group" aria-label="Chat tools">
        <div className="seg-set" role="group" aria-label="Mode">
          <button type="button" aria-pressed={mode === 'ask'} onClick={() => onMode('ask')}
            title="Ask before anything that leaves this machine or cannot be undone">Ask</button>
          <button type="button" aria-pressed={mode === 'auto'} onClick={() => onMode('auto')}
            title="Never ask: every local change can be undone">Auto</button>
        </div>
        <div className="seg-set cx-attach" role="group" aria-label="Attach">
          <button type="button" disabled={disabled} onClick={() => picker.current?.click()}
            title="Attach files or images (or paste a screenshot, or drop files and folders here)">Attach</button>
          <button type="button" disabled={disabled || !attach.shell} onClick={() => void pickFolder()}
            title={attach.shell ? 'Attach a folder: the model gets its path and its top-level listing' : 'Folders can be attached in the CortexCode app'}>Folder</button>
        </div>
        {target.type === 'turn' && onSpecMarked && (
          <div className="seg-set cx-spec-mark" role="group" aria-label="Spec">
            <button type="button" aria-pressed={specMarked} onClick={() => onSpecMarked(!specMarked)}
              title="Keep this message as the conversation's spec: its requirements become a checklist the work is held to">Spec</button>
          </div>
        )}
        {options.length > 1 && (
          <Menu label="To" value={targetLabel(target)} items={items} title="Where this message goes" className="cx-target" />
        )}
        <div className="grow" />
        {running && !agentSession && (
          <button className="detent warn" type="button" onClick={onStop} disabled={stopping}>
            {stopping ? 'Stopping…' : 'Stop'}
          </button>
        )}
        <button className="detent send" type="button" disabled={!canSend} onClick={() => void send()}
          title={ready ? undefined : 'Waiting for attachments'}>
          {sending ? 'Sending…' : target.type === 'turn' ? 'Send' : target.kind === 'steer' ? 'Steer' : 'Queue'}
        </button>
      </div>
    </div>
  );
}
