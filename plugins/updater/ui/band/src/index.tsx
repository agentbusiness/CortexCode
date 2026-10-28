/**
 * workbench.band@1 — updates. In the band only "Restart to update" when a newer frame is staged; it is
 * the band's first item (order 10), so it takes the empty middle and the card at the right stays put.
 * At the bottom right: the round, every update of a check together, one row per plugin with where
 * it stands; it leaves by itself a few seconds after the last one is done (a failure stays until
 * hidden). Below it, one question at a time: a plugin the user changed that has a newer version,
 * until answered (use the newer version and drop the changes, or keep theirs; Settings > History
 * shows the changes), and a newer version that could not start (the previous one keeps running),
 * until seen.
 */

import { definePlugin, errorMessage, useClient, useQuery, useShared } from '@cortex/sdk';
import { useEffect, useState } from 'react';

interface Pending { plugin: string; name?: string; version: string; from?: string; installed?: string; files?: string[] }
interface Applied { plugin: string; version: string; ms: number; outcome: string; detail?: string }
interface Failed { plugin: string; name?: string; version: string; error?: string; ms: number; seen?: boolean }
interface RoundItem { plugin: string; name?: string; version: string; state: string; percent?: number | null; error?: string; ms: number }

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
  round?: RoundItem[];
}

/** How long the round stays after its last update is done. */
const SHOWN_MS = 8_000;
const GOING = ['queued', 'downloading', 'installing'];

/** Where one row stands, in a word. */
function where(row: RoundItem) {
  switch (row.state) {
    case 'queued': return <span className="ap faint">next</span>;
    case 'downloading': return <span className="ap num">{row.percent ?? 0}%</span>;
    case 'installing': return <span className="ap live">installing</span>;
    case 'at_restart': return <span className="ap">at restart</span>;
    case 'failed': return <span className="ap live">not updated</span>;
    default: return <span className="ap faint">done</span>;
  }
}

/** The round: every update of a check together. */
function Round({ rows, onHide }: { rows: RoundItem[]; onHide: () => void }) {
  const going = rows.filter((row) => GOING.includes(row.state)).length;
  const failed = rows.filter((row) => row.state === 'failed').length;
  const title = going
    ? `Updating · ${rows.length - going} of ${rows.length} done`
    : failed
      ? `${failed} of ${rows.length} not updated`
      : rows.every((row) => row.state === 'at_restart')
        ? 'Ready at the next restart'
        : `${rows.length} updated`;
  return (
    <div className="sheet" role="status" aria-live="polite" style={{ width: '100%' }}>
      <div className="sheet-h">
        <span className="ap grow">{title}</span>
        <button className="detent" type="button" onClick={onHide}>Hide</button>
      </div>
      <ul style={{ listStyle: 'none', margin: 0, padding: '4px 14px 8px', maxHeight: '40vh', overflowY: 'auto' }}>
        {rows.map((row) => (
          <li key={row.plugin} title={row.error || undefined} style={{ padding: '7px 0', borderTop: '1px solid var(--rule)' }}>
            <div style={{ display: 'flex', alignItems: 'baseline', gap: 9 }}>
              <span style={{ flex: 1, minWidth: 0, overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap', fontSize: 14 }}>{row.name || row.plugin}</span>
              <span className="ap faint num">{short(row.version)}</span>
              <span style={{ minWidth: 72, textAlign: 'right' }}>{where(row)}</span>
            </div>
            {row.state === 'downloading' && (
              <div style={{ height: 1, marginTop: 6, background: 'var(--rule-2)' }}>
                <div style={{ height: 1, width: `${row.percent ?? 0}%`, background: 'var(--counter)', transition: 'width .3s linear' }} />
              </div>
            )}
            {row.state === 'failed' && row.error && <div className="cx-attn" style={{ fontSize: 13, marginTop: 4 }}>{row.error}</div>}
          </li>
        ))}
      </ul>
    </div>
  );
}

function Updates() {
  const client = useClient();
  const status = useQuery<Status>('update.status:band', (c) => c.call<Status>('update.status', {}), { topics: ['update.changed'] });
  const [, setSettings] = useShared<string | null>('settings.open', null);
  const [, setOverlay] = useShared<boolean>('overlay.open', false);
  const [busy, setBusy] = useState<string | null>(null);
  const [failure, setFailure] = useState<string | null>(null);
  const [hidden, setHidden] = useState<string | null>(null);
  const [, redraw] = useState(0);
  const data = status.data;
  const rows = data?.round ?? [];
  const signature = rows.map((row) => `${row.plugin}@${row.version}`).join(' ');
  const going = rows.some((row) => GOING.includes(row.state));
  const stays = going || rows.some((row) => row.state === 'failed');
  const last = Math.max(0, ...rows.map((row) => row.ms));
  // Drawn once more when the shown time is over, so the round leaves.
  useEffect(() => {
    if (stays || !last) return;
    const timer = setTimeout(() => redraw((n) => n + 1), Math.max(0, last + SHOWN_MS - Date.now()) + 50);
    return () => clearTimeout(timer);
  }, [stays, last]);
  if (!data) return null;
  const showRound = rows.length > 0 && hidden !== signature && (stays || Date.now() - last < SHOWN_MS);

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

  const band = data.restart ? (
    <button className="detent cx-status cx-attn" type="button" disabled={busy === 'restart'}
      title={`CortexCode ${data.restart.staged} is ready (running ${data.restart.running}). Restarting stops every run and the window, then starts again.`}
      onClick={() => void act('restart', 'update.restart', {})}>
      Restart to update
    </button>
  ) : null;

  const ask = data.pending?.[0];
  const broke = (data.failed ?? []).find((failed) => !failed.seen);
  const files = ask?.files?.length ?? 0;
  const source = ask?.from === 'app' ? 'the build this app came with' : 'the official release';
  return (
    <>
      {band}
      {(showRound || ask || broke) && (
      <div style={{ position: 'fixed', right: 16, bottom: 16, zIndex: 89, width: 'min(400px, calc(100% - 32px))', display: 'flex', flexDirection: 'column', gap: 8 }}>
      {showRound && <Round rows={rows} onHide={() => setHidden(signature)} />}
      {ask ? (
        <div className="sheet" role="dialog" aria-modal="false" aria-labelledby="updater-ask-title" style={{ width: '100%' }}>
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
        <div className="sheet" role="dialog" aria-modal="false" aria-labelledby="updater-failed-title" style={{ width: '100%' }}>
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
      </div>
      )}
    </>
  );
}

export default definePlugin({ slot: 'workbench.band@1', component: Updates });
