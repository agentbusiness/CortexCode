import { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import Markdown from 'react-markdown';
import remarkGfm from 'remark-gfm';
import { Compartment, EditorState, type Extension } from '@codemirror/state';
import { EditorView, hoverTooltip } from '@codemirror/view';
import { redo, redoDepth, undo, undoDepth } from '@codemirror/commands';
import { LanguageDescription } from '@codemirror/language';
import { languages } from '@codemirror/language-data';
import { MergeView, getChunks, unifiedMergeView } from '@codemirror/merge';
import {
  Menu,
  ago,
  basename,
  dirname,
  errorMessage,
  hunkAtLine,
  openInCode,
  openTarget,
  runtime,
  setShared,
  subscribeTheme,
  activeTheme,
  useClient,
  useCodeBase,
  useCodeOptions,
  useCodeScope,
  useCodeTarget,
  useHost,
  useQueries,
  useQuery,
  useSelection,
  useSessions,
  useSlotState,
  type AgentRecord,
  type CodeOpen,
  type JournalEntry,
  type Task,
  type VcsStatus,
  showPane,
} from '@cortex/sdk';
import { basics, themeExtensions } from './cm';
import { loadFile, type FileModel } from './load';
import { attributeHunks, editsFromJournal, sessionLabel, type Edit } from './attribution';
import { actionsRef, annotationField, setAnnotations, type Annotation, type CallSiteNote } from './annotations';


interface ImpactSymbol { sid: string; kind: string; name: string; path: string; start_line: number }
interface ImpactSite { caller: ImpactSymbol; callee: ImpactSymbol; path: string; line: number }
interface Impact { changed: ImpactSymbol[]; call_sites: ImpactSite[]; tests: ImpactSymbol[]; tasks?: string[] }
interface Checkpoint { id: string; label: string; ts_ms: number }

type SaveState = 'saved' | 'dirty' | 'saving' | 'error';

/** The editor for one file: the whole file, the inline diff with per-change controls, and what
 *  the journal and the code graph know about it. */
export function FileEditor({ open, active }: { open: CodeOpen; active: boolean }) {
  const client = useClient();
  const host = useHost();
  const { projectId, project } = useSelection();
  const { setTarget } = useCodeTarget();
  const [scope] = useCodeScope();
  const [base] = useCodeBase();
  const [options, setOptions] = useCodeOptions();
  const [scrolls, setScrolls] = useSlotState<Record<string, number>>('scroll', {});
  const [openSymbols, setOpenSymbols] = useState<string[]>([]);
  const [model, setModel] = useState<FileModel | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [save, setSave] = useState<SaveState>('saved');
  const [depth, setDepth] = useState({ undo: 0, redo: 0 });
  const [chunks, setChunks] = useState(0);
  const [stale, setStale] = useState(false);
  const [confirmRevert, setConfirmRevert] = useState(false);
  const [busy, setBusy] = useState(false);
  const [reloads, setReloads] = useState(0);
  const element = useRef<HTMLDivElement>(null);
  const view = useRef<EditorView | null>(null);
  const merge = useRef<MergeView | null>(null);
  const savedText = useRef('');
  const themeCompartment = useRef(new Compartment());
  const language = useRef(new Compartment());
  const root = open.root;
  const path = open.path;
  const markdown = /\.(md|markdown|mdx)$/i.test(path);

  // ─── the file in this scope ───────────────────────────────────────────────────────────
  useEffect(() => {
    let cancelled = false;
    setError(null);
    loadFile(client, root, path, scope, base)
      .then((loaded) => {
        if (!cancelled) {
          setModel(loaded);
          setStale(false);
        }
      })
      .catch((cause) => {
        if (!cancelled) {
          setModel(null);
          setError(`Could not open ${path}: ${errorMessage(cause)}`);
        }
      });
    return () => {
      cancelled = true;
    };
  }, [client, root, path, scope, base, reloads]);

  // Disk or git changed under the open file: take the new state, unless there are unsaved edits.
  const saveRef = useRef(save);
  saveRef.current = save;
  useEffect(() => {
    const offs = ['tools.files_changed', 'vcs.changed'].map((topic) => client.on(topic, (_t, data) => {
      const record = (data ?? {}) as { root?: string; workspace?: string; paths?: string[] };
      const about = record.root ?? record.workspace;
      if (about && about !== root) return;
      if (record.paths && !record.paths.some((p) => p === path || p.endsWith(`/${path}`))) return;
      if (saveRef.current === 'saved') setReloads((n) => n + 1);
      else setStale(true);
    }));
    return () => offs.forEach((off) => off());
  }, [client, root, path]);

  // ─── what the journal and the graph know ──────────────────────────────────────────────
  const sessions = useSessions(projectId);
  const relevant = (sessions.data ?? []).filter((session) => session.workspace === root);
  const journals = useQueries<Edit[]>(
    relevant.map((session) => ({
      key: `code.edits:${session.id}`,
      fetch: async (c) => editsFromJournal(session.id, (await c.call<{ entries: JournalEntry[] }>('frame.journal.read', { session_id: session.id })).entries),
    })),
    { topics: ['tools.files_changed'] },
  );
  const edits = useMemo(() => journals.flatMap((query) => query.data ?? []), [journals]);
  const attributions = useMemo(() => (model ? attributeHunks(path, model.hunks, edits) : []), [model, path, edits]);
  const workspace = project && project.root !== root ? root : undefined;
  const impact = useQuery<Impact>(
    projectId && model && model.hunks.length > 0 ? `memory.impact:${root}:${path}:${model.added}:${model.removed}` : null,
    (c) => c.call<Impact>('memory.impact', { project_id: projectId, ...(workspace ? { workspace } : {}), paths: [path], depth: 1 }),
  );
  const tasks = useQuery<Task[]>(projectId ? `memory.task.list:${projectId}` : null, async (c) => (await c.call<{ tasks: Task[] }>('memory.task.list', { project_id: projectId }))?.tasks ?? [], { topics: ['memory.task.changed'] });
  const checkpoints = useQuery<Checkpoint[]>(`vcs.checkpoints:${root}`, async (c) => (await c.call<{ checkpoints: Checkpoint[] }>('vcs.checkpoints', { root }))?.checkpoints ?? [], { topics: ['vcs.changed'] });
  const agents = useQuery<AgentRecord[]>(projectId ? `agents.list:${projectId}` : null, async (c) => (await c.call<{ agents: AgentRecord[] }>('agents.list', { project_id: projectId }))?.agents ?? [], { topics: ['agents.changed'] });
  const others = [
    ...(project && project.root !== root ? [{ owner: 'your tree', root: project.root, agent: null as string | null }] : []),
    ...(agents.data ?? []).filter((agent) => agent.workspace && agent.workspace !== root).map((agent) => ({ owner: `@${agent.name}`, root: agent.workspace!, agent: agent.name })),
  ];
  const otherStatus = useQueries<VcsStatus>(others.map((other) => ({ key: `vcs.status:${other.root}`, fetch: (c) => c.call<VcsStatus>('vcs.status', { root: other.root }) })), { topics: ['vcs.changed', 'tools.files_changed'] });
  const alsoChanged = others.filter((_other, index) => (otherStatus[index]?.data?.files ?? []).some((file) => file.path === path));

  const sessionOf = (id: string) => (sessions.data ?? []).find((session) => session.id === id);
  const why = (tasks.data ?? []).find((task) => (task.touches ?? []).some((touch) => touch === path || touch.endsWith(`/${path}`)))
    ?? (attributions.find(Boolean) ? (tasks.data ?? []).find((task) => task.session_id && task.session_id === attributions.find(Boolean)!.edit.sessionId) : undefined);
  const testsReached = impact.data?.tests ?? [];

  // ─── annotations: who made each change; what each changed symbol reaches ─────────────────
  const notes = useMemo<Annotation[]>(() => {
    if (!model || !options.marks) return [];
    const list: Annotation[] = [];
    model.hunks.forEach((hunk, index) => {
      const attribution = attributions[index];
      if (!attribution) return;
      const session = sessionOf(attribution.edit.sessionId);
      list.push({
        kind: 'hunk', line: Math.max(1, hunk.new_lines === 0 ? hunk.new_start + 1 : hunk.new_start), index,
        who: sessionLabel(session), turn: attribution.edit.turn, when: ago(attribution.edit.ts), exact: attribution.exact,
        sessionId: attribution.edit.sessionId, seq: attribution.edit.seq,
      });
    });
    for (const symbol of impact.data?.changed ?? []) {
      if (symbol.path !== path && !symbol.path.endsWith(`/${path}`)) continue;
      const sites: CallSiteNote[] = (impact.data?.call_sites ?? []).filter((site) => site.callee.sid === symbol.sid).map((site) => ({ path: site.path, line: site.line, caller: site.caller.name }));
      list.push({ kind: 'symbol', line: symbol.start_line, sid: symbol.sid, name: symbol.name, symbolKind: symbol.kind, sites, tests: (impact.data?.tests ?? []).length, open: openSymbols.includes(symbol.sid) });
    }
    return list.sort((a, b) => a.line - b.line);
    // `sessionOf` reads sessions.data, a dependency.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [model, attributions, impact.data, openSymbols, options.marks, sessions.data, path]);

  actionsRef.current = {
    showStep: (sessionId, seq) => void openTarget({ session_id: sessionId, seq }),
    toggleSymbol: (sid) => setOpenSymbols((list) => (list.includes(sid) ? list.filter((entry) => entry !== sid) : [...list, sid])),
    openSite: (sitePath, line) => openInCode({ root, path: sitePath, line }),
  };

  // ─── git actions on one change ─────────────────────────────────────────────────────────
  const act = useCallback(async (method: string, params: Record<string, unknown>, done: string) => {
    if (saveRef.current !== 'saved') {
      host.notify('Save or reload your edits first: git acts on the file on disk.');
      return;
    }
    setBusy(true);
    try {
      await client.call(method, { root, ...params });
      host.notify(done);
      runtime().queries.invalidate('code.diff:');
      runtime().queries.invalidate(`vcs.status:${root}`);
      setReloads((n) => n + 1);
    } catch (cause) {
      host.notify(errorMessage(cause));
    } finally {
      setBusy(false);
    }
  }, [client, host, root]);

  const modelRef = useRef(model);
  modelRef.current = model;
  const controlFor = useCallback((type: 'accept' | 'reject', _action: (event: MouseEvent) => void): HTMLElement => {
    const current = modelRef.current;
    const button = document.createElement('button');
    button.type = 'button';
    if (!current || current.controls === 'none' || (current.controls === 'unstage' && type === 'reject')) {
      button.hidden = true;
      return button;
    }
    const label = current.controls === 'unstage' ? 'Unstage' : type === 'accept' ? 'Stage' : 'Revert';
    button.className = `detent${type === 'reject' ? ' warn' : ''}`;
    button.textContent = label;
    button.dataset.action = label.toLowerCase();
    button.title = label === 'Stage' ? 'Stage this change (it leaves the pending diff)' : label === 'Revert' ? 'Discard this change on disk (asks once)' : 'Take this change out of the index';
    let armed = false;
    button.onmousedown = (event) => {
      event.preventDefault();
      const editor = view.current ?? merge.current?.b;
      const model = modelRef.current;
      if (!editor || !model) return;
      if (label === 'Revert' && !armed) {
        armed = true;
        button.textContent = 'Discard?';
        setTimeout(() => {
          armed = false;
          button.textContent = 'Revert';
        }, 3000);
        return;
      }
      const line = editor.state.doc.lineAt(editor.posAtDOM(button)).number;
      const index = hunkAtLine(model.hunks, line);
      const params = model.untracked ? { paths: [path] } : index >= 0 ? { paths: [path], hunk: index } : null;
      if (!params) {
        host.notify('This change no longer matches git’s view of the file; refreshing.');
        setReloads((n) => n + 1);
        return;
      }
      if (label === 'Stage') void act('vcs.stage', params, `Staged a change in ${basename(path)}`);
      else if (label === 'Unstage') void act('vcs.unstage', params, `Unstaged a change in ${basename(path)}`);
      else void act('vcs.revert', params, model.untracked ? `Deleted ${path}` : `Reverted a change in ${basename(path)}`);
    };
    return button;
  }, [act, host, path]);

  // ─── the editor itself ───────────────────────────────────────────────────────────────
  const doSave = useCallback(async () => {
    const editor = view.current ?? merge.current?.b;
    if (!editor) return;
    const content = editor.state.doc.toString();
    setSave('saving');
    try {
      await client.call('tools.save', { workspace: root, path, content });
      savedText.current = content;
      setSave('saved');
      setError(null);
    } catch (cause) {
      setSave('error');
      setError(`Not saved: ${errorMessage(cause)}`);
    }
  }, [client, root, path]);

  useEffect(() => {
    const parent = element.current;
    if (!parent || !model || (markdown && options.rendered)) return;
    savedText.current = model.doc;
    const hover = hoverTooltip(async (editorView, pos) => {
      if (!projectId) return null;
      const word = editorView.state.wordAt(pos);
      if (!word) return null;
      const name = editorView.state.sliceDoc(word.from, word.to);
      if (!/^[A-Za-z_$][\w$]*$/.test(name)) return null;
      try {
        const result = await runtime().client.call<{ matches: Array<{ symbol: { kind: string; qname: string; signature: string; description?: string | null; path: string; start_line: number }; callers: number; callees: number }> }>('memory.symbol', { project_id: projectId, ...(workspace ? { workspace } : {}), name });
        const match = result.matches[0];
        if (!match) return null;
        return {
          pos: word.from,
          end: word.to,
          above: true,
          create: () => {
            const dom = document.createElement('div');
            dom.className = 'cx-symbol-tip';
            const head = document.createElement('div');
            head.className = 'ap';
            head.textContent = `${match.symbol.kind} · ${match.callers} caller${match.callers === 1 ? '' : 's'} · ${match.callees} callee${match.callees === 1 ? '' : 's'}`;
            const signature = document.createElement('code');
            signature.textContent = match.symbol.signature || match.symbol.qname;
            dom.append(head, signature);
            if (match.symbol.description) {
              const description = document.createElement('p');
              description.textContent = match.symbol.description;
              dom.appendChild(description);
            }
            const where = document.createElement('div');
            where.className = 'ap faint';
            where.textContent = `${match.symbol.path}:${match.symbol.start_line}`;
            dom.appendChild(where);
            return { dom };
          },
        };
      } catch {
        return null;
      }
    }, { hoverTime: 400 });
    const shared: Extension[] = [...basics(), themeCompartment.current.of(themeExtensions(activeTheme())), language.current.of([]), hover, ...(options.wrap ? [EditorView.lineWrapping] : [])];
    const listener = EditorView.updateListener.of((update) => {
      const state = update.state;
      setChunks(getChunks(state)?.chunks.length ?? 0);
      const undo = undoDepth(state);
      const redo = redoDepth(state);
      setDepth((current) => (current.undo === undo && current.redo === redo ? current : { undo, redo }));
      if (update.docChanged) setSave(state.doc.toString() === savedText.current ? 'saved' : 'dirty');
    });
    const scroll = EditorView.domEventHandlers({
      scroll: (_event, editorView) => {
        const top = editorView.scrollDOM.scrollTop;
        setScrolls((all) => ({ ...all, [model.key]: top }));
      },
    });
    const editable = [EditorState.readOnly.of(!model.editable), EditorView.editable.of(model.editable)];
    let editor: EditorView;
    if (options.split && model.original !== null && options.marks) {
      const view2 = new MergeView({
        parent,
        a: { doc: model.original, extensions: [...shared, EditorState.readOnly.of(true), EditorView.editable.of(false)] },
        b: { doc: model.doc, extensions: [...shared, ...editable, listener, scroll, annotationField] },
        highlightChanges: true,
        gutter: true,
      });
      merge.current = view2;
      editor = view2.b;
    } else {
      const extensions: Extension[] = [...shared, ...editable, listener, scroll, annotationField];
      if (model.original !== null && options.marks) {
        extensions.push(unifiedMergeView({ original: model.original, mergeControls: model.controls === 'none' ? false : controlFor, gutter: true, highlightChanges: true, syntaxHighlightDeletions: true }));
      }
      editor = new EditorView({ parent, state: EditorState.create({ doc: model.doc, extensions }) });
      view.current = editor;
    }
    setSave('saved');
    setChunks(getChunks(editor.state)?.chunks.length ?? 0);
    const description = LanguageDescription.matchFilename(languages, basename(path));
    if (description) {
      void description.load().then((support) => {
        for (const target of merge.current ? [merge.current.a, merge.current.b] : [editor]) target.dispatch({ effects: language.current.reconfigure(support) });
      }).catch(() => undefined);
    }
    const offTheme = subscribeTheme(() => {
      for (const target of merge.current ? [merge.current.a, merge.current.b] : [editor]) target.dispatch({ effects: themeCompartment.current.reconfigure(themeExtensions(activeTheme())) });
    });
    // Bring the first change (or where the reader was) into view.
    requestAnimationFrame(() => {
      const line = open.line ?? null;
      if (line && line <= editor.state.doc.lines) {
        const pos = editor.state.doc.line(line).from;
        editor.dispatch({ selection: { anchor: pos }, effects: EditorView.scrollIntoView(pos, { y: 'center' }) });
      } else if (scrolls[model.key]) {
        editor.scrollDOM.scrollTo({ top: scrolls[model.key] });
      }
    });
    const observer = new ResizeObserver(() => editor.requestMeasure());
    observer.observe(parent);
    return () => {
      observer.disconnect();
      offTheme();
      merge.current?.destroy();
      merge.current = null;
      if (view.current) view.current.destroy();
      view.current = null;
    };
    // Rebuilt for a new model or another way of showing it.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [model, options.split, options.marks, options.rendered, options.wrap]);

  useEffect(() => {
    const editor = view.current ?? merge.current?.b;
    editor?.dispatch({ effects: setAnnotations.of(notes) });
  }, [notes]);

  useEffect(() => {
    if (active) (view.current ?? merge.current?.b)?.requestMeasure();
  }, [active]);

  // ─── header ───────────────────────────────────────────────────────────────────────────
  const dir = dirname(path);
  const shortDir = dir.split('/').length > 2 ? `…/${dir.split('/').slice(-2).join('/')}` : dir;
  const editor = view.current ?? merge.current?.b ?? null;
  return (
    <div className="cx-file" data-path={path} data-chunks={chunks} data-save={save} data-scope={scope} data-build={__CORTEX_BUILD__}>
      <div className="cx-file-head">
        <span className="cx-file-path" title={`${root}/${path}`}>
          {shortDir && <span className="cx-file-dir">{shortDir}/</span>}
          <b className="cx-file-name">{basename(path)}</b>
        </span>
        {model && (model.added > 0 || model.removed > 0) && (
          <span className="cx-file-counts">
            <span className="ap cx-plus">+{model.added}</span>
            <span className="ap cx-minus">−{model.removed}</span>
          </span>
        )}
        {model && model.staged > 0 && <span className="ap faint" title="Lines already in the index (see the Staged scope)">{model.staged} staged</span>}
        <span className={`ap ${save === 'dirty' ? 'hot' : save === 'error' ? 'live' : 'faint'}`}>
          {save === 'saved' ? (model?.editable ? 'Saved' : 'Read-only') : save === 'dirty' ? 'Unsaved' : save === 'saving' ? 'Saving…' : 'Not saved'}
        </span>
        <span className="grow" />
        {markdown && (
          <div className="seg-set" role="group" aria-label="Markdown">
            <button type="button" aria-pressed={!options.rendered} onClick={() => setOptions({ rendered: false })}>Source</button>
            <button type="button" aria-pressed={options.rendered} onClick={() => setOptions({ rendered: true })}>Rendered</button>
          </div>
        )}
        <div className="seg-set" role="group" aria-label="Changes">
          <button type="button" aria-pressed={options.marks} onClick={() => setOptions({ marks: true })} title="The file with its changes marked">Diff</button>
          <button type="button" aria-pressed={!options.marks} onClick={() => setOptions({ marks: false })} title="The plain file">File</button>
        </div>
        <span className="cx-break" />
        {/* Editing controls only where the file can be edited (not a staged, branch or partial view). */}
        {model?.editable && (
          <>
            <button className="detent" type="button" disabled={!depth.undo} onClick={() => editor && undo(editor)}>Undo</button>
            <button className="detent" type="button" disabled={!depth.redo} onClick={() => editor && redo(editor)}>Redo</button>
            <button className="detent send" type="button" disabled={save !== 'dirty' && save !== 'error'} onClick={() => void doSave()}>Save</button>
          </>
        )}
        {host.shell && (
          <button className="detent" type="button" title="Show the file in Finder" onClick={() => void host.shell!.reveal(`${root}/${path}`).then((problem) => problem && host.notify(problem))}>Reveal</button>
        )}
        <Menu
          variant="detent"
          label="More"
          items={[
            { key: 'stage', label: 'Stage the whole file', disabled: busy || !model || model.controls !== 'stage-revert', onSelect: () => void act('vcs.stage', { paths: [path] }, `Staged ${path}`) },
            { key: 'revert', label: model?.untracked ? 'Delete this new file…' : 'Revert the whole file…', disabled: busy || !model || model.controls !== 'stage-revert', onSelect: () => setConfirmRevert(true) },
            { key: 'copy', label: 'Copy the path', note: path, onSelect: () => void navigator.clipboard?.writeText(path) },
            { key: 'reload', label: 'Reload from disk', onSelect: () => setReloads((n) => n + 1) },
            { key: 'wrap', label: options.wrap ? 'Scroll long lines sideways' : 'Wrap long lines', onSelect: () => setOptions({ wrap: !options.wrap }) },
          ]}
        />
      </div>
      {confirmRevert && (
        <div className="notice contradiction cx-file-notice">
          <span className="kindmark" />
          <span className="ap lbl">{model?.untracked ? 'Delete this new file' : 'Discard every unstaged change'}</span>
          <p>{model?.untracked ? `${path} is not tracked; deleting it cannot be undone from git.` : `${path} goes back to its staged text.`}</p>
          <div className="acts">
            <button className="detent warn" type="button" onClick={() => { setConfirmRevert(false); void act('vcs.revert', { paths: [path] }, model?.untracked ? `Deleted ${path}` : `Reverted ${path}`); }}>Discard</button>
            <button className="detent" type="button" onClick={() => setConfirmRevert(false)}>Keep</button>
          </div>
        </div>
      )}
      {(why || testsReached.length > 0 || alsoChanged.length > 0 || model?.note) && (
        <div className="cx-file-facts">
          {why && (
            <div className="cx-fact cx-fact-line">
              <span className="ap faint">Why</span>
              <button type="button" className="cx-fact-link" title={why.title} onClick={() => { setShared('board.focus', why.id); setShared('board.view', 'tasks'); showPane('board'); }}>
                {why.title}
              </button>
              <span className="ap faint">{why.status.replace(/_/g, ' ')}{(why.contract?.checks.length ?? 0) > 0 ? ` · ${why.contract!.checks.length} contract check${why.contract!.checks.length === 1 ? '' : 's'}` : ''}</span>
            </div>
          )}
          {testsReached.length > 0 && (
            <div className="cx-fact">
              <span className="ap faint">Tests reaching this change</span>
              {testsReached.slice(0, 6).map((test) => (
                <button key={test.sid} type="button" className="cx-file-link" onClick={() => openInCode({ root, path: test.path, line: test.start_line })}>{test.name}</button>
              ))}
              {testsReached.length > 6 && <span className="ap faint">+{testsReached.length - 6}</span>}
            </div>
          )}
          {alsoChanged.length > 0 && (
            <div className="cx-fact cx-conflict">
              <span className="ap hot">Also changed in</span>
              {alsoChanged.map((other) => (
                <button key={other.root} type="button" className="cx-fact-link" title={other.root}
                  onClick={() => setTarget(other.agent ? { root: other.root, label: other.owner, agent: other.agent } : { root: other.root, label: project?.name ?? other.root, agent: null })}>
                  {other.owner}
                </button>
              ))}
              <span className="ap faint">— merging both will need care</span>
            </div>
          )}
          {model?.note && <p className="hint cx-fact">{model.note}</p>}
        </div>
      )}
      {stale && (
        <div className="notice overdue cx-file-notice">
          <span className="kindmark" />
          <span className="ap lbl">Changed on disk</span>
          <p>This file changed while you had unsaved edits.</p>
          <div className="acts">
            <button className="detent" type="button" onClick={() => setReloads((n) => n + 1)}>Load the disk version</button>
            <button className="detent" type="button" onClick={() => void doSave()}>Keep mine and save</button>
          </div>
        </div>
      )}
      {error && <p className="inline-error cx-file-notice" role="alert">{error}</p>}
      {markdown && options.rendered && model ? (
        <div className="cx-file-md prose"><Markdown remarkPlugins={[remarkGfm]}>{model.doc}</Markdown></div>
      ) : (
        <div className="cx-file-host" ref={element} hidden={!model} />
      )}
      {!model && !error && <p className="empty-line">Reading {path}…</p>}
      <Checkpoints root={root} list={checkpoints.data ?? []} onRestored={() => setReloads((n) => n + 1)} />
    </div>
  );
}

/** The tree's checkpoints as ticks on a hairline; pick one to restore the tree to it. */
function Checkpoints({ root, list, onRestored }: { root: string; list: Checkpoint[]; onRestored: () => void }) {
  const client = useClient();
  const host = useHost();
  const [picked, setPicked] = useState<string | null>(null);
  const [armed, setArmed] = useState(false);
  if (list.length === 0) return null;
  const sorted = [...list].sort((a, b) => a.ts_ms - b.ts_ms);
  const first = sorted[0]!.ts_ms;
  const span = Math.max(1, sorted[sorted.length - 1]!.ts_ms - first);
  const chosen = sorted.find((checkpoint) => checkpoint.id === picked) ?? null;
  const restore = async () => {
    if (!chosen) return;
    try {
      const result = await client.call<{ backup: Checkpoint }>('vcs.restore', { root, id: chosen.id });
      host.notify(`Restored “${chosen.label}”; the tree as it was is kept as checkpoint ${result.backup.id.slice(0, 8)}`);
      setArmed(false);
      onRestored();
    } catch (cause) {
      host.notify(errorMessage(cause));
    }
  };
  return (
    <div className="cx-checkpoints" aria-label="Checkpoints">
      <span className="ap faint">Checkpoints</span>
      <div className="cx-scrub">
        {sorted.map((checkpoint) => (
          <button key={checkpoint.id} type="button" className={`cx-tick${checkpoint.id === picked ? ' on' : ''}`}
            style={{ left: `${((checkpoint.ts_ms - first) / span) * 100}%` }}
            title={`${checkpoint.label} · ${new Date(checkpoint.ts_ms).toLocaleString()}`} aria-label={checkpoint.label}
            onClick={() => { setPicked(checkpoint.id); setArmed(false); }} />
        ))}
      </div>
      {chosen ? (
        <>
          <span className="ap">{chosen.label}</span>
          <span className="ap faint">{ago(chosen.ts_ms)}</span>
          <button className={`detent${armed ? ' warn' : ''}`} type="button" onClick={() => (armed ? void restore() : setArmed(true))}>
            {armed ? 'Restore the whole tree' : 'Restore…'}
          </button>
        </>
      ) : (
        <span className="ap faint">{sorted.length} kept</span>
      )}
    </div>
  );
}

