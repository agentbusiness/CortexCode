import { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import Markdown, { type Components } from 'react-markdown';
import remarkGfm from 'remark-gfm';
import { Compartment, EditorState, RangeSet, StateEffect, StateField, type Extension } from '@codemirror/state';
import { EditorView, GutterMarker, gutterLineClass, hoverTooltip } from '@codemirror/view';
import { redo, redoDepth, undo, undoDepth } from '@codemirror/commands';
import { LanguageDescription } from '@codemirror/language';
import { languages } from '@codemirror/language-data';
import {
  Menu,
  aboutRoot,
  basename,
  decodeRead,
  dirname,
  errorMessage,
  runtime,
  setShared,
  subscribeTheme,
  activeTheme,
  useClient,
  useCodeOptions,
  useCodeTarget,
  useHost,
  useQueries,
  useQuery,
  useSelection,
  useShared,
  useSlotState,
  type AgentRecord,
  type CodeOpen,
  type Diff,
  type Hunk,
  type ReadResult,
  type Task,
  type VcsStatus,
  openInCode,
  showPane,
} from '@cortex/sdk';
import { basics, themeExtensions } from './cm';

type SaveState = 'saved' | 'dirty' | 'saving' | 'error';

interface Loaded {
  text: string;
  editable: boolean;
  note: string;
}

/** Lines (1-based, on the file as it is) that the hunks add or change. */
export function changedLines(hunks: readonly Hunk[]): number[] {
  const lines: number[] = [];
  for (const hunk of hunks) {
    let line = hunk.new_start;
    for (const entry of hunk.lines) {
      if (entry.kind === '+') lines.push(entry.new_line ?? line);
      if (entry.kind !== '-') line = (entry.new_line ?? line) + 1;
    }
  }
  return lines;
}

// The numbers of changed lines are marked (orange in the theme's rubric): a gutter class on them.
const changedMarker = new (class extends GutterMarker {
  override elementClass = 'cx-changed-line';
})();
const setChanged = StateEffect.define<number[]>();
const changedField = StateField.define<RangeSet<GutterMarker>>({
  create: () => RangeSet.empty,
  update(set, transaction) {
    let next = set.map(transaction.changes);
    for (const effect of transaction.effects) {
      if (!effect.is(setChanged)) continue;
      const doc = transaction.state.doc;
      const lines = [...new Set(effect.value)].filter((line) => line >= 1 && line <= doc.lines).sort((a, b) => a - b);
      next = RangeSet.of(lines.map((line) => changedMarker.range(doc.line(line).from)));
    }
    return next;
  },
  provide: (field) => gutterLineClass.from(field),
});

/** One file as it is on disk: editable, saved through `tools.save`, its changed lines' numbers
 *  marked, and what the project knows about why it changed. */
export function FileEditor({ open, active }: { open: CodeOpen; active: boolean }) {
  const client = useClient();
  const host = useHost();
  const { projectId, project } = useSelection();
  const { setTarget } = useCodeTarget();
  const [options, setOptions] = useCodeOptions();
  const [scrolls, setScrolls] = useSlotState<Record<string, number>>('scroll', {});
  const [loaded, setLoaded] = useState<Loaded | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [save, setSave] = useState<SaveState>('saved');
  const [depth, setDepth] = useState({ undo: 0, redo: 0 });
  const [stale, setStale] = useState(false);
  const [confirmRevert, setConfirmRevert] = useState(false);
  const [busy, setBusy] = useState(false);
  const [reloads, setReloads] = useState(0);
  const element = useRef<HTMLDivElement>(null);
  const view = useRef<EditorView | null>(null);
  const savedText = useRef('');
  const themeCompartment = useRef(new Compartment());
  const language = useRef(new Compartment());
  const root = open.root;
  const path = open.path;
  const key = `${root}\u0000${path}`;
  const markdown = /\.(md|markdown|mdx)$/i.test(path);
  // Markdown opens drawn, as the Markdown view sets it; Source shows the text to edit.
  const [mdSource, setMdSource] = useShared<boolean>('code.mdSource', false);
  const rendered = markdown && !mdSource;

  // ─── the file on disk ─────────────────────────────────────────────────────────────────
  useEffect(() => {
    let cancelled = false;
    setError(null);
    client
      .call<ReadResult>('tools.read', { workspace: root, target: path })
      .then((result) => {
        if (cancelled) return;
        const read = decodeRead(result, path);
        setLoaded({ text: read.text, editable: read.editable, note: read.editable ? '' : `Read-only: ${read.reason}` });
        setStale(false);
      })
      .catch((cause) => {
        if (cancelled) return;
        const message = errorMessage(cause);
        if (/not.?found|no such file/i.test(message)) setLoaded({ text: '', editable: false, note: 'This file no longer exists on disk.' });
        else {
          setLoaded(null);
          setError(`Could not open ${path}: ${message}`);
        }
      });
    return () => {
      cancelled = true;
    };
  }, [client, root, path, reloads]);

  // Disk or git changed under the open file: take the new text, unless there are unsaved edits.
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

  // ─── what changed: the file against HEAD (everything not committed) ───────────────────
  const changes = useQuery<Hunk[]>(
    `code.filechanges:${root}:${path}`,
    async (c) => {
      const diff = await c.call<Diff>('vcs.diff', { root, scope: 'uncommitted', path });
      return (diff.files.find((file) => file.path === path) ?? diff.files[0])?.hunks ?? [];
    },
    { topics: ['vcs.changed', 'tools.files_changed'], when: aboutRoot(root) },
  );
  const hunks = changes.data ?? [];
  const marked = useMemo(() => changedLines(hunks), [hunks]);
  const counts = useMemo(() => {
    let added = 0;
    let removed = 0;
    for (const hunk of hunks) for (const line of hunk.lines) {
      if (line.kind === '+') added++;
      else if (line.kind === '-') removed++;
    }
    return { added, removed };
  }, [hunks]);

  // ─── why it changed, and who else changed it ──────────────────────────────────────────
  const workspace = project && project.root !== root ? root : undefined;
  const tasks = useQuery<Task[]>(projectId ? `memory.task.list:${projectId}` : null, async (c) => (await c.call<{ tasks: Task[] }>('memory.task.list', { project_id: projectId }))?.tasks ?? [], { topics: ['memory.task.changed'] });
  const agents = useQuery<AgentRecord[]>(projectId ? `agents.list:${projectId}` : null, async (c) => (await c.call<{ agents: AgentRecord[] }>('agents.list', { project_id: projectId }))?.agents ?? [], { topics: ['agents.changed'] });
  const others = [
    ...(project && project.root !== root ? [{ owner: 'your tree', root: project.root, agent: null as string | null }] : []),
    ...(agents.data ?? []).filter((agent) => agent.workspace && agent.workspace !== root).map((agent) => ({ owner: `@${agent.name}`, root: agent.workspace!, agent: agent.name })),
  ];
  const otherStatus = useQueries<VcsStatus>(others.map((other) => ({ key: `vcs.status:${other.root}`, fetch: (c) => c.call<VcsStatus>('vcs.status', { root: other.root }) })), { topics: ['vcs.changed', 'tools.files_changed'] });
  const alsoChanged = others.filter((_other, index) => (otherStatus[index]?.data?.files ?? []).some((file) => file.path === path));
  const why = hunks.length > 0
    ? (tasks.data ?? []).find((task) => task.status !== 'cancelled' && (task.touches ?? []).some((touch) => touch === path || touch.endsWith(`/${path}`)))
    : undefined;

  // ─── git actions on the whole file ────────────────────────────────────────────────────
  const act = useCallback(async (method: string, params: Record<string, unknown>, done: string) => {
    if (saveRef.current !== 'saved') {
      host.notify('Save or reload your edits first: git acts on the file on disk.');
      return;
    }
    setBusy(true);
    try {
      await client.call(method, { root, ...params });
      host.notify(done);
      runtime().queries.invalidate('code.');
      runtime().queries.invalidate(`vcs.status:${root}`);
      setReloads((n) => n + 1);
    } catch (cause) {
      host.notify(errorMessage(cause));
    } finally {
      setBusy(false);
    }
  }, [client, host, root]);

  const doSave = useCallback(async () => {
    const editor = view.current;
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

  // ─── the editor ───────────────────────────────────────────────────────────────────────
  const markedRef = useRef(marked);
  markedRef.current = marked;
  useEffect(() => {
    const parent = element.current;
    if (!parent || !loaded || rendered) return;
    savedText.current = loaded.text;
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
    const listener = EditorView.updateListener.of((update) => {
      const state = update.state;
      const undoCount = undoDepth(state);
      const redoCount = redoDepth(state);
      setDepth((current) => (current.undo === undoCount && current.redo === redoCount ? current : { undo: undoCount, redo: redoCount }));
      if (update.docChanged) setSave(state.doc.toString() === savedText.current ? 'saved' : 'dirty');
    });
    const scroll = EditorView.domEventHandlers({
      scroll: (_event, editorView) => {
        const top = editorView.scrollDOM.scrollTop;
        setScrolls((all) => ({ ...all, [key]: top }));
      },
    });
    const extensions: Extension[] = [
      ...basics(),
      themeCompartment.current.of(themeExtensions(activeTheme())),
      language.current.of([]),
      hover,
      changedField,
      listener,
      scroll,
      EditorState.readOnly.of(!loaded.editable),
      EditorView.editable.of(loaded.editable),
      ...(options.wrap ? [EditorView.lineWrapping] : []),
    ];
    const editor = new EditorView({ parent, state: EditorState.create({ doc: loaded.text, extensions }) });
    view.current = editor;
    editor.dispatch({ effects: setChanged.of(markedRef.current) });
    setSave('saved');
    const description = LanguageDescription.matchFilename(languages, basename(path));
    if (description) void description.load().then((support) => editor.dispatch({ effects: language.current.reconfigure(support) })).catch(() => undefined);
    const offTheme = subscribeTheme(() => editor.dispatch({ effects: themeCompartment.current.reconfigure(themeExtensions(activeTheme())) }));
    // Where the reader was, unless the file was opened at a line (handled below).
    requestAnimationFrame(() => {
      if (!open.line && scrolls[key]) editor.scrollDOM.scrollTo({ top: scrolls[key] });
    });
    const observer = new ResizeObserver(() => editor.requestMeasure());
    observer.observe(parent);
    return () => {
      observer.disconnect();
      offTheme();
      editor.destroy();
      view.current = null;
    };
    // Rebuilt for new text or another way of showing it.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [loaded, rendered, options.wrap]);

  // The changed lines follow the diff.
  useEffect(() => {
    view.current?.dispatch({ effects: setChanged.of(marked) });
  }, [marked]);

  // Opened at a line (a chat's edit, a diff's file name, a tree row): bring it to the middle.
  useEffect(() => {
    const editor = view.current;
    const line = open.line ?? null;
    if (!editor || !line) return;
    requestAnimationFrame(() => {
      if (line > editor.state.doc.lines) return;
      const pos = editor.state.doc.line(line).from;
      editor.dispatch({ selection: { anchor: pos }, effects: EditorView.scrollIntoView(pos, { y: 'center' }) });
    });
  }, [open, loaded]);

  useEffect(() => {
    if (active) view.current?.requestMeasure();
  }, [active]);

  // ─── header ───────────────────────────────────────────────────────────────────────────
  const dir = dirname(path);
  const shortDir = dir.split('/').length > 2 ? `…/${dir.split('/').slice(-2).join('/')}` : dir;
  const editor = view.current;
  const showChanges = () => {
    setShared('code.diffFocus', { path, at: Date.now() });
    setShared('code.view', 'changes');
  };
  return (
    <div className="cx-file" data-path={path} data-changed={marked.length} data-save={save} data-build={__CORTEX_BUILD__}>
      <div className="cx-file-head">
        <span className="cx-file-path" title={`${root}/${path}`}>
          {shortDir && <span className="cx-file-dir">{shortDir}/</span>}
          <b className="cx-file-name">{basename(path)}</b>
        </span>
        {(counts.added > 0 || counts.removed > 0) && (
          <button type="button" className="cx-file-counts" onClick={showChanges} title="See this file's changes as a diff">
            <span className="ap cx-plus">+{counts.added}</span>
            <span className="ap cx-minus">−{counts.removed}</span>
          </button>
        )}
        <span className={`ap ${save === 'dirty' ? 'hot' : save === 'error' ? 'live' : 'faint'}`}>
          {save === 'saved' ? (loaded?.editable ? 'Saved' : 'Read-only') : save === 'dirty' ? 'Unsaved' : save === 'saving' ? 'Saving…' : 'Not saved'}
        </span>
        <span className="grow" />
        {markdown && (
          <div className="seg-set" role="group" aria-label="Markdown">
            <button type="button" aria-pressed={rendered} onClick={() => setMdSource(false)}>Rendered</button>
            <button type="button" aria-pressed={!rendered} onClick={() => setMdSource(true)}>Source</button>
          </div>
        )}
        {loaded?.editable && !rendered && (
          <button className="detent send" type="button" disabled={save !== 'dirty' && save !== 'error'} onClick={() => void doSave()} title="Save (the file is edited in place)">Save</button>
        )}
        <Menu
          variant="detent"
          label="More"
          items={[
            ...(loaded?.editable && !rendered ? [
              { key: 'undo', label: 'Undo', note: '⌘Z', disabled: !depth.undo, onSelect: () => { if (editor) undo(editor); } },
              { key: 'redo', label: 'Redo', note: '⇧⌘Z', disabled: !depth.redo, onSelect: () => { if (editor) redo(editor); } },
            ] : []),
            { key: 'changes', label: 'See this file’s changes', disabled: hunks.length === 0, onSelect: showChanges },
            { key: 'stage', label: 'Stage the whole file', disabled: busy || hunks.length === 0, onSelect: () => void act('vcs.stage', { paths: [path] }, `Staged ${path}`) },
            { key: 'revert', label: 'Discard this file’s changes…', disabled: busy || hunks.length === 0, onSelect: () => setConfirmRevert(true) },
            { key: 'wrap', label: options.wrap ? 'Scroll long lines sideways' : 'Wrap long lines', onSelect: () => setOptions({ wrap: !options.wrap }) },
            { key: 'reload', label: 'Reload from disk', onSelect: () => setReloads((n) => n + 1) },
            { key: 'copy', label: 'Copy the path', note: path, onSelect: () => void navigator.clipboard?.writeText(path) },
            ...(host.shell ? [{ key: 'reveal', label: 'Show in Finder', onSelect: () => void host.shell!.reveal(`${root}/${path}`).then((problem) => problem && host.notify(problem)) }] : []),
          ]}
        />
      </div>
      {confirmRevert && (
        <div className="notice contradiction cx-file-notice">
          <span className="kindmark" />
          <span className="ap lbl">Discard this file’s changes</span>
          <p>{path} goes back to its last committed or staged text. This cannot be undone.</p>
          <div className="acts">
            <button className="detent warn" type="button" onClick={() => { setConfirmRevert(false); void act('vcs.revert', { paths: [path] }, `Reverted ${path}`); }}>Discard</button>
            <button className="detent" type="button" onClick={() => setConfirmRevert(false)}>Keep</button>
          </div>
        </div>
      )}
      {(why || alsoChanged.length > 0 || loaded?.note) && (
        <div className="cx-file-facts">
          {why && (
            <div className="cx-fact cx-fact-line">
              <span className="ap faint">Why</span>
              <button type="button" className="cx-fact-link" title={why.title} onClick={() => { setShared('board.focus', why.id); setShared('board.view', 'tasks'); showPane('board'); }}>
                {why.title}
              </button>
              <span className="ap faint">{why.status.replace(/_/g, ' ')}</span>
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
            </div>
          )}
          {loaded?.note && <p className="hint cx-fact">{loaded.note}</p>}
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
      {rendered && loaded ? (
        <div className="cx-file-md">
          <article className="prose cx-md-doc"><Markdown remarkPlugins={[remarkGfm]} components={mdLinks(root, path)}>{loaded.text}</Markdown></article>
        </div>
      ) : (
        <div className="cx-file-host" ref={element} hidden={!loaded} />
      )}
      {!loaded && !error && <p className="empty-line cx-file-notice">Reading {path}…</p>}
    </div>
  );
}

/** Resolve a relative link against the file it appears in (`docs/a.md` + `../b.md` -> `b.md`). */
function resolveRelative(from: string, link: string): string {
  const parts = (link.startsWith('/') ? [] : dirname(from).split('/')).filter(Boolean);
  for (const part of link.replace(/^\//, '').split('/')) {
    if (part === '' || part === '.') continue;
    if (part === '..') parts.pop();
    else parts.push(part);
  }
  return parts.join('/');
}

/** Links in drawn Markdown: web links open in the browser pane, other files open here. */
function mdLinks(root: string, from: string): Components {
  return {
    a: ({ href, children }) => (
      <a href={href} onClick={(event) => {
        event.preventDefault();
        if (!href || href.startsWith('#')) return;
        if (/^https?:/i.test(href)) {
          showPane('browser');
          void runtime().client.call('browser.open', { url: href }).catch(() => undefined);
        } else {
          openInCode({ root, path: resolveRelative(from, href.replace(/#.*$/, '')) });
        }
      }}>{children}</a>
    ),
  };
}
