/**
 * pane.security@1 — the project's security findings as the memory plugin keeps them
 * (`memory.security.*`), sorted by the security sentinel's filter (`skills-security.*`): the alerts
 * in colour, what is being investigated or not judged yet, and the findings the filter set aside in
 * grey under one fold; one finding's trace, fix, evidence, the filter's reason and history, and the
 * moves on it (a fix session, investigate, mute, accept the risk, not an issue, a re-check by the
 * host, reopen, forget). The All projects view shows every project of this computer, the problems
 * several share, the live sources' freshness and what the sweep found on the computer itself. An
 * audit, a fix or an investigation is a conversation: the pane creates the session and sends its
 * first message, as the composer does.
 */

import { useState } from 'react';
import {
  ServiceProblem,
  ago,
  clock,
  definePlugin,
  errorMessage,
  openInCode,
  setShared,
  useClient,
  useNow,
  useQuery,
  useSelection,
  useShared,
  flowMethod,
  type Session,
} from '@cortex/sdk';
import type {
  AffectedResult,
  Fleet,
  InvestigateResult,
  MuteKind,
  SecurityChanged,
  SecurityFinding,
  SecurityFindingDetail,
  SecurityRecheck,
  SecurityStatus,
  SecuritySummary,
  SecurityTraceStep,
  SentinelStatus,
} from './types';
import './security.css';

/** The chip a finding wears: its overall severity, `lead` for a lead, `note` for a hardening note. */
const SEVERITIES = ['critical', 'high', 'medium', 'low', 'lead', 'hardening'] as const;
type Severity = (typeof SEVERITIES)[number] | 'unrated';
type Mode = 'ask' | 'auto';

const ACTIVE: SecurityStatus[] = ['open', 'fixing', 'accepted'];
const CLOSED: SecurityStatus[] = ['fixed', 'verified', 'gone'];
const AUDIT_TEXT = 'security audit of this codebase';
const RESOLVE = 'python3 "$CORTEX_HOME/live/plugins/skills-security/data/scripts/secaudit.py" resolve';

export function severityOf(finding: Pick<SecurityFinding, 'verdict' | 'severity_overall'>): Severity {
  if (finding.verdict === 'needs_validation') return 'lead';
  if (finding.verdict === 'hardening') return 'hardening';
  const overall = finding.severity_overall;
  return overall && (SEVERITIES as readonly string[]).includes(overall) ? (overall as Severity) : 'unrated';
}

export function severityLabel(severity: Severity): string {
  return severity === 'hardening' ? 'note' : severity;
}

/** What says a finding is likely to be attacked: CISA KEV, the EPSS score, the dependency watch. */
export function exploitChips(finding: Pick<SecurityFinding, 'kev' | 'epss' | 'source'>): string[] {
  const chips: string[] = [];
  if (finding.kev) chips.push('exploited');
  if (typeof finding.epss === 'number' && finding.epss >= 0.01) chips.push(`EPSS ${Math.round(finding.epss * 100)}%`);
  if (finding.source === 'watch' || finding.source === 'sweep') chips.push('sweep');
  return chips;
}

/** The sink's place for a row: `path:line`, or the path alone. */
/** What the filter made of a finding: `alert`, `investigating`, `judging` (not judged yet), `quiet` or `muted`. */
export type TriageShown = 'alert' | 'investigating' | 'judging' | 'quiet' | 'muted';

export function triageOf(finding: Pick<SecurityFinding, 'triage'>): TriageShown {
  return (finding.triage as TriageShown | null | undefined) ?? 'judging';
}

/** Kept grey under the fold: what the filter set aside. */
export function filteredOut(finding: Pick<SecurityFinding, 'triage'>): boolean {
  return finding.triage === 'quiet' || finding.triage === 'muted';
}

const TRIAGE_LABEL: Record<TriageShown, string> = {
  alert: 'alert',
  investigating: 'investigating',
  judging: 'judging',
  quiet: 'filtered',
  muted: 'muted',
};

const REACH_LABEL: Record<string, string> = {
  called: 'the code calls the vulnerable function',
  imported: 'the code imports it',
  not_imported: 'the code does not import it',
  transitive: 'pulled in by another package',
  dev: 'development only',
  unknown: 'not known',
};

const MUTE_KINDS: { kind: MuteKind; label: string }[] = [
  { kind: 'finding', label: 'This finding' },
  { kind: 'similar', label: 'The same in every project' },
  { kind: 'package', label: 'This package everywhere' },
  { kind: 'advisory', label: 'This advisory everywhere' },
  { kind: 'path', label: 'This class in this folder' },
];

/** `5 min ago` for a time in the past, `in 5 min` for one ahead. */
function when(ms: number | null | undefined, now: number): string {
  if (!ms) return 'never';
  if (ms <= now) return ago(ms, now);
  const minutes = Math.max(1, Math.round((ms - now) / 60_000));
  return minutes < 90 ? `in ${minutes} min` : `in ${Math.round(minutes / 60)} h`;
}

export function placeOf(path: string | null | undefined, line: number | null | undefined): string | null {
  if (!path) return null;
  return line ? `${path}:${line}` : path;
}

/** A path as the code view wants it: relative to the project's root. */
export function relativeTo(root: string, file: string): string {
  return root && file.startsWith(`${root}/`) ? file.slice(root.length + 1) : file;
}

/** The first message of a fix session, from the finding: a sentence whose field is empty is left out. */
export function fixMessage(finding: SecurityFinding): string {
  const grade = finding.severity_overall || finding.verdict;
  const sentences: string[] = [`Fix security finding ${finding.id} (${grade}, ${finding.class}): ${finding.title}.`];
  if (finding.principal && finding.result) {
    const place = placeOf(finding.sink_path, finding.sink_line);
    const because = finding.control ? ` because ${finding.control}` : '';
    sentences.push(`${finding.principal} can ${finding.result}${because}${place ? ` at ${place}` : ''}.`);
  }
  const quick = finding.quick_fix;
  if (quick && quick.commands.length > 0) {
    sentences.push(
      `Fix: run, in order: ${quick.commands.join('; ')}, so that ${quick.lockfile} pins ${quick.package} at ${quick.to} or later (now ${quick.from}); then run the project's tests.`,
    );
  } else if (finding.fix_change) {
    const where = finding.fix_where ? ` at ${finding.fix_where}` : '';
    const test = finding.fix_test ? `; prove it with ${finding.fix_test}` : '';
    sentences.push(`Fix: ${finding.fix_change}${where}${test}.`);
  }
  sentences.push(`When the fix is in and tested, record it with: ${RESOLVE} ${finding.id} "<how you verified>"`);
  return sentences.join(' ');
}

/** Whether a `memory.security.changed` event is about this project (and, when asked, this finding). */
function about(projectId: string | null, id?: string) {
  return (_topic: string, data: unknown) => {
    const change = (data ?? {}) as Partial<SecurityChanged>;
    if (change.project_id && change.project_id !== projectId) return false;
    return !id || !change.id || change.id === id;
  };
}

function Security() {
  const { projectId, project, select } = useSelection();
  const client = useClient();
  const now = useNow(30_000, true);
  const [mode] = useShared<Mode>('agents.defaultMode', 'auto');
  const [scope, setScope] = useShared<'project' | 'fleet'>('security.scope', 'project');
  const [selected, setSelected] = useShared<string | null>('security.selected', null);
  const [severity, setSeverity] = useShared<Severity | null>('security.severity', null);
  const [showFixed, setShowFixed] = useShared<boolean>('security.showFixed', false);
  const [showQuiet, setShowQuiet] = useShared<boolean>('security.showQuiet', false);
  const [auditing, setAuditing] = useState(false);
  const [auditError, setAuditError] = useState<string | null>(null);
  const status = useQuery<SentinelStatus>(
    'skills-security.status',
    (c) => c.call<SentinelStatus>('skills-security.status', {}),
    { topics: ['skills-security.progress', 'skills-security.changed'] },
  );
  const summary = useQuery<SecuritySummary>(
    projectId ? `memory.security.summary:${projectId}` : null,
    (c) => c.call<SecuritySummary>('memory.security.summary', { project_id: projectId }),
    { topics: ['memory.security.changed'], when: about(projectId) },
  );
  const statuses = showFixed ? [...ACTIVE, ...CLOSED] : ACTIVE;
  const findings = useQuery<SecurityFinding[]>(
    projectId ? `memory.security.findings:${projectId}:${showFixed ? 'all' : 'active'}` : null,
    async (c) => (await c.call<{ findings: SecurityFinding[] }>('memory.security.findings', { project_id: projectId, status: statuses }))?.findings ?? [],
    { topics: ['memory.security.changed'], when: about(projectId) },
  );

  const scopeBar = (
    <div className="cx-security-scope" role="group" aria-label="Scope">
      <button type="button" className="chip" aria-pressed={scope === 'project' && !!projectId} disabled={!projectId} onClick={() => setScope('project')}>This project</button>
      <button type="button" className="chip" aria-pressed={scope === 'fleet' || !projectId} onClick={() => setScope('fleet')}>All projects</button>
    </div>
  );
  const sentinelBar = <SentinelLine status={status.data} now={now} />;

  if (scope === 'fleet' || !projectId || !project) {
    return (
      <div className="cx-security" data-build={__CORTEX_BUILD__}>
        <div className="bar cx-security-head">{scopeBar}<div className="grow" />{sentinelBar}</div>
        <FleetView now={now} onOpen={(id) => { select(id, null); setScope('project'); }} />
      </div>
    );
  }

  /** A conversation that starts with one message, as the composer sends it. */
  const startSession = async (title: string, text: string): Promise<string> => {
    const created = await client.call<Session>('frame.session.create', { project_id: projectId, kind: 'chat', title });
    setShared('chat.stick', Date.now());
    await client.call(flowMethod(created.flow, 'turn'), { session_id: created.id, text, mode });
    return created.id;
  };

  const audit = async () => {
    setAuditing(true);
    setAuditError(null);
    try {
      const sessionId = await startSession('Security audit', AUDIT_TEXT);
      select(projectId, sessionId);
    } catch (cause) {
      setAuditError(errorMessage(cause));
    } finally {
      setAuditing(false);
    }
  };

  const changed = () => {
    void summary.refresh();
    void findings.refresh();
  };

  const auditButton = (
    <button className="detent" type="button" disabled={auditing} onClick={() => void audit()}>{auditing ? 'Starting…' : 'Audit now'}</button>
  );
  const data = summary.data;
  const lastAudit = data?.last_audit ?? null;
  const bySeverity = (findings.data ?? []).filter((finding) => !severity || severityOf(finding) === severity);
  const shown = bySeverity.filter((finding) => !filteredOut(finding) || !ACTIVE.includes(finding.status as SecurityStatus));
  const quiet = bySeverity.filter((finding) => filteredOut(finding) && ACTIVE.includes(finding.status as SecurityStatus));
  const noAuditYet = data !== undefined && !lastAudit && findings.data !== undefined && findings.data.length === 0;

  const row = (finding: SecurityFinding) => {
    const sev = severityOf(finding);
    const open = selected === finding.id;
    const place = placeOf(finding.sink_path, finding.sink_line);
    const triage = triageOf(finding);
    return (
      <li key={finding.id} className={open ? 'sel' : undefined} data-finding={finding.id} data-status={finding.status} data-severity={sev} data-triage={triage}>
        <button type="button" aria-expanded={open} onClick={() => setSelected(open ? null : finding.id)}>
          <div className="t">{finding.title}</div>
          <div className="r">
            <span className="ap cx-security-sev" data-sev={filteredOut(finding) ? 'hardening' : sev}>{severityLabel(sev)}</span>
            <span className="ap cx-security-triage" data-triage={triage}>{TRIAGE_LABEL[triage]}</span>
            {place && <span className="cx-security-place">{place}</span>}
            <span className="ap faint">{finding.class}</span>
            <span className={`ap ${finding.status === 'fixing' ? 'live' : 'faint'}`}>{finding.status}</span>
            {finding.quick_fix && <span className="ap origin">quick fix</span>}
            {exploitChips(finding).map((chip) => (
              <span key={chip} className={`ap ${chip === 'exploited' ? 'cx-security-sev' : 'faint'}`} data-sev={chip === 'exploited' ? 'critical' : undefined}>{chip}</span>
            ))}
            <span className="grow" />
            <span className="ap faint">{ago(finding.last_seen_ms, now)}</span>
          </div>
          {finding.triage_reason && <div className="cx-security-reason">{finding.triage_reason}</div>}
        </button>
        {open && (
          <FindingDetail projectId={projectId} root={project.root} row={finding} now={now}
            startSession={startSession} onSession={(sessionId) => select(projectId, sessionId)}
            onChanged={changed} onForgotten={() => { setSelected(null); changed(); }} />
        )}
      </li>
    );
  };

  return (
    <div className="cx-security" data-build={__CORTEX_BUILD__}>
      <div className="bar cx-security-head">
        {scopeBar}
        {lastAudit ? (
          <span className="ap faint" title={lastAudit.scope ? `scope: ${lastAudit.scope}` : undefined}>
            Audit {ago(lastAudit.finished_ms ?? lastAudit.started_ms, now)}
            {lastAudit.finished_ms ? '' : ' · running'} · {lastAudit.units} unit{lastAudit.units === 1 ? '' : 's'} · {lastAudit.lines_judged} line{lastAudit.lines_judged === 1 ? '' : 's'} judged
          </span>
        ) : (
          <span className="ap faint">{data ? 'No audit yet' : 'Reading…'}</span>
        )}
        {(data?.kev ?? 0) > 0 && (
          <span className="ap cx-security-sev" data-sev="critical" title="Open findings whose dependency CISA lists as exploited in the wild">
            {data?.kev} exploited
          </span>
        )}
        <div className="grow" />
        {auditButton}
      </div>
      <div className="bar cx-security-filters">
        <div className="cx-security-chips" role="group" aria-label="Severity">
          {SEVERITIES.map((entry) => {
            const count = data?.by_severity[entry] ?? 0;
            return (
              <button key={entry} type="button" className="chip cx-security-sev" data-sev={entry} aria-pressed={severity === entry}
                onClick={() => setSeverity(severity === entry ? null : entry)}>
                {severityLabel(entry)} <span className="num">{count}</span>
              </button>
            );
          })}
        </div>
        <div className="grow" />
        {sentinelBar}
        <button className="detent" type="button" aria-pressed={showFixed} onClick={() => setShowFixed(!showFixed)}>Show fixed</button>
      </div>
      {auditError && <p className="inline-error" role="alert">{auditError}</p>}
      <div className="cx-security-scroll">
        {summary.error && !summary.data && <ServiceProblem service="memory" state={summary} className="inline-error" />}
        {findings.error && !findings.data && !summary.error && <ServiceProblem service="memory" state={findings} className="inline-error" />}
        {noAuditYet && (
          <div className="cx-security-empty">
            <p className="empty-line">No findings yet. The sentinel sweeps this project's dependencies and supply chain on its own; an audit maps the stack, hunts each attack class and records what it finds here.</p>
            <div className="cx-security-empty-acts">{auditButton}</div>
          </div>
        )}
        {!noAuditYet && findings.data && findings.data.length === 0 && (
          <p className="empty-line">{showFixed ? 'No findings.' : 'No active findings.'}</p>
        )}
        {!noAuditYet && findings.data && findings.data.length > 0 && shown.length === 0 && quiet.length > 0 && (
          <p className="empty-line">Nothing needs you: the filter set aside {quiet.length} finding{quiet.length === 1 ? '' : 's'}.</p>
        )}
        {!noAuditYet && findings.data && findings.data.length > 0 && shown.length === 0 && quiet.length === 0 && (
          <p className="empty-line">No {severity ? severityLabel(severity) : ''} findings in this list.</p>
        )}
        {shown.length > 0 && <ul className="lst cx-security-list">{shown.map(row)}</ul>}
        {quiet.length > 0 && (
          <>
            <button type="button" className="cx-security-fold" aria-expanded={showQuiet} onClick={() => setShowQuiet(!showQuiet)}>
              {showQuiet ? '▾' : '▸'} Set aside by the filter ({quiet.length})
            </button>
            {showQuiet && <ul className="lst cx-security-list cx-security-quiet">{quiet.map(row)}</ul>}
          </>
        )}
      </div>
    </div>
  );
}

/** The sentinel in one line: the running sweep, else when it last looked and what it waits for. */
function SentinelLine({ status, now }: { status: SentinelStatus | undefined; now: number }) {
  const client = useClient();
  const [busy, setBusy] = useState(false);
  if (!status) return null;
  const running = status.running;
  const osv = status.feeds.find((feed) => feed.name === 'osv');
  const stale = status.feeds.filter((feed) => !feed.ok);
  const sweep = async () => {
    setBusy(true);
    try {
      await client.call('skills-security.sweep', { full: true });
    } finally {
      setBusy(false);
    }
  };
  return (
    <span className="cx-security-sentinel">
      {running ? (
        <span className="ap live" title={running.note}>Sweeping{running.total ? ` ${running.done}/${running.total}` : ''}</span>
      ) : (
        <span className="ap faint" title={status.last ? `${status.last.projects} projects, ${status.last.packages} packages; next check ${when(status.next_delta_ms, now)}` : undefined}>
          Watched · advisories {when(osv?.checked_ms, now)}
        </span>
      )}
      {stale.length > 0 && <span className="ap cx-security-sev" data-sev="medium" title={stale.map((feed) => `${feed.name}: ${feed.note}`).join('\n')}>source down</span>}
      {status.judging > 0 && <span className="ap live">judging {status.judging}</span>}
      {status.investigations > 0 && <span className="ap live">{status.investigations} investigating</span>}
      {!running && <button className="detent" type="button" disabled={busy} onClick={() => void sweep()}>{busy ? 'Starting…' : 'Sweep now'}</button>}
    </span>
  );
}

/** Every project of this computer, the problems several share, and "am I hit?". */
function FleetView({ now, onOpen }: { now: number; onOpen: (projectId: string) => void }) {
  const client = useClient();
  const fleet = useQuery<Fleet>(
    'skills-security.fleet',
    (c) => c.call<Fleet>('skills-security.fleet', {}),
    { topics: ['skills-security.changed', 'memory.security.changed'] },
  );
  const [query, setQuery] = useState('');
  const [affected, setAffected] = useState<AffectedResult | null>(null);
  const [asking, setAsking] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const ask = async () => {
    const text = query.trim();
    if (!text) return;
    setAsking(true);
    setError(null);
    try {
      setAffected(await client.call<AffectedResult>('skills-security.affected', { query: text }));
    } catch (cause) {
      setError(errorMessage(cause));
    } finally {
      setAsking(false);
    }
  };
  const data = fleet.data;
  const names = new Map((data?.projects ?? []).map((project) => [project.project_id, project.name]));
  return (
    <div className="cx-security-scroll cx-security-fleet">
      {fleet.error && !fleet.data && <ServiceProblem service="skills-security" state={fleet} className="inline-error" />}
      {(data?.status.machine.length ?? 0) > 0 && (
        <div className="cx-security-machine" role="alert">
          <span className="ap cx-security-sev" data-sev="critical">On this computer</span>
          {data?.status.machine.map((line) => <p key={line} className="cx-security-text">{line}</p>)}
          <p className="cx-security-text">Remove these, then rotate the GitHub, npm and cloud credentials this computer holds.</p>
        </div>
      )}
      <div className="cx-security-ask">
        <input className="write one" value={query} placeholder="Am I hit? A package, name@version, an advisory id or a file name" aria-label="Am I hit"
          onChange={(event) => setQuery(event.target.value)}
          onKeyDown={(event) => { if (event.key === 'Enter') void ask(); }} />
        <button className="detent" type="button" disabled={asking || !query.trim()} onClick={() => void ask()}>{asking ? 'Checking…' : 'Check'}</button>
      </div>
      {error && <p className="inline-error" role="alert">{error}</p>}
      {affected && (
        <div className="cx-security-affected">
          <span className="ap faint">{affected.hits.length === 0 ? `Not found in ${affected.checked} projects` : `${affected.query}: ${affected.hits.length} place${affected.hits.length === 1 ? '' : 's'}`}</span>
          <ul className="cx-security-history">
            {affected.hits.map((hit, index) => (
              <li key={`${hit.project_id}:${hit.path}:${index}`}>
                <button type="button" className="cx-file-link" onClick={() => onOpen(hit.project_id)}>{hit.name}</button>
                <span className="cx-security-place">{hit.path}</span>
                <span className="cx-security-what">{hit.what}</span>
              </li>
            ))}
          </ul>
          {affected.note && <p className="cx-security-text">{affected.note}</p>}
        </div>
      )}
      {data && data.groups.length > 0 && (
        <>
          <span className="ap faint cx-security-section">Shared by several projects</span>
          <ul className="lst cx-security-list">
            {data.groups.map((group) => (
              <li key={group.key} data-triage={group.triage}>
                <div className="cx-security-group">
                  <div className="t">{group.title}</div>
                  <div className="r">
                    <span className="ap cx-security-sev" data-sev={group.triage === 'alert' ? group.severity : 'hardening'}>{group.severity || 'note'}</span>
                    <span className="ap faint">{group.project_ids.length} projects</span>
                    {group.fix && <span className="ap origin">upgrade to {group.fix}</span>}
                  </div>
                  <div className="cx-security-reason">{group.project_ids.map((id) => names.get(id) ?? id).join(', ')}</div>
                </div>
              </li>
            ))}
          </ul>
        </>
      )}
      {data && (
        <>
          <span className="ap faint cx-security-section">{data.projects.length} project{data.projects.length === 1 ? '' : 's'}</span>
          <ul className="lst cx-security-list">
            {data.projects.map((project) => (
              <li key={project.project_id} data-triage={project.alerts > 0 ? 'alert' : 'quiet'}>
                <button type="button" onClick={() => onOpen(project.project_id)}>
                  <div className="t">{project.name}</div>
                  <div className="r">
                    {project.urgent > 0 && <span className="ap cx-security-sev" data-sev="critical">{project.urgent} urgent</span>}
                    {Object.entries(project.by_severity).map(([sev, count]) => (
                      <span key={sev} className="ap cx-security-sev" data-sev={sev}>{count} {sev}</span>
                    ))}
                    {project.alerts === 0 && <span className="ap faint">no alerts</span>}
                    {project.investigating > 0 && <span className="ap live">{project.investigating} investigating</span>}
                    {project.pending > 0 && <span className="ap faint">{project.pending} judging</span>}
                    {project.quiet > 0 && <span className="ap faint">{project.quiet} set aside</span>}
                    <span className="grow" />
                    {project.last_audit_ms ? <span className="ap faint">audit {ago(project.last_audit_ms, now)}</span> : <span className="ap faint">never audited</span>}
                  </div>
                  {project.error && <div className="cx-security-reason">{project.error}</div>}
                  {project.posture.length > 0 && <div className="cx-security-reason">Settings: {project.posture.join('; ')}</div>}
                </button>
              </li>
            ))}
          </ul>
        </>
      )}
    </div>
  );
}

type Pending = 'accept' | 'gone' | 'reopen' | 'mute';

function FindingDetail({ projectId, root, row, now, startSession, onSession, onChanged, onForgotten }: {
  projectId: string;
  root: string;
  row: SecurityFinding;
  now: number;
  startSession: (title: string, text: string) => Promise<string>;
  onSession: (sessionId: string) => void;
  onChanged: () => void;
  onForgotten: () => void;
}) {
  const client = useClient();
  const [busy, setBusy] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [pending, setPending] = useState<Pending | null>(null);
  const [note, setNote] = useState('');
  const [confirmForget, setConfirmForget] = useState(false);
  const [recheck, setRecheck] = useState<SecurityRecheck | null>(null);
  const [muteKind, setMuteKind] = useState<MuteKind>('finding');
  const detail = useQuery<SecurityFindingDetail>(
    `memory.security.finding:${projectId}:${row.id}`,
    (c) => c.call<SecurityFindingDetail>('memory.security.finding', { project_id: projectId, id: row.id }),
    { topics: ['memory.security.changed'], when: about(projectId, row.id) },
  );
  const finding = detail.data?.finding ?? row;
  const history = detail.data?.history ?? [];
  const status = finding.status;

  const run = async (key: string, work: () => Promise<void>) => {
    setBusy(key);
    setError(null);
    try {
      await work();
    } catch (cause) {
      setError(errorMessage(cause));
    } finally {
      setBusy(null);
    }
  };

  const update = (next: SecurityStatus, text: string, sessionId?: string) =>
    client.call('memory.security.finding.update', {
      project_id: projectId,
      id: finding.id,
      status: next,
      note: text,
      by: 'user',
      ...(sessionId ? { session_id: sessionId } : {}),
    });

  const fix = (quick: boolean) => run(quick ? 'quick' : 'fix', async () => {
    const text = fixMessage(quick ? finding : { ...finding, quick_fix: null });
    const sessionId = await startSession(`Fix: ${finding.title}`, text);
    await update('fixing', 'session started', sessionId);
    onChanged();
    onSession(sessionId);
  });

  /** The filter looks again, or a session investigates it. */
  const lookAgain = () => run('again', async () => {
    await client.call('skills-security.triage', { project_id: projectId, ids: [finding.id], again: true });
    await detail.refresh();
    onChanged();
  });

  const investigate = () => run('investigate', async () => {
    const result = await client.call<InvestigateResult>('skills-security.investigate', { project_id: projectId, id: finding.id });
    await detail.refresh();
    onChanged();
    if (result?.session_id) onSession(result.session_id);
  });

  const unmute = () => run('unmute', async () => {
    if (finding.muted_by) await client.call('skills-security.mute.remove', { key: finding.muted_by });
    await detail.refresh();
    onChanged();
  });

  const move = () => {
    if (!pending) return;
    const text = note.trim();
    if (pending === 'mute') {
      void run('mute', async () => {
        await client.call('skills-security.mute.add', { project_id: projectId, finding_id: finding.id, kind: muteKind, note: text });
        setPending(null);
        setNote('');
        await detail.refresh();
        onChanged();
      });
      return;
    }
    if (pending !== 'reopen' && pending !== 'mute' && !text) return;
    const next: SecurityStatus = pending === 'accept' ? 'accepted' : pending === 'gone' ? 'gone' : 'open';
    void run(pending, async () => {
      await update(next, text);
      setPending(null);
      setNote('');
      await detail.refresh();
      onChanged();
    });
  };

  /** The host compares the sink's evidence with the file now; a gone sink moves the finding. */
  const recheckNow = () => run('recheck', async () => {
    const result = await client.call<{ checked: SecurityRecheck[] }>('memory.security.recheck', { project_id: projectId, ids: [finding.id] });
    setRecheck(result?.checked?.[0] ?? null);
    await detail.refresh();
    onChanged();
  });

  const forget = () => {
    if (!confirmForget) {
      setConfirmForget(true);
      return;
    }
    void run('forget', async () => {
      await client.call('memory.security.forget', { project_id: projectId, id: finding.id });
      setConfirmForget(false);
      onForgotten();
    });
  };

  const begin = (next: Pending) => {
    setConfirmForget(false);
    setError(null);
    setPending(pending === next ? null : next);
    setNote('');
  };

  const openStep = (step: SecurityTraceStep) => openInCode({ root, path: relativeTo(root, step.file), line: step.line || null });
  const sev = severityOf(finding);
  const canFix = status === 'open';
  const canReopen = status === 'accepted' || status === 'fixed' || status === 'verified' || status === 'gone' || status === 'fixing';
  const canForget = CLOSED.includes(status as SecurityStatus);
  const pendingLabel = pending === 'accept' ? 'Accept' : pending === 'gone' ? 'Mark not an issue' : pending === 'mute' ? 'Mute' : 'Reopen';
  const triage = triageOf(finding);
  const active = ACTIVE.includes(status as SecurityStatus);

  return (
    <div className="cx-security-open" data-status={status}>
      <div className="cx-security-grade">
        <span className="ap cx-security-sev" data-sev={sev}>{severityLabel(sev)}</span>
        <span className="ap faint">{finding.verdict.replace(/_/g, ' ')}</span>
        {finding.severity_likelihood && <span className="ap faint">likelihood {finding.severity_likelihood}</span>}
        {finding.severity_impact && <span className="ap faint">impact {finding.severity_impact}</span>}
        {finding.source === 'watch' || finding.source === 'sweep'
          ? <span className="ap faint">found by the security sweep</span>
          : <span className="ap faint">seen in {finding.seen_in} audit{finding.seen_in === 1 ? '' : 's'}</span>}
        {exploitChips(finding).filter((chip) => chip !== 'sweep').map((chip) => (
          <span key={chip} className={`ap ${chip === 'exploited' ? 'cx-security-sev' : 'faint'}`} data-sev={chip === 'exploited' ? 'critical' : undefined}
            title={chip === 'exploited' ? 'CISA Known Exploited Vulnerabilities' : 'FIRST EPSS: the probability of exploitation in the next 30 days'}>{chip}</span>
        ))}
        <span className="grow" />
        <span className="cx-security-id">{finding.id}</span>
      </div>
      {detail.error && !detail.data && <ServiceProblem service="memory" state={detail} className="inline-error" />}

      <dl className="cx-security-facts cx-security-filter" data-triage={triage}>
        <dt className="ap faint">Filter</dt>
        <dd>
          <span className="ap cx-security-triage" data-triage={triage}>{TRIAGE_LABEL[triage]}</span>
          {finding.triage_by && <span className="ap faint"> by {finding.triage_by === 'model' ? 'the local model' : finding.triage_by === 'investigation' ? 'an investigation' : finding.triage_by === 'user' ? 'you' : 'the host'}</span>}
          {finding.triage_reason ? <span className="cx-security-what">{finding.triage_reason}</span> : triage === 'judging' && <span className="cx-security-what">Waiting for the filter.</span>}
        </dd>
        {finding.reachable && <><dt className="ap faint">Reach</dt><dd>{REACH_LABEL[finding.reachable] ?? finding.reachable}</dd></>}
        {(finding.evidence?.length ?? 0) > 0 && (
          <>
            <dt className="ap faint">Evidence</dt>
            <dd><ul className="cx-security-evidence">{finding.evidence?.map((line, index) => <li key={index}>{line}</li>)}</ul></dd>
          </>
        )}
        {finding.investigation_session && (
          <>
            <dt className="ap faint">Investigation</dt>
            <dd><button type="button" className="cx-file-link" onClick={() => onSession(finding.investigation_session as string)}>Open the session</button></dd>
          </>
        )}
      </dl>

      {(finding.principal || finding.result || finding.control || finding.impact) && (
        <dl className="cx-security-facts">
          {finding.principal && <><dt className="ap faint">Who</dt><dd>{finding.principal}</dd></>}
          {finding.result && <><dt className="ap faint">Can</dt><dd>{finding.result}</dd></>}
          {finding.control && <><dt className="ap faint">Because</dt><dd>{finding.control}</dd></>}
          {finding.impact && <><dt className="ap faint">So what</dt><dd>{finding.impact}</dd></>}
        </dl>
      )}

      {finding.verdict === 'needs_validation' && finding.missing_fact && (
        <>
          <span className="ap faint">Decides it</span>
          <p className="cx-security-text">{finding.missing_fact}</p>
        </>
      )}

      {finding.verdict === 'rejected' && finding.reason && (
        <>
          <span className="ap faint">Rejected because</span>
          <p className="cx-security-text">{finding.reason}</p>
        </>
      )}

      {(finding.signal || (finding.advisories?.length ?? 0) > 0) && (
        <dl className="cx-security-facts">
          {finding.signal && <><dt className="ap faint">Host signal</dt><dd>{finding.signal}{finding.host_severity ? ` (at least ${finding.host_severity})` : ''}</dd></>}
          {(finding.advisories?.length ?? 0) > 0 && <><dt className="ap faint">Advisories</dt><dd>{finding.advisories?.join(', ')}</dd></>}
        </dl>
      )}

      {finding.trace.length > 0 && (
        <>
          <span className="ap faint">Trace</span>
          <ol className="cx-security-trace">
            {finding.trace.map((step, index) => (
              <li key={`${step.file}:${step.line}:${index}`} data-kind={step.kind}>
                <span className="ap faint">{step.kind.replace(/_/g, ' ')}</span>
                <button type="button" className="cx-file-link" title={step.file} onClick={() => openStep(step)}>
                  {relativeTo(root, step.file)}{step.line ? `:${step.line}` : ''}
                </button>
                {step.what && <span className="cx-security-what">{step.what}</span>}
              </li>
            ))}
          </ol>
        </>
      )}

      {(finding.fix_change || finding.fix_where || finding.fix_test || finding.quick_fix) && (
        <>
          <span className="ap faint">Fix</span>
          <dl className="cx-security-facts">
            {finding.fix_where && <><dt className="ap faint">Where</dt><dd>{finding.fix_where}</dd></>}
            {finding.fix_change && <><dt className="ap faint">Change</dt><dd>{finding.fix_change}</dd></>}
            {finding.fix_test && <><dt className="ap faint">Prove</dt><dd>{finding.fix_test}</dd></>}
            {finding.quick_fix && (
              <>
                <dt className="ap origin">Quick fix</dt>
                <dd>
                  {finding.quick_fix.package} {finding.quick_fix.from} → {finding.quick_fix.to} in {finding.quick_fix.lockfile}
                  {finding.quick_fix.commands.length > 0 && <pre className="cx-security-commands">{finding.quick_fix.commands.join('\n')}</pre>}
                </dd>
              </>
            )}
          </dl>
        </>
      )}

      {(finding.evidence_how || finding.evidence_observed) && (
        <>
          <span className="ap faint">Evidence</span>
          <dl className="cx-security-facts">
            {finding.evidence_how && <><dt className="ap faint">How</dt><dd>{finding.evidence_how}</dd></>}
            {finding.evidence_observed && <><dt className="ap faint">Observed</dt><dd>{finding.evidence_observed}</dd></>}
          </dl>
        </>
      )}

      {finding.priority_reason && (
        <>
          <span className="ap faint">Priority {finding.priority}</span>
          <p className="cx-security-text">{finding.priority_reason}</p>
        </>
      )}

      {(finding.session_id || finding.fixed_commit) && (
        <p className="cx-security-links">
          {finding.session_id && (
            <button type="button" className="cx-file-link" onClick={() => onSession(finding.session_id as string)}>
              {status === 'fixing' ? 'Open the fix session' : 'Open the session'}
            </button>
          )}
          {finding.fixed_commit && <span className="cx-security-id">commit {finding.fixed_commit.slice(0, 12)}</span>}
        </p>
      )}

      {history.length > 0 && (
        <>
          <span className="ap faint">History</span>
          <ul className="cx-security-history">
            {[...history].sort((a, b) => b.ms - a.ms).map((change, index) => (
              <li key={`${change.ms}:${index}`}>
                {change.triage
                  ? <span className="ap cx-security-triage" data-triage={change.triage === 'pending' ? 'judging' : change.triage}>filter: {TRIAGE_LABEL[(change.triage === 'pending' ? 'judging' : change.triage) as TriageShown] ?? change.triage}</span>
                  : <span className={`ap ${change.status === 'fixing' ? 'live' : ''}`}>{change.status}</span>}
                <span className="ap faint">{change.by}</span>
                <span className="cx-security-when" title={clock(change.ms)}>{ago(change.ms, now)}</span>
                {change.note && <span className="cx-security-what">{change.note}</span>}
                {change.session_id && (
                  <button type="button" className="cx-file-link" onClick={() => onSession(change.session_id as string)}>session</button>
                )}
              </li>
            ))}
          </ul>
        </>
      )}

      {recheck && (
        <p className="cx-security-text" role="status">
          Re-check: {recheck.sentence}{recheck.now ? `; now ${recheck.now}` : ''}.
        </p>
      )}

      {error && <p className="inline-error" role="alert">{error}</p>}
      <div className="cx-security-acts">
        {canFix && (
          <button className="detent" type="button" disabled={busy !== null} onClick={() => void fix(false)}>
            {busy === 'fix' ? 'Starting…' : 'Fix in a session'}
          </button>
        )}
        {canFix && finding.quick_fix && (
          <button className="detent" type="button" disabled={busy !== null} onClick={() => void fix(true)}>
            {busy === 'quick' ? 'Starting…' : 'Quick fix'}
          </button>
        )}
        {status === 'open' && (
          <button className="detent" type="button" aria-pressed={pending === 'accept'} disabled={busy !== null} onClick={() => begin('accept')}>Accept risk</button>
        )}
        {(status === 'open' || status === 'accepted') && (
          <button className="detent" type="button" aria-pressed={pending === 'gone'} disabled={busy !== null} onClick={() => begin('gone')}>Not an issue</button>
        )}
        {active && triage !== 'investigating' && (
          <button className="detent" type="button" disabled={busy !== null} onClick={() => void investigate()}>
            {busy === 'investigate' ? 'Starting…' : 'Investigate'}
          </button>
        )}
        {active && triage !== 'muted' && (
          <button className="detent" type="button" aria-pressed={pending === 'mute'} disabled={busy !== null} onClick={() => begin('mute')}>Mute…</button>
        )}
        {active && triage === 'muted' && finding.muted_by && (
          <button className="detent" type="button" disabled={busy !== null} onClick={() => void unmute()}>{busy === 'unmute' ? 'Unmuting…' : 'Unmute'}</button>
        )}
        {active && triage !== 'investigating' && (
          <button className="detent" type="button" disabled={busy !== null} onClick={() => void lookAgain()}>{busy === 'again' ? 'Judging…' : 'Judge again'}</button>
        )}
        {(status === 'open' || status === 'fixing' || status === 'fixed') && (
          <button className="detent" type="button" disabled={busy !== null} onClick={() => void recheckNow()}>
            {busy === 'recheck' ? 'Checking…' : 'Re-check'}
          </button>
        )}
        {canReopen && (
          <button className="detent" type="button" aria-pressed={pending === 'reopen'} disabled={busy !== null} onClick={() => begin('reopen')}>Reopen</button>
        )}
        {canForget && (
          <button className={`detent${confirmForget ? ' warn' : ''}`} type="button" disabled={busy !== null} onClick={forget}
            onBlur={() => setConfirmForget(false)}>
            {busy === 'forget' ? 'Forgetting…' : confirmForget ? 'Click again to forget' : 'Forget'}
          </button>
        )}
      </div>
      {pending && (
        <div className="cx-security-note">
          {pending === 'mute' && (
            <div className="cx-security-chips" role="group" aria-label="What to mute">
              {MUTE_KINDS.filter((entry) => entry.kind !== 'similar' || finding.fleet_key).filter((entry) => entry.kind !== 'advisory' || (finding.advisories?.length ?? 0) > 0)
                .filter((entry) => entry.kind !== 'package' || finding.quick_fix || finding.fleet_key?.startsWith('dep:'))
                .map((entry) => (
                  <button key={entry.kind} type="button" className="chip" aria-pressed={muteKind === entry.kind} onClick={() => setMuteKind(entry.kind)}>{entry.label}</button>
                ))}
            </div>
          )}
          <textarea className="write one" rows={1} value={note} aria-label="Note"
            placeholder={pending === 'accept' ? 'Why the risk is accepted' : pending === 'gone' ? 'Why it is not an issue' : pending === 'mute' ? 'Why it is muted (optional)' : 'Why it is open again (optional)'}
            onChange={(event) => setNote(event.target.value)}
            onKeyDown={(event) => {
              if (event.key === 'Enter' && !event.shiftKey) {
                event.preventDefault();
                move();
              }
              if (event.key === 'Escape') begin(pending);
            }} />
          <div className="tools">
            <span className="grow" />
            <button className="detent" type="button" disabled={busy !== null} onClick={() => begin(pending)}>Cancel</button>
            <button className="detent send" type="button" disabled={busy !== null || (pending !== 'reopen' && pending !== 'mute' && !note.trim())} onClick={move}>
              {busy === pending ? 'Saving…' : pendingLabel}
            </button>
          </div>
        </div>
      )}
    </div>
  );
}

export default definePlugin({ slot: 'pane.security@1', component: Security });
