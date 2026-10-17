/**
 * New project from GitHub, a sheet: the dev folder (vcs.github.status; Change… saves it with
 * vcs.dev_folder.set) and the repository, picked from the user's own (vcs.github.repos, filtered by
 * what is typed) or typed as owner/name or an https address. Clone runs vcs.clone into
 * `<dev folder>/<name>`, then frame.project.open on the new folder, which `onOpened` selects.
 */

import { useEffect, useRef, useState } from 'react';
import { createPortal } from 'react-dom';
import { duration, errorMessage, setShared, useClient, useHost, useNow, useQuery, useSlotInfo, type Project } from '@cortex/sdk';

interface GithubStatus { connected: boolean; login?: string | null; dev_folder?: string | null }
interface Repo { full_name: string; private: boolean; description: string; fork: boolean }

/** The folder name a clone of `repo` gets (its last path part, without `.git`). */
function folderName(repo: string): string {
  const last = repo.trim().replace(/\/+$/, '').split(/[/:]/).pop() ?? '';
  return last.replace(/\.git$/, '');
}

export function FromGithub({ onClose, onOpened }: { onClose: () => void; onOpened: (project: Project) => Promise<void> }) {
  const client = useClient();
  const host = useHost();
  const owner = useSlotInfo();
  const status = useQuery<GithubStatus>('vcs.github.status', (c) => c.call('vcs.github.status', {}), { topics: ['plugin.swapped'] });
  const connected = status.data?.connected === true;
  const repos = useQuery<Repo[]>(connected ? `vcs.github.repos:${status.data?.login ?? ''}` : null,
    async (c) => (await c.call<{ repos: Repo[] }>('vcs.github.repos', {}))?.repos ?? []);
  const [repo, setRepo] = useState('');
  const [busy, setBusy] = useState<{ step: string; started: number } | null>(null);
  const [error, setError] = useState<string | null>(null);
  const now = useNow(500, busy !== null);
  const dialog = useRef<HTMLDivElement>(null);
  const closeRef = useRef(onClose);
  closeRef.current = onClose;

  // The dialog pattern: focus moves in, Escape closes, focus returns where it was.
  useEffect(() => {
    const before = document.activeElement instanceof HTMLElement ? document.activeElement : null;
    dialog.current?.querySelector('input')?.focus({ preventScroll: true });
    const onKey = (event: KeyboardEvent) => {
      if (event.key !== 'Escape' || event.defaultPrevented) return;
      event.preventDefault();
      closeRef.current();
    };
    document.addEventListener('keydown', onKey);
    return () => {
      document.removeEventListener('keydown', onKey);
      if (before?.isConnected) before.focus({ preventScroll: true });
    };
  }, []);

  const devFolder = status.data?.dev_folder ?? null;
  const name = folderName(repo);
  const typed = repo.trim().toLowerCase();
  const listed = (repos.data ?? []).filter((r) => !typed || r.full_name.toLowerCase().includes(typed));

  const changeFolder = async () => {
    if (!host.shell) {
      setError('Choosing a folder needs the CortexCode app.');
      return;
    }
    const path = await host.shell.pickFolder('Choose your dev folder');
    if (!path) return;
    try {
      await client.call('vcs.dev_folder.set', { path });
      await status.refresh();
    } catch (cause) {
      setError(errorMessage(cause));
    }
  };
  const clone = async () => {
    if (!devFolder || !name) return;
    setError(null);
    setBusy({ step: 'Cloning', started: Date.now() });
    try {
      const { path } = await client.call<{ path: string }>('vcs.clone', { repo: repo.trim(), parent: devFolder });
      setBusy({ step: 'Opening', started: Date.now() });
      await onOpened(await client.call<Project>('frame.project.open', { root: path }));
      onClose();
    } catch (cause) {
      setError(errorMessage(cause));
    } finally {
      setBusy(null);
    }
  };

  const root = document.getElementById('cc') ?? document.body;
  return createPortal(
    <div className="sheet-scrim open" data-slot={owner.slot} data-plugin-id={owner.pluginId} data-portal="from-github">
      <div ref={dialog} className="sheet cx-np" role="dialog" aria-modal="true" aria-label="New project from GitHub">
        <form onSubmit={(event) => { event.preventDefault(); void clone(); }}>
          <div className="sheet-h">
            <span className="ap">New project from GitHub</span>
            <span className="grow" />
            <button className="detent" type="button" onClick={onClose}>Close</button>
          </div>
          <div className="sheet-b">
            <div className="fld">
              <span className="ap faint">Dev folder</span>
              <div className="cx-np-row" data-dev-folder={devFolder ?? ''}>
                <span className={devFolder ? 'cx-mono cx-np-path' : 'ap faint'}>{devFolder ?? 'Not chosen yet'}</span>
                <span className="grow" />
                <button className="detent" type="button" data-action="dev-folder-change" disabled={busy !== null} onClick={() => void changeFolder()}>
                  {devFolder ? 'Change…' : 'Choose…'}
                </button>
              </div>
            </div>
            <label className="fld">
              <span className="ap faint">Repository</span>
              <input type="text" value={repo} placeholder="owner/name or https://…" spellCheck={false} autoComplete="off" data-field="repo"
                onChange={(event) => setRepo(event.target.value)} />
            </label>
            {connected ? (
              repos.data ? (
                listed.length ? (
                  <ul className="cx-np-repos" role="listbox" aria-label="Your repositories">
                    {listed.map((r) => (
                      <li key={r.full_name}>
                        <button type="button" role="option" aria-selected={r.full_name === repo.trim()} data-repo={r.full_name} onClick={() => setRepo(r.full_name)}>
                          <span className="t">{r.full_name}</span>
                          {r.private && <span className="ap faint">private</span>}
                          {r.fork && <span className="ap faint">fork</span>}
                          {r.description && <span className="d">{r.description}</span>}
                        </button>
                      </li>
                    ))}
                  </ul>
                ) : <p className="hint">None of your repositories match; Clone takes what is typed.</p>
              ) : repos.error ? <p className="inline-error">GitHub did not list your repositories: {repos.error}</p>
                : <p className="hint">Reading your repositories…</p>
            ) : status.data ? (
              <p className="hint" data-github="not-connected">
                GitHub is not connected, so your repositories are not listed. Connect it in{' '}
                <button type="button" className="cx-file-link" onClick={() => { onClose(); setShared('settings.open', 'github'); }}>Settings &gt; GitHub</button>.
                A public repository can still be typed as owner/name.
              </p>
            ) : null}
            {devFolder && name && <p className="hint">Clones into <span className="cx-mono">{devFolder}/{name}</span></p>}
            {error && <p className="inline-error" role="alert">{error}</p>}
          </div>
          <div className="sheet-f">
            {busy && <span className="ap faint">{duration(now - busy.started)}</span>}
            <span className="grow" />
            <button className="detent" type="button" onClick={onClose}>Cancel</button>
            <button className="detent" type="submit" data-action="clone-project" disabled={!devFolder || !name || busy !== null}>
              {busy ? `${busy.step}…` : 'Clone'}
            </button>
          </div>
        </form>
      </div>
    </div>,
    root,
  );
}
