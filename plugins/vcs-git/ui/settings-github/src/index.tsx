/**
 * settings.github@1 — the GitHub connection (vcs.github.status): the account (avatar, login, name),
 * how it was connected and the permissions GitHub granted, and Disconnect (vcs.github.disconnect).
 * Not connected: Connect GitHub, GitHub's device sign-in (vcs.github.device.start shows a code, Copy,
 * and the page opened in the user's own browser; vcs.github.device.poll at GitHub's interval until it
 * is approved), or a personal access token (vcs.github.token.set). Then the dev folder new clones go
 * into (vcs.dev_folder.set). The token stays in the macOS Keychain and never reaches the page.
 */

import { useEffect, useState } from 'react';
import { definePlugin, duration, errorMessage, useClient, useHost, useNow, useQuery, ServiceProblem } from '@cortex/sdk';
import './github.css';

interface GithubStatus {
  connected: boolean;
  pending: boolean;
  login?: string | null;
  name?: string | null;
  avatar_url?: string | null;
  method?: string | null;
  scopes: string[];
  device_available: boolean;
  dev_folder?: string | null;
  error?: string | null;
}
interface DeviceCode { user_code: string; verification_uri: string; expires_in: number; interval: number }

function GithubPanel() {
  const client = useClient();
  const host = useHost();
  const status = useQuery<GithubStatus>('vcs.github.status', (c) => c.call('vcs.github.status', {}), { topics: ['plugin.swapped'] });
  const [device, setDevice] = useState<(DeviceCode & { started: number }) | null>(null);
  const [token, setToken] = useState('');
  const [busy, setBusy] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [copied, setCopied] = useState(false);
  const [avatarFailed, setAvatarFailed] = useState(false);
  const now = useNow(1000, device !== null);

  // While a code is shown, ask the plugin at GitHub's interval whether the user approved it.
  useEffect(() => {
    if (!device) return;
    let stopped = false;
    let timer: ReturnType<typeof setTimeout> | undefined;
    const poll = async () => {
      try {
        const next = await client.call<GithubStatus>('vcs.github.device.poll', {});
        if (stopped) return;
        if (next.pending && !next.connected) {
          timer = setTimeout(() => void poll(), device.interval * 1000);
          return;
        }
        if (next.error) setError(next.error);
      } catch (cause) {
        if (stopped) return;
        setError(errorMessage(cause));
      }
      setDevice(null);
      await status.refresh();
    };
    timer = setTimeout(() => void poll(), device.interval * 1000);
    return () => {
      stopped = true;
      clearTimeout(timer);
    };
  }, [device, client]);

  const run = async (name: string, work: () => Promise<unknown>) => {
    setBusy(name);
    setError(null);
    try {
      await work();
      await status.refresh();
    } catch (cause) {
      setError(errorMessage(cause));
    } finally {
      setBusy(null);
    }
  };
  const openPage = async (url: string) => {
    const problem = host.shell?.openExternal ? await host.shell.openExternal(url) : 'this needs the CortexCode app';
    if (problem) setError(`Could not open the browser (${problem}): go to ${url} yourself.`);
  };
  const connect = () => run('connect', async () => {
    const code = await client.call<DeviceCode>('vcs.github.device.start', {});
    setCopied(false);
    setDevice({ ...code, started: Date.now() });
  });
  const copy = async (text: string) => {
    try {
      await navigator.clipboard.writeText(text);
      setCopied(true);
    } catch {
      setError('Copy did not work: select the code and copy it.');
    }
  };
  const saveToken = () => run('token', async () => {
    await client.call('vcs.github.token.set', { token: token.trim() });
    setToken('');
  });
  const changeFolder = async () => {
    if (!host.shell) {
      setError('Choosing a folder needs the CortexCode app.');
      return;
    }
    const path = await host.shell.pickFolder('Choose your dev folder');
    if (path) await run('folder', () => client.call('vcs.dev_folder.set', { path }));
  };

  const s = status.data;
  const left = device ? Math.max(0, device.expires_in * 1000 - (now - device.started)) : 0;
  return (
    <div className="cx-settings-panel-body" data-build={__CORTEX_BUILD__} data-panel="github">
      <h3 className="cx-settings-title">GitHub</h3>
      {status.error && !s && <ServiceProblem service="vcs" state={status} className="inline-error" />}
      {!s && !status.error && <p className="hint">Reading the GitHub connection…</p>}
      {s?.connected && (
        <>
          <div className="cx-gh-account" data-connected="true" data-login={s.login ?? ''}>
            {s.avatar_url && !avatarFailed
              ? <img className="cx-gh-avatar" src={s.avatar_url} alt="" onError={() => setAvatarFailed(true)} />
              : <span className="cx-gh-avatar cx-gh-initial" aria-hidden="true">{(s.login ?? '?').charAt(0).toUpperCase()}</span>}
            <div className="cx-gh-who">
              <div className="t">{s.name || s.login}</div>
              {s.name && <div className="ap faint">{s.login}</div>}
            </div>
            <span className="grow" />
            <button className="detent warn" type="button" data-action="github-disconnect" disabled={busy !== null}
              onClick={() => void run('disconnect', () => client.call('vcs.github.disconnect', {}))}>
              {busy === 'disconnect' ? 'Disconnecting…' : 'Disconnect'}
            </button>
          </div>
          <table className="tbl">
            <tbody>
              <tr><td>Connected with</td><td>{s.method === 'device' ? 'GitHub sign-in' : 'a personal access token'}</td></tr>
              <tr><td>Permissions</td><td>{s.scopes.length ? s.scopes.join(', ') : <span className="ap faint">none listed</span>}</td></tr>
            </tbody>
          </table>
          <p className="cx-settings-line">Backup, Share and New project use this connection. The sign-in is kept in the macOS Keychain.</p>
        </>
      )}
      {s && !s.connected && (
        <>
          {device ? (
            <div className="cx-gh-device" data-device="pending">
              <p className="cx-settings-line">Enter this code on GitHub:</p>
              <div className="cx-gh-code" data-user-code={device.user_code}>{device.user_code}</div>
              <div className="cx-gh-row">
                <button className="detent" type="button" data-action="github-copy-code" onClick={() => void copy(device.user_code)}>{copied ? 'Copied' : 'Copy'}</button>
                <button className="detent" type="button" data-action="github-open-page" onClick={() => void openPage(device.verification_uri)}>Open GitHub</button>
                <span className="grow" />
                <button className="detent" type="button" onClick={() => setDevice(null)}>Cancel</button>
              </div>
              <p className="hint">Waiting for you to approve it on GitHub… the code is good for {duration(left)} more.</p>
            </div>
          ) : (
            <>
              <p className="cx-settings-line">Connect your GitHub account to clone your repositories, back up and share improvements.</p>
              {s.device_available && (
                <div className="cx-gh-row">
                  <button className="detent" type="button" data-action="github-connect" disabled={busy !== null} onClick={() => void connect()}>
                    {busy === 'connect' ? 'Starting…' : 'Connect GitHub'}
                  </button>
                </div>
              )}
            </>
          )}
          <h3 className="cx-settings-title">{s.device_available ? 'Or use a token' : 'Personal access token'}</h3>
          <form className="cx-gh-row cx-gh-token" onSubmit={(event) => { event.preventDefault(); if (token.trim()) void saveToken(); }}>
            <input type="password" value={token} placeholder="ghp_… or github_pat_…" aria-label="Personal access token" autoComplete="off" spellCheck={false}
              onChange={(event) => setToken(event.target.value)} />
            <button className="detent" type="submit" data-action="github-save-token" disabled={!token.trim() || busy !== null}>{busy === 'token' ? 'Checking…' : 'Save'}</button>
          </form>
          <p className="hint">The token needs the “repo” scope. It is kept in the macOS Keychain.</p>
        </>
      )}
      {error && <p className="inline-error" role="alert">{error}</p>}
      {s && (
        <>
          <h3 className="cx-settings-title">Dev folder</h3>
          <div className="cx-gh-row" data-dev-folder={s.dev_folder ?? ''}>
            <span className={s.dev_folder ? 'cx-path' : 'ap faint'}>{s.dev_folder ?? 'Not chosen yet'}</span>
            <span className="grow" />
            <button className="detent" type="button" data-action="dev-folder-change" disabled={busy !== null} onClick={() => void changeFolder()}>
              {busy === 'folder' ? 'Saving…' : s.dev_folder ? 'Change…' : 'Choose…'}
            </button>
          </div>
          <p className="hint">Repositories you clone from GitHub go into this folder, one folder each.</p>
        </>
      )}
    </div>
  );
}

export default definePlugin({ slot: 'settings.github@1', component: GithubPanel });
