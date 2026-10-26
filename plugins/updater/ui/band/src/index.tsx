/**
 * workbench.band@1 — updates. In the band: what is downloading or installing, "Restart to update"
 * when a newer frame is staged, and for a minute after an update the plugin and its new version.
 * At the bottom right: one popup per plugin the user changed that has a release, until answered —
 * merge the release into the change, keep the user's version (asked again at the next release),
 * or show the changes (Settings > History).
 */

import { definePlugin, errorMessage, useClient, useNow, useQuery, useShared } from '@cortex/sdk';
import { useState, type ReactNode } from 'react';

interface Pending { plugin: string; name?: string; version: string; installed?: string; files?: string[] }
interface Applied { plugin: string; version: string; ms: number; outcome: string; detail?: string }
interface Status {
  enabled: boolean;
  state: string;
  detail?: string;
  error?: string | null;
  restart?: { running: string; staged: string } | null;
  pending?: Pending[];
  waiting?: string[];
  recent?: Applied[];
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
  } else if (latest && now - latest.ms < SHOWN_MS && latest.outcome === 'failed') {
    band = <span className="cx-status cx-attn" title={latest.detail}>{latest.plugin} {latest.version} not updated</span>;
  }

  const ask = data.pending?.[0];
  return (
    <>
      {band}
      {ask && (
        <div className="sheet" role="dialog" aria-modal="false" aria-labelledby="updater-ask-title"
          style={{ position: 'fixed', right: 16, bottom: 16, zIndex: 89, width: 'min(380px, calc(100% - 32px))' }}>
          <div className="sheet-h">
            <span className="lbl grow" id="updater-ask-title">{ask.name || ask.plugin} {ask.version} is available</span>
          </div>
          <div className="sheet-b">
            <p>
              You changed this plugin{ask.files?.length ? ` (${ask.files.length} file${ask.files.length === 1 ? '' : 's'})` : ''}.
              Merge the update into your change, or keep your version until the next release.
            </p>
            {failure && <p className="cx-attn">{failure}</p>}
          </div>
          <div className="sheet-f">
            <button className="detent" type="button" onClick={() => { setSettings('history'); setOverlay(true); }}>Show changes</button>
            <span className="grow" />
            <button className="detent" type="button" disabled={busy !== null}
              onClick={() => void act('keep', 'update.decide', { plugin: ask.plugin, version: ask.version, choice: 'keep' })}>
              Keep mine
            </button>
            <button className="detent send" type="button" disabled={busy !== null}
              onClick={() => void act('merge', 'update.decide', { plugin: ask.plugin, version: ask.version, choice: 'merge' })}>
              Merge update
            </button>
          </div>
        </div>
      )}
    </>
  );
}

export default definePlugin({ slot: 'workbench.band@1', component: Updates });
