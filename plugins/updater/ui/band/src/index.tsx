/**
 * workbench.band@1 — updates. In the band: what is downloading or installing, "Restart to update"
 * when a newer frame is staged, and for a minute after an update the plugin and its new version.
 * At the bottom right, one popup at a time: a plugin the user changed that has a newer version,
 * until answered (use the newer version and drop the changes, or keep theirs; Settings > History
 * shows the changes), and a newer version that could not start (the previous one keeps running),
 * until seen.
 */

import { definePlugin, errorMessage, useClient, useNow, useQuery, useShared } from '@cortex/sdk';
import { useState, type ReactNode } from 'react';

interface Pending { plugin: string; name?: string; version: string; from?: string; installed?: string; files?: string[] }
interface Applied { plugin: string; version: string; ms: number; outcome: string; detail?: string }
interface Failed { plugin: string; name?: string; version: string; error?: string; ms: number; seen?: boolean }

/** The version without its build tag. */
const short = (version: string) => version.split('+')[0] || version;
interface Status {
  enabled: boolean;
  state: string;
  detail?: string;
  error?: string | null;
  restart?: { running: string; staged: string } | null;
  pending?: Pending[];
  waiting?: string[];
  recent?: Applied[];
  failed?: Failed[];
}

/** How long a finished update stays in the band. */
const SHOWN_MS = 60_000;

function Updates() {
  const client = useClient();
  const status = useQuery<Status>('update.status:band', (c) => c.call<Status>('update.status', {}), { topics: ['update.changed'] });
  const [, setSettings] = useShared<string | null>('settings.open', null);
  const [, setOverlay] = useShared<boolean>('overlay.open', false);
  const [busy, setBusy] = useState<string | null>(null);
  const [failure, setFailure] = useState<string | null>(null);
  const latest = status.data?.recent?.[0];
  const now = useNow(5_000, !!latest && Date.now() - latest.ms < SHOWN_MS);
  const data = status.data;
  if (!data) return null;

  const act = async (key: string, method: string, params: object) => {
    setBusy(key);
    setFailure(null);
    try {
      await client.call(method, params);
      await status.refresh();
    } catch (error) {
      setFailure(errorMessage(error));
    } finally {
      setBusy(null);
    }
  };

  let band: ReactNode = null;
  if (data.restart) {
    band = (
      <button className="detent cx-status cx-attn" type="button" disabled={busy === 'restart'}
        title={`CortexCode ${data.restart.staged} is ready (running ${data.restart.running}). Restarting stops every run and the window, then starts again.`}
        onClick={() => void act('restart', 'update.restart', {})}>
        Restart to update
      </button>
    );
  } else if (data.state === 'downloading' || data.state === 'installing') {
    band = <span className="cx-status" title={data.detail}>{data.detail}</span>;
  } else if (latest && now - latest.ms < SHOWN_MS && (latest.outcome === 'installed' || latest.outcome === 'built')) {
    band = <span className="cx-status" title={latest.detail || undefined}>{latest.plugin} updated to {latest.version}</span>;
  } else if (latest && now - latest.ms < SHOWN_MS && latest.outcome === 'reset') {
    band = <span className="cx-status" title={latest.detail || undefined}>{latest.plugin} reset to {latest.version}</span>;
  } else if (latest && now - latest.ms < SHOWN_MS && latest.outcome === 'failed') {
    band = <span className="cx-status cx-attn" title={latest.detail}>{latest.plugin} {latest.version} not updated</span>;
  }

  const ask = data.pending?.[0];
  const broke = (data.failed ?? []).find((failed) => !failed.seen);
  const files = ask?.files?.length ?? 0;
  const source = ask?.from === 'app' ? 'the build this app came with' : 'the official release';
  return (
    <>
      {band}
      {ask ? (
        <div className="sheet" role="dialog" aria-modal="false" aria-labelledby="updater-ask-title"
          style={{ position: 'fixed', right: 16, bottom: 16, zIndex: 89, width: 'min(400px, calc(100% - 32px))' }}>
          <div className="sheet-h">
            <span className="lbl grow" id="updater-ask-title">{ask.name || ask.plugin}: a newer version is ready</span>
          </div>
          <div className="sheet-b">
            <p>
              You changed this plugin{files ? ` (${files} file${files === 1 ? '' : 's'})` : ''}, so it was not updated by itself.
              {' '}Version <span className="num">{short(ask.version)}</span> is {source}{ask.installed ? <>; you run <span className="num">{short(ask.installed)}</span></> : null}.
            </p>
            <p>Use the newer version and drop your changes, or keep your version? If you keep it, the next version asks again.</p>
            {failure && <p className="cx-attn">{failure}</p>}
          </div>
          <div className="sheet-f">
            <button className="detent" type="button" onClick={() => { setSettings('history'); setOverlay(true); }}>Show my changes</button>
            <span className="grow" />
            <button className="detent" type="button" disabled={busy !== null} data-action="keep"
              onClick={() => void act('keep', 'update.decide', { plugin: ask.plugin, version: ask.version, choice: 'keep' })}>
              Keep my version
            </button>
            <button className="detent send" type="button" disabled={busy !== null} data-action="use"
              onClick={() => void act('use', 'update.decide', { plugin: ask.plugin, version: ask.version, choice: 'use' })}>
              Use the newer version
            </button>
          </div>
        </div>
      ) : broke ? (
        <div className="sheet" role="dialog" aria-modal="false" aria-labelledby="updater-failed-title"
          style={{ position: 'fixed', right: 16, bottom: 16, zIndex: 89, width: 'min(400px, calc(100% - 32px))' }}>
          <div className="sheet-h">
            <span className="lbl grow" id="updater-failed-title">{broke.name || broke.plugin}: the newer version could not start</span>
          </div>
          <div className="sheet-b">
            <p>Version <span className="num">{short(broke.version)}</span> failed its start-up check, so the previous version keeps running. It is not tried again until a newer version comes.</p>
            {broke.error && <p className="cx-attn">{broke.error}</p>}
            {failure && <p className="cx-attn">{failure}</p>}
          </div>
          <div className="sheet-f">
            <button className="detent" type="button" onClick={() => { setSettings('plugins'); setOverlay(true); }}>Open Plugins</button>
            <span className="grow" />
            <button className="detent send" type="button" disabled={busy !== null} data-action="dismiss"
              onClick={() => void act('dismiss', 'update.dismiss', { plugin: broke.plugin })}>
              OK
            </button>
          </div>
        </div>
      ) : null}
    </>
  );
}

export default definePlugin({ slot: 'workbench.band@1', component: Updates });
