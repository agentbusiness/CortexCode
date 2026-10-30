/**
 * settings.mesh@1 — joining computers. Nothing to set: an invitation (one hour, used once) to copy
 * for another computer, a box to paste one into, and the joined computers with Remove. What runs
 * where (a session's model, an agent, a command) is decided by the host from what each computer
 * reports; the list shows it as it happens. Each member has one switch: whether it may work on
 * this computer (`mesh.allow`); one that may not is offered nothing here.
 */

import { useState } from 'react';
import { definePlugin, errorMessage, useClient, useHost, useNow, useQuery } from '@cortex/sdk';
import './mesh.css';

interface Member { node: string; name: string; added_ms: number; by: string }
interface ModelCard { model_id: string; variant: string; family: string; tags?: string[]; slots: number; slots_busy: number; queued: number; tok_s: number }
interface Card { software: string; chip: string; memory_bytes: number; free_memory_bytes: number; on_mains: boolean; user_active: boolean; model?: ModelCard | null; languages: { id: string; version: string }[]; docker: boolean; agents_running: number }
interface Peer { member: Member; card?: Card | null; reachable: boolean; path: string; rtt_ms: number; serving: string[] }
interface PairCode { code: string; expires_ms: number; invitation: string }
interface Placed { session_id: string; node: string; name: string }
interface Network { id: string; founder: string; founded_ms: number; members: number }
interface Status { node: string; name: string; members: number; reachable: number; network?: Network | null; pairing?: PairCode | null; serving: [string, string][]; placed: Placed[]; allowed: string[] }

/** What a member allowed by `mesh.allow` may do on this computer. */
const ALLOW_LABEL = 'Projects';
const ALLOW_TEXT = 'may see the projects on this computer';

function gb(bytes: number): string {
  const value = bytes / (1024 * 1024 * 1024);
  return `${value < 10 ? value.toFixed(1).replace(/\.0$/, '') : Math.round(value)} GB`;
}

/** A toolchain's version number out of its tool's own line ("Apple clangd version 21.0.0 (…)" -> 21.0.0). */
function shortVersion(version: string): string {
  return /\d+(?:\.\d+)+/.exec(version)?.[0] ?? '';
}

/** The app identifies itself in its card; model tags do not identify the app. */
function softwareName(software?: string): string {
  if (software === 'cortexcode') return 'CortexCode';
  if (software === 'cortexterm') return 'CortexTerm';
  return software || 'unknown';
}

function where(path: string): string {
  if (path === 'lan') return 'on this network';
  if (path === 'relay') return 'remote, through a relay';
  if (path === 'direct') return 'remote, direct';
  return '';
}

function MeshPanel() {
  const client = useClient();
  const host = useHost();
  const status = useQuery<Status>('mesh.status', (c) => c.call('mesh.status', {}), { topics: ['mesh.changed', 'plugin.swapped'] });
  const peers = useQuery<{ peers: Peer[] }>('mesh.peers', (c) => c.call('mesh.peers', {}), { topics: ['mesh.changed'] });
  const [busy, setBusy] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [token, setToken] = useState('');
  const [copied, setCopied] = useState(false);
  const now = useNow(1000, Boolean(status.data?.pairing));

  const run = async (what: string, work: () => Promise<string | void>) => {
    setBusy(what);
    setError(null);
    try {
      const said = await work();
      if (said) host.notify(said);
    } catch (cause) {
      setError(errorMessage(cause));
    } finally {
      setBusy(null);
      await Promise.all([status.refresh(), peers.refresh()]);
    }
  };

  const data = status.data;
  if (status.error && !data) {
    return (
      <section className="cx-mesh" data-build={__CORTEX_BUILD__}>
        <p className="cx-mesh-note">Joining computers is not available: {status.error}</p>
      </section>
    );
  }
  if (!data) return null;
  const pairing = data.pairing && data.pairing.expires_ms > now ? data.pairing : null;
  const minutesLeft = pairing ? Math.max(1, Math.round((pairing.expires_ms - now) / 60000)) : 0;
  const others = (peers.data?.peers ?? []).filter((peer) => peer.member.node !== data.node);

  const invite = () => void run('pair', async () => {
    const code = pairing ?? (await client.call<PairCode>('mesh.pair.start', {}));
    try {
      await navigator.clipboard?.writeText(code.invitation);
      setCopied(true);
      return 'Invitation copied: paste it on the other computer';
    } catch {
      return 'Invitation ready below: copy it to the other computer';
    }
  });

  return (
    <section className="cx-mesh" data-build={__CORTEX_BUILD__}>
      <div className="cx-mesh-grid">
        <span className="ap">This computer</span>
        <span className="t">{data.name}</span>
        <span />

        <span className="ap">Network</span>
        {data.network ? (
          <>
            <span className="v">started by {data.network.founder} on {new Date(data.network.founded_ms).toLocaleDateString()} · {data.reachable} of {others.length} other{others.length === 1 ? '' : 's'} reachable</span>
            <button type="button" className="detent tiny warn" data-action="leave" disabled={busy !== null} onClick={() => void run('leave', async () => { await client.call('mesh.leave', {}); return 'Left the network'; })}>Leave the network</button>
          </>
        ) : (
          <>
            <span className="v faint">none yet: make an invitation, or join with one</span>
            <span />
          </>
        )}

        <span className="ap">Add a computer</span>
        <span className="cx-mesh-cell">
          <button type="button" className="detent tiny send" data-action="pair" disabled={busy !== null} onClick={invite}>{pairing ? (copied ? 'Copy the invitation again' : 'Copy the invitation') : 'Make an invitation'}</button>
          {pairing && <span className="v faint">good for {minutesLeft} min, one computer</span>}
        </span>
        {pairing ? <button type="button" className="detent tiny" disabled={busy !== null} onClick={() => void run('cancel', async () => { await client.call('mesh.pair.cancel', {}); setCopied(false); })}>Withdraw</button> : <span />}
        <span />
        {pairing ? (
          <div className="cx-mesh-inv" data-testid="invitation">{pairing.invitation}</div>
        ) : (
          <span className="cx-mesh-note">Paste it on the other computer: in its Settings → Mesh, or with <code>cx mesh join '…'</code>.</span>
        )}
        <span />

        <span className="ap">Join</span>
        <span className="cx-mesh-cell cx-mesh-join">
          <label className="field cx-mesh-field"><input value={token} placeholder="paste an invitation from another computer" aria-label="Invitation" onChange={(event) => setToken(event.target.value)} /></label>
          <button type="button" className="detent tiny send" data-action="join" disabled={busy !== null || !token.trim()} onClick={() => void run('join', async () => {
            const member = await client.call<Member>('mesh.pair.join', { code: token.trim() });
            setToken('');
            return `Joined as ${member.name}`;
          })}>{busy === 'join' ? 'Joining…' : 'Join'}</button>
        </span>
      </div>

      {others.length > 0 && (
        <ul className="cx-mesh-list">
          {others.map((peer) => {
            const member = peer.member;
            const card = peer.card;
            const placedHere = data.placed.filter((placed) => placed.node === member.node).length;
            const allowed = data.allowed.includes(member.node);
            return (
              <li key={member.node} data-node={member.node} className="cx-mesh-grid">
                <span className="cx-mesh-name">
                  <span className="t">{member.name}</span>
                  <span className={`ap ${peer.reachable ? 'live' : 'faint'}`}>{peer.reachable ? `${where(peer.path)}${peer.rtt_ms ? ` · ${peer.rtt_ms} ms` : ''}` : 'not reachable now'}</span>
                </span>
                <button type="button" className="detent tiny warn" data-action="revoke" disabled={busy !== null} onClick={() => void run('revoke', async () => { await client.call('mesh.revoke', { node: member.node }); return `${member.name} removed`; })}>Remove</button>
                <span className="ap">Software</span>
                <span className="v">{softwareName(card?.software)}</span>
                <span />
                <span className="ap">{ALLOW_LABEL}</span>
                <span className="v">{ALLOW_TEXT}</span>
                <button type="button" className="sw" data-action="allow" aria-pressed={allowed} aria-label={`${member.name} ${ALLOW_TEXT}`} disabled={busy !== null} onClick={() => void run('allow', async () => { await client.call('mesh.allow', { node: member.node, allowed: !allowed }); })} />
                {card && <>
                  <span className="ap">Computer</span>
                  <span className="v">{[card.chip, `${gb(card.memory_bytes)}, ${gb(card.free_memory_bytes)} free`, card.on_mains ? 'on mains' : 'on battery', card.docker ? 'Docker' : '', card.user_active ? 'in use' : ''].filter(Boolean).join(' · ')}</span>
                  <span />
                  <span className="ap">Model</span>
                  <span className="v" title={card.model?.model_id}>{card.model ? [card.model.variant || card.model.model_id, (card.model.tags ?? []).join(', '), card.model.slots_busy > 0 || card.model.queued > 0 ? 'busy' : 'free', card.model.tok_s ? `${card.model.tok_s.toFixed(1)} tok/s` : ''].filter(Boolean).join(' · ') : 'none'}</span>
                  <span />
                  {card.languages.length > 0 && <>
                    <span className="ap">Languages</span>
                    <span className="v">{card.languages.map((language) => [language.id, shortVersion(language.version)].filter(Boolean).join(' ')).join(', ')}</span>
                    <span />
                  </>}
                  {(peer.serving.length > 0 || placedHere > 0) && <>
                    <span className="ap">For here</span>
                    <span className="v">{[peer.serving.length > 0 ? `its ${peer.serving.join(', ')} serves this computer` : '', placedHere > 0 ? `runs ${placedHere} session${placedHere === 1 ? '' : 's'} from here` : ''].filter(Boolean).join(' · ')}</span>
                    <span />
                  </>}
                </>}
              </li>
            );
          })}
        </ul>
      )}

      {error && <p className="inline-error">{error}</p>}
    </section>
  );
}

export default definePlugin({ slot: 'settings.mesh@1', component: MeshPanel });
