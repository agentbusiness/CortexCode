/**
 * settings.share@1 — Share improvements with CortexCode: the user's changes not shared yet
 * (`source.share.candidates`), each with its summary, the plugins it touched and an unticked box.
 * Under a change, what the privacy check found (a path on this Mac, something from one of the
 * user's projects, something that looks like a secret, with the line); such a change needs a
 * second tick before it can be sent. Show the change opens its diff. Send (`source.share.send`)
 * opens a pull request to the CortexCode project; below, the shared pull requests and their state
 * (`source.share.status`). Needs GitHub connected; otherwise one line points to Settings > GitHub.
 */

import { useState } from 'react';
import {
  DiffView,
  ServiceProblem,
  clock,
  definePlugin,
  errorMessage,
  parseUnifiedDiff,
  setShared,
  useClient,
  useHost,
  useQuery,
  type FileDiff,
  type SourceFileDiff,
} from '@cortex/sdk';
import './share.css';

/** `source.share.*` (`protocol/src/contracts/source.rs`). */
interface Finding {
  path: string;
  line: number;
  text: string;
  /** path | project | secret */
  reason: string;
}

interface Candidate {
  commit: string;
  ms: number;
  summary: string;
  plugins: string[];
  files: string[];
  findings: Finding[];
}

interface Shared {
  url: string;
  number: number;
  commits: string[];
  /** open | merged | closed */
  state: string;
  ms: number;
}

const REASON: Record<string, string> = {
  path: 'A path on this Mac',
  project: 'From one of your projects',
  secret: 'Looks like a secret',
};

interface Units { units?: Array<{ id: string; name?: string }> }

/** Plugin ids to the names the person knows them by (`frame.plugins.list` units); the id otherwise. */
function usePluginNames(): (id: string) => string {
  const names = useQuery<Record<string, string>>('frame.plugins.list:names', async (c) => {
    const result = await c.call<Units>('frame.plugins.list', {});
    return Object.fromEntries((result?.units ?? []).map((unit) => [unit.id, unit.name || unit.id]));
  }, { topics: ['plugin.unit'] });
  return (id) => names.data?.[id] || id;
}

/** `source.diff` files as the diff view draws them (one unified diff per file). */
function diffFiles(files: readonly SourceFileDiff[]): FileDiff[] {
  return files.map((file) => {
    const parsed = parseUnifiedDiff(file.patch)[0];
    return { path: file.path, old_path: parsed?.old_path ?? null, status: file.status, binary: parsed?.binary ?? false, hunks: parsed?.hunks ?? [] };
  });
}

function CandidateRow({ candidate, name, picked, checked, onPick, onCheck }: {
  candidate: Candidate;
  name: (id: string) => string;
  picked: boolean;
  checked: boolean;
  onPick: (on: boolean) => void;
  onCheck: (on: boolean) => void;
}) {
  const [showing, setShowing] = useState(false);
  const diff = useQuery<FileDiff[]>(showing ? `source.diff:commit:${candidate.commit}` : null, async (c) => diffFiles((await c.call<{ files: SourceFileDiff[] }>('source.diff', { commit: candidate.commit }))?.files ?? []));
  const findings = candidate.findings;
  return (
    <li data-commit={candidate.commit} data-findings={findings.length}>
      <div className="cx-share-change">
        <label className="cx-share-tick">
          <input type="checkbox" checked={picked} onChange={(event) => onPick(event.target.checked)} />
          <span className="t">{candidate.summary || 'No summary.'}</span>
        </label>
        <div className="r">
          <span className="ap faint">{clock(candidate.ms)}</span>
          {candidate.plugins.length > 0 && <span className="ap faint">{candidate.plugins.map(name).join(', ')}</span>}
          {findings.length > 0 && <span className="ap hot">check {findings.length === 1 ? '1 line' : `${findings.length} lines`}</span>}
          <span className="grow" />
          <button className="detent" type="button" data-action="show-change" aria-expanded={showing} onClick={() => setShowing(!showing)}>{showing ? 'Hide the change' : 'Show the change'}</button>
        </div>
        {findings.length > 0 && (
          <div className="cx-share-findings">
            <ul>
              {findings.map((finding, index) => (
                <li key={`${finding.path}:${finding.line}:${index}`} data-reason={finding.reason}>
                  <span className="ap live">{REASON[finding.reason] ?? finding.reason}</span>
                  <span className="cx-path">{finding.path}:{finding.line}</span>
                  <code className="cx-share-text">{finding.text}</code>
                </li>
              ))}
            </ul>
            <label className="cx-share-tick">
              <input type="checkbox" checked={checked} disabled={!picked} onChange={(event) => onCheck(event.target.checked)} />
              <span className="t">I checked these lines: share this change anyway</span>
            </label>
          </div>
        )}
        {showing && (
          <div className="cx-share-diff">
            {diff.data ? (diff.data.length ? <DiffView files={diff.data} /> : <p className="hint">No file changes.</p>)
              : diff.error ? <p className="inline-error">The change could not be read: {diff.error}</p>
              : <p className="hint">Reading the change…</p>}
          </div>
        )}
      </div>
    </li>
  );
}

function SharePanel() {
  const client = useClient();
  const host = useHost();
  const name = usePluginNames();
  const github = useQuery<{ connected: boolean }>('vcs.github.status', (c) => c.call<{ connected: boolean }>('vcs.github.status', {}));
  const candidates = useQuery<Candidate[]>('source.share.candidates', async (c) => (await c.call<{ changes: Candidate[] }>('source.share.candidates', {}))?.changes ?? [], { topics: ['source.changed'] });
  const shared = useQuery<Shared[]>('source.share.status', async (c) => (await c.call<{ shared: Shared[] }>('source.share.status', {}))?.shared ?? [], { topics: ['source.changed'] });
  const [picked, setPicked] = useState<Set<string>>(new Set());
  const [checked, setChecked] = useState<Set<string>>(new Set());
  const [busy, setBusy] = useState(false);
  const [sent, setSent] = useState<Shared | null>(null);
  const [error, setError] = useState<string | null>(null);

  const toggle = (set: Set<string>, commit: string, on: boolean) => {
    const next = new Set(set);
    if (on) next.add(commit);
    else next.delete(commit);
    return next;
  };
  const list = candidates.data ?? [];
  const sendable = list.filter((candidate) => picked.has(candidate.commit) && (candidate.findings.length === 0 || checked.has(candidate.commit)));
  const send = async () => {
    setBusy(true);
    setError(null);
    try {
      setSent(await client.call<Shared>('source.share.send', { commits: sendable.map((candidate) => candidate.commit) }));
      setPicked(new Set());
      setChecked(new Set());
      await Promise.all([candidates.refresh(), shared.refresh()]);
    } catch (cause) {
      setError(errorMessage(cause));
    } finally {
      setBusy(false);
    }
  };
  const openLink = host.shell?.openExternal ? (url: string) => void host.shell?.openExternal?.(url) : null;
  const link = (pr: Shared) => (openLink
    ? <button type="button" className="cx-file-link" onClick={() => openLink(pr.url)} title={pr.url}>Pull request #{pr.number}</button>
    : <span className="cx-path" title={pr.url}>{pr.url}</span>);

  const connected = github.data?.connected === true;
  return (
    <div className="cx-settings-panel-body" data-build={__CORTEX_BUILD__} data-panel="share">
      <h3 className="cx-settings-title">Share improvements with CortexCode</h3>
      <p className="cx-settings-line">Send the changes you pick to the CortexCode project as a pull request. Nothing leaves this Mac until you press Send, and every change starts unticked.</p>
      {github.data && !connected && (
        <p className="cx-settings-line" data-github="disconnected">
          Connect GitHub first: <button type="button" className="cx-file-link" onClick={() => setShared('settings.open', 'github')}>Settings › GitHub</button>
        </p>
      )}
      {github.error && !github.data && <ServiceProblem service="GitHub" state={github} className="inline-error" />}
      {candidates.error && !candidates.data && <ServiceProblem service="source manager" state={candidates} className="inline-error" />}
      {candidates.data && list.length === 0 && <p className="hint">No changes to share yet.</p>}
      {list.length > 0 && (
        <>
          <ul className="cx-share">
            {list.map((candidate) => (
              <CandidateRow key={candidate.commit} candidate={candidate} name={name}
                picked={picked.has(candidate.commit)} checked={checked.has(candidate.commit)}
                onPick={(on) => { setPicked(toggle(picked, candidate.commit, on)); if (!on) setChecked(toggle(checked, candidate.commit, false)); }}
                onCheck={(on) => setChecked(toggle(checked, candidate.commit, on))} />
            ))}
          </ul>
          <div className="cx-share-send">
            <button className="detent send" type="button" data-action="send" disabled={!connected || sendable.length === 0 || busy} onClick={() => void send()}>
              {busy ? 'Sending…' : sendable.length > 1 ? `Send ${sendable.length} changes` : 'Send'}
            </button>
            {picked.size > sendable.length && <span className="hint">Tick the second box under a change with lines to check.</span>}
          </div>
        </>
      )}
      {sent && <p className="cx-settings-line" data-sent={sent.number}>Sent: {link(sent)} is open.</p>}
      {error && <p className="inline-error">Not sent: {error}</p>}
      {(shared.data ?? []).length > 0 && (
        <>
          <h3 className="cx-settings-title">Shared</h3>
          <table className="tbl cx-share-shared">
            <tbody>
              {(shared.data ?? []).map((pr) => (
                <tr key={pr.url} data-pr={pr.number} data-state={pr.state}>
                  <td>{link(pr)}</td>
                  <td><span className={`ap ${pr.state === 'merged' ? 'origin' : pr.state === 'closed' ? 'faint' : 'live'}`}>{pr.state}</span></td>
                  <td><span className="ap faint">{pr.commits.length === 1 ? '1 change' : `${pr.commits.length} changes`} · {clock(pr.ms)}</span></td>
                </tr>
              ))}
            </tbody>
          </table>
        </>
      )}
    </div>
  );
}

export default definePlugin({ slot: 'settings.share@1', component: SharePanel });
