/**
 * settings.backup@1 — Backup & Sync: the user's GitHub repository CortexCode's own changes are
 * pushed to (picked from `vcs.github.repos`, or a new private one from `vcs.github.repo.create`),
 * Back up now (`source.backup.push`), when the last backup was and its last error, the "Back up
 * after each change" switch (`source.backup.set {auto}`), and Restore from backup
 * (`source.backup.restore`, after a confirm). Needs GitHub connected; otherwise one line points to
 * Settings > GitHub.
 */

import { useState } from 'react';
import { ServiceProblem, ago, definePlugin, errorMessage, setShared, useClient, useQuery } from '@cortex/sdk';
import './backup.css';

/** `source.backup.*` (`protocol/src/contracts/source.rs`, `Backup`). */
interface Backup {
  repo?: string | null;
  auto: boolean;
  last_push_ms?: number | null;
  last_error?: string | null;
  branch: string;
}

interface RestoreResult {
  commit: string;
  rebuilt?: string[];
  not_built?: string[];
}

interface GithubStatus {
  connected: boolean;
  login?: string | null;
}

interface Repo {
  full_name: string;
  private: boolean;
}

const NEW_REPO = 'cortexcode-backup';

function Repository({ backup, onSaved }: { backup: Backup; onSaved: () => Promise<void> }) {
  const client = useClient();
  const [picking, setPicking] = useState(false);
  const [picked, setPicked] = useState('');
  const [name, setName] = useState(NEW_REPO);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const repos = useQuery<Repo[]>(picking ? 'vcs.github.repos' : null, async (c) => (await c.call<{ repos: Repo[] }>('vcs.github.repos', {}))?.repos ?? []);
  const use = async (choose: () => Promise<string>) => {
    setBusy(true);
    setError(null);
    try {
      await client.call<Backup>('source.backup.set', { repo: await choose() });
      await onSaved();
      setPicking(false);
    } catch (cause) {
      setError(errorMessage(cause));
    } finally {
      setBusy(false);
    }
  };
  return (
    <>
      <div className="row" data-setting="repo">
        <div className="grow">
          <div className="nm">Repository</div>
          <div className="cx-backup-note">{backup.repo ? <span className="cx-path">{backup.repo}</span> : 'None chosen yet.'}</div>
        </div>
        <button className="detent" type="button" data-action="choose-repo" aria-expanded={picking} onClick={() => setPicking(!picking)}>{picking ? 'Cancel' : backup.repo ? 'Change…' : 'Choose…'}</button>
      </div>
      {picking && (
        <div className="cx-backup-pick">
          <div className="cx-backup-pickrow">
            <label className="field cx-backup-field">
              <select aria-label="Your repositories" value={picked} disabled={!repos.data || busy} onChange={(event) => setPicked(event.target.value)}>
                <option value="">{repos.data ? 'Pick one of your repositories' : repos.error ? 'Your repositories could not be listed' : 'Listing your repositories…'}</option>
                {(repos.data ?? []).map((repo) => <option key={repo.full_name} value={repo.full_name}>{repo.full_name}{repo.private ? ' (private)' : ''}</option>)}
              </select>
            </label>
            <button className="detent" type="button" data-action="use-repo" disabled={!picked || busy} onClick={() => void use(async () => picked)}>Use this one</button>
          </div>
          <div className="cx-backup-pickrow">
            <label className="field cx-backup-field"><input value={name} aria-label="New repository name" onChange={(event) => setName(event.target.value)} /></label>
            <button className="detent" type="button" data-action="create-repo" disabled={!/^[\w.-]+$/.test(name.trim()) || busy}
              onClick={() => void use(async () => (await client.call<Repo>('vcs.github.repo.create', { name: name.trim(), private: true, description: 'CortexCode backup' })).full_name)}>
              {busy ? 'Working…' : 'Create a private one'}
            </button>
          </div>
          {repos.error && <p className="inline-error">{repos.error}</p>}
        </div>
      )}
      {error && <p className="inline-error">Not saved: {error}</p>}
    </>
  );
}

function BackupPanel() {
  const client = useClient();
  const github = useQuery<GithubStatus>('vcs.github.status', (c) => c.call<GithubStatus>('vcs.github.status', {}));
  const backup = useQuery<Backup>('source.backup.status', (c) => c.call<Backup>('source.backup.status', {}), { topics: ['source.changed'] });
  const [busy, setBusy] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [restoring, setRestoring] = useState(false);
  const [restored, setRestored] = useState<string | null>(null);

  const act = async (what: string, work: () => Promise<void>) => {
    setBusy(what);
    setError(null);
    try {
      await work();
      await backup.refresh();
    } catch (cause) {
      setError(errorMessage(cause));
    } finally {
      setBusy(null);
    }
  };
  const restore = () => act('restore', async () => {
    const done = await client.call<RestoreResult>('source.backup.restore', {});
    setRestoring(false);
    setRestored([
      'Restored from the backup.',
      done.rebuilt?.length ? `Rebuilt ${done.rebuilt.join(', ')}.` : '',
      done.not_built?.length ? `Not built on this Mac (tools missing): ${done.not_built.join(', ')}.` : '',
    ].filter(Boolean).join(' '));
  });

  const b = backup.data;
  const connected = github.data?.connected === true;
  return (
    <div className="cx-settings-panel-body" data-build={__CORTEX_BUILD__} data-panel="backup">
      <h3 className="cx-settings-title">Backup & Sync</h3>
      <p className="cx-settings-line">CortexCode's changes to itself (learned skills, guides, prompts and code) are kept in a GitHub repository of yours, so another Mac can restore them.</p>
      {github.data && !connected && (
        <p className="cx-settings-line" data-github="disconnected">
          Connect GitHub first: <button type="button" className="cx-file-link" onClick={() => setShared('settings.open', 'github')}>Settings › GitHub</button>
        </p>
      )}
      {github.error && !github.data && <ServiceProblem service="GitHub" state={github} className="inline-error" />}
      {backup.error && !b && <ServiceProblem service="source manager" state={backup} className="inline-error" />}
      {connected && b && (
        <section className="cx-backup">
          <Repository backup={b} onSaved={backup.refresh} />
          <div className="row" data-setting="push">
            <div className="grow">
              <div className="nm">Back up now</div>
              <div className="cx-backup-note" data-last-push={b.last_push_ms ?? ''}>{b.last_push_ms ? `Last backup: ${ago(b.last_push_ms)}` : 'Not backed up yet.'}</div>
            </div>
            <button className="detent" type="button" data-action="push" disabled={!b.repo || busy !== null} onClick={() => void act('push', async () => { await client.call<Backup>('source.backup.push', {}); })}>
              {busy === 'push' ? 'Backing up…' : 'Back up now'}
            </button>
          </div>
          {b.last_error && <p className="inline-error" data-last-error>The last backup failed: {b.last_error}</p>}
          <div className="row" data-setting="auto">
            <div className="grow">
              <div className="nm">Back up after each change</div>
              <div className="cx-backup-note">Push to the repository whenever CortexCode changes itself.</div>
            </div>
            <button type="button" className="sw" aria-pressed={b.auto} aria-label="Back up after each change" disabled={!b.repo || busy !== null}
              onClick={() => void act('auto', async () => { await client.call<Backup>('source.backup.set', { auto: !b.auto }); })} />
          </div>
          <div className="row" data-setting="restore">
            <div className="grow">
              <div className="nm">Restore from backup</div>
              <div className="cx-backup-note">Bring the backed-up changes onto this Mac, for example on a new one.</div>
            </div>
            <button className="detent warn" type="button" data-action="restore" disabled={!b.repo || busy !== null} onClick={() => setRestoring(true)}>Restore…</button>
          </div>
          {restoring && (
            <div className="cx-backup-confirm">
              <span className="t">This replaces the changes on this Mac with the ones in {b.repo}. Restore?</span>
              <button className="detent warn" type="button" data-action="restore-confirm" disabled={busy !== null} onClick={() => void restore()}>{busy === 'restore' ? 'Restoring…' : 'Restore'}</button>
              <button className="detent" type="button" disabled={busy !== null} onClick={() => setRestoring(false)}>Cancel</button>
            </div>
          )}
          {restored && <p className="cx-settings-line">{restored}</p>}
          {error && <p className="inline-error">{error}</p>}
        </section>
      )}
    </div>
  );
}

export default definePlugin({ slot: 'settings.backup@1', component: BackupPanel });
