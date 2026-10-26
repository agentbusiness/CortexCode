/**
 * pane.terminal@1 — xterm.js (WebGL renderer) over the terminal plugin's PTYs (`terminal.*`).
 * Terminals belong to the frame, not to this view: the open terminals of each workspace are kept
 * in the durable slot state, so a swapped or reopened pane re-attaches to the same shells and
 * repaints their screen from `terminal.snapshot`.
 */

import { useEffect, useLayoutEffect, useRef, useState } from 'react';
import { createPortal } from 'react-dom';
import { Terminal } from '@xterm/xterm';
import { FitAddon } from '@xterm/addon-fit';
import { WebglAddon } from '@xterm/addon-webgl';
import '@xterm/xterm/css/xterm.css';
import {
  activeTheme,
  currentGround,
  definePlugin,
  errorMessage,
  runtime,
  subscribeTheme,
  useClient,
  useSelection,
  useSlotState,
  type TermInfo,
  type TermSnapshot,
} from '@cortex/sdk';

/** xterm's palette for the ground on screen, from the active theme plugin. */
function palette() {
  return activeTheme()?.xterm[currentGround()];
}
import './terminal.css';

export function decodeBase64(data: string): Uint8Array {
  const binary = atob(data);
  const bytes = new Uint8Array(binary.length);
  for (let i = 0; i < binary.length; i++) bytes[i] = binary.charCodeAt(i);
  return bytes;
}

function TerminalPane({ active }: { active?: boolean }) {
  const client = useClient();
  const { workspace } = useSelection();
  const [terms, setTerms] = useSlotState<Record<string, string[]>>('terms', {});
  const [current, setCurrent] = useSlotState<Record<string, string>>('current', {});
  const [exited, setExited] = useState<Record<string, number | null>>({});
  const [error, setError] = useState<string | null>(null);
  const [opening, setOpening] = useState(false);
  const [link, setLink] = useState<string | null>(null);
  const list = workspace ? terms[workspace] ?? [] : [];
  const termId = workspace ? current[workspace] ?? list[0] ?? null : null;

  const openTerminal = async () => {
    if (!workspace) return;
    setOpening(true);
    setError(null);
    try {
      const info = await client.call<TermInfo>('terminal.open', { cwd: workspace, cols: 100, rows: 30 });
      setTerms((all) => ({ ...all, [workspace]: [...(all[workspace] ?? []), info.term_id] }));
      setCurrent((all) => ({ ...all, [workspace]: info.term_id }));
    } catch (cause) {
      setError(`No terminal: ${errorMessage(cause)}`);
    } finally {
      setOpening(false);
    }
  };

  const closeTerminal = async (id: string) => {
    if (!workspace) return;
    try {
      await client.call('terminal.close', { term_id: id });
    } catch {
      // Already gone: forget it either way.
    }
    setTerms((all) => ({ ...all, [workspace]: (all[workspace] ?? []).filter((entry) => entry !== id) }));
    setCurrent((all) => {
      const rest = (terms[workspace] ?? []).filter((entry) => entry !== id);
      return { ...all, [workspace]: rest.at(-1) ?? '' };
    });
  };

  // Forget terminals the frame no longer has (after a restart of the terminal plugin, say).
  useEffect(() => {
    if (!workspace || list.length === 0) return;
    let cancelled = false;
    client.call<{ terminals: TermInfo[] }>('terminal.list', {})
      .then((result) => {
        if (cancelled) return;
        const alive = new Set((result?.terminals ?? []).filter((term) => term.alive).map((term) => term.term_id));
        const kept = list.filter((id) => alive.has(id));
        if (kept.length !== list.length) setTerms((all) => ({ ...all, [workspace]: kept }));
      })
      .catch(() => undefined);
    return () => {
      cancelled = true;
    };
    // Checked when the pane is shown for a workspace.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [workspace, active]);

  // The first visit opens a shell.
  useEffect(() => {
    if (active && workspace && list.length === 0 && !opening && !error) void openTerminal();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [active, workspace, list.length]);

  if (!workspace) return <div className="cx-terminal"><p className="empty-line">Open a project to get a terminal in it.</p></div>;
  return (
    <div className="cx-terminal" data-build={__CORTEX_BUILD__} data-term={termId ?? ''}>
      <div className="bar">
        {list.length > 0 && (
          <div className="seg-set" role="group" aria-label="Terminals">
            {list.map((id, index) => (
              <button key={id} type="button" aria-pressed={id === termId} onClick={() => setCurrent((all) => ({ ...all, [workspace]: id }))}>
                Shell {index + 1}{exited[id] !== undefined ? ' · ended' : ''}
              </button>
            ))}
          </div>
        )}
        <div className="grow" />
        <button className="detent" type="button" onClick={() => void openTerminal()} disabled={opening}>{opening ? 'Opening…' : 'New terminal'}</button>
        {termId && <button className="detent warn" type="button" onClick={() => void closeTerminal(termId)}>Close</button>}
      </div>
      {error && (
        <div className="cx-terminal-error">
          <p className="inline-error" role="alert">{error}</p>
          <button className="detent" type="button" onClick={() => { setError(null); void openTerminal(); }}>Try again</button>
        </div>
      )}
      {termId && (
        <XTerm key={termId} termId={termId} active={Boolean(active)} onLink={setLink}
          onExit={(code) => setExited((all) => ({ ...all, [termId]: code }))} />
      )}
      {termId && exited[termId] !== undefined && (
        <p className="hint cx-terminal-ended">The shell ended{exited[termId] !== null ? ` with code ${exited[termId]}` : ''}.</p>
      )}
      {link && <LinkDialog url={link} onOpen={() => { window.open(link, '_blank', 'noopener'); setLink(null); }} onCancel={() => setLink(null)} />}
    </div>
  );
}

/** A link a program printed (OSC 8) asks in the theme's modal before it opens, not in the browser's confirm. */
function LinkDialog({ url, onOpen, onCancel }: { url: string; onOpen: () => void; onCancel: () => void }) {
  const openButton = useRef<HTMLButtonElement>(null);
  useLayoutEffect(() => openButton.current?.focus(), []);
  // Over the whole window, inside the theme's scope.
  return createPortal(
    <div className="modal-backdrop" role="presentation" onClick={(event) => event.target === event.currentTarget && onCancel()}
      onKeyDown={(event) => event.key === 'Escape' && onCancel()}>
      <div className="modal-card cx-terminal-link" role="alertdialog" aria-modal="true" aria-labelledby="cx-terminal-link-title" aria-describedby="cx-terminal-link-body">
        <h2 id="cx-terminal-link-title" className="cx-terminal-link-title">Open this link?</h2>
        <p id="cx-terminal-link-body" className="cx-terminal-link-body">A program in the terminal printed it; open it only if you trust where it leads.</p>
        <p className="cx-terminal-link-url">{url}</p>
        <div className="modal-actions cx-terminal-link-actions">
          <button type="button" className="detent" onClick={onCancel}>Cancel</button>
          <button type="button" className="detent warn" ref={openButton} onClick={onOpen}>Open link</button>
        </div>
      </div>
    </div>,
    document.getElementById('cc') ?? document.body,
  );
}

function XTerm({ termId, active, onExit, onLink }: { termId: string; active: boolean; onExit: (code: number | null) => void; onLink: (url: string) => void }) {
  const host = useRef<HTMLDivElement>(null);
  const fitRef = useRef<FitAddon | null>(null);
  const termRef = useRef<Terminal | null>(null);
  const exitRef = useRef(onExit);
  exitRef.current = onExit;
  const linkRef = useRef(onLink);
  linkRef.current = onLink;
  const activeRef = useRef(active);
  activeRef.current = active;

  useEffect(() => {
    const element = host.current;
    if (!element) return;
    const client = runtime().client;
    const term = new Terminal({
      fontFamily: activeTheme()?.fonts.mono,
      fontSize: 12.5,
      lineHeight: 1.25,
      cursorBlink: true,
      cursorStyle: 'block',
      allowProposedApi: true,
      scrollback: 10_000,
      theme: palette(),
      // xterm's own handler asks with window.confirm; the pane asks in the theme's modal.
      linkHandler: { activate: (_event, uri) => linkRef.current(uri) },
    });
    const fit = new FitAddon();
    fitRef.current = fit;
    termRef.current = term;
    term.loadAddon(fit);
    term.open(element);
    try {
      const webgl = new WebglAddon();
      webgl.onContextLoss(() => webgl.dispose());
      term.loadAddon(webgl);
      element.dataset.renderer = 'webgl';
    } catch {
      // WebGL unavailable: xterm keeps its DOM renderer.
      element.dataset.renderer = 'dom';
    }
    let lastSize = '';
    const resize = () => {
      if (!element.offsetWidth || !element.offsetHeight) return;
      try {
        fit.fit();
      } catch {
        return;
      }
      const size = `${term.cols}x${term.rows}`;
      if (size === lastSize) return;
      lastSize = size;
      void client.call('terminal.resize', { term_id: termId, cols: term.cols, rows: term.rows }).catch(() => undefined);
    };
    // Repaint what is on the screen now, then follow the live output.
    let live = false;
    const queued: Uint8Array[] = [];
    const offOutput = client.on('terminal.output', (_topic, data) => {
      const event = data as { term_id?: string; data?: string };
      if (event?.term_id !== termId || typeof event.data !== 'string') return;
      const bytes = decodeBase64(event.data);
      if (live) term.write(bytes);
      else queued.push(bytes);
    });
    const offExit = client.on('terminal.exit', (_topic, data) => {
      const event = data as { term_id?: string; exit_code?: number | null };
      if (event?.term_id === termId) exitRef.current(event.exit_code ?? null);
    });
    client.call<TermSnapshot>('terminal.snapshot', { term_id: termId })
      .then((snapshot) => {
        // A screen snapshot is plain text: written as lines, then the cursor goes where it was.
        if (snapshot?.text) {
          term.write(snapshot.text.replace(/\n/g, '\r\n'));
          term.write(`\x1b[${snapshot.cursor_row + 1};${snapshot.cursor_col + 1}H`);
        }
      })
      .catch(() => undefined)
      .finally(() => {
        live = true;
        for (const bytes of queued.splice(0)) term.write(bytes);
      });
    const input = term.onData((data) => {
      void client.call('terminal.write', { term_id: termId, data }).catch(() => undefined);
    });
    const binary = term.onBinary((data) => {
      void client.call('terminal.write', { term_id: termId, data }).catch(() => undefined);
    });
    const observer = new ResizeObserver(() => resize());
    observer.observe(element);
    requestAnimationFrame(() => {
      resize();
      if (activeRef.current) term.focus();
    });
    // A new theme version or another ground: restyle (the attributes say what xterm was given).
    const restyle = () => {
      const colors = palette();
      if (!colors) return;
      term.options.theme = colors;
      element.dataset.cursorColor = colors.cursor;
      element.dataset.background = colors.background;
    };
    restyle();
    const offTheme = subscribeTheme(restyle);
    const themeObserver = new MutationObserver(restyle);
    themeObserver.observe(document.documentElement, { attributes: true, attributeFilter: ['data-theme'] });
    return () => {
      offOutput();
      offExit();
      input.dispose();
      binary.dispose();
      observer.disconnect();
      themeObserver.disconnect();
      offTheme();
      term.dispose();
      fitRef.current = null;
      termRef.current = null;
    };
  }, [termId]);

  useEffect(() => {
    if (active) requestAnimationFrame(() => {
      try {
        fitRef.current?.fit();
      } catch {
        // Not laid out yet.
      }
      // Shown: typing goes to the shell.
      termRef.current?.focus();
    });
  }, [active]);

  return <div className="cx-xterm" ref={host} data-term-id={termId} />;
}

export default definePlugin({ slot: 'pane.terminal@1', component: TerminalPane });
