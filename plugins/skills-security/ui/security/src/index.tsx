/**
 * pane.security@1 — the project's security findings as the memory plugin keeps them
 * (`memory.security.*`): the last audit and the counts by severity, the active findings in priority
 * order, one finding's trace, fix, evidence and history, and the moves on it (a fix session, accept
 * the risk, not an issue, reopen, forget). An audit or a fix is a conversation: the pane creates
 * the session and sends its first message, as the composer does.
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
  type Session,
} from '@cortex/sdk';
import type { SecurityChanged, SecurityFinding, SecurityFindingDetail, SecurityStatus, SecuritySummary, SecurityTraceStep } from './types';
import './security.css';

/** The chip a finding wears: its overall severity, `lead` for a lead, `note` for a hardening note. */
const SEVERITIES = ['critical', 'high', 'medium', 'low', 'lead', 'hardening'] as const;
type Severity = (typeof SEVERITIES)[number] | 'unrated';
type Mode = 'ask' | 'auto';

const ACTIVE: SecurityStatus[] = ['open', 'fixing', 'accepted'];
const CLOSED: SecurityStatus[] = ['fixed', 'verified', 'gone'];
const AUDIT_TEXT = 'security audit of this codebase';
const RESOLVE = 'python3 "$CORTEX_HOME/code/plugins/skills-security/data/scripts/secaudit.py" resolve';

export function severityOf(finding: Pick<SecurityFinding, 'verdict' | 'severity_overall'>): Severity {
  if (finding.verdict === 'needs_validation') return 'lead';
  if (finding.verdict === 'hardening') return 'hardening';
  const overall = finding.severity_overall;
  return overall && (SEVERITIES as readonly string[]).includes(overall) ? (overall as Severity) : 'unrated';
}

export function severityLabel(severity: Severity): string {
  return severity === 'hardening' ? 'note' : severity;
}

/** The sink's place for a row: `path:line`, or the path alone. */
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
  const [selected, setSelected] = useShared<string | null>('security.selected', null);
  const [severity, setSeverity] = useShared<Severity | null>('security.severity', null);
  const [showFixed, setShowFixed] = useShared<boolean>('security.showFixed', false);
  const [auditing, setAuditing] = useState(false);
  const [auditError, setAuditError] = useState<string | null>(null);
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

  if (!projectId || !project) return <div className="cx-security"><p className="empty-line">Open a project to see its security findings.</p></div>;

  /** A conversation that starts with one message, as the composer sends it. */
  const startSession = async (title: string, text: string): Promise<string> => {
    const created = await client.call<Session>('frame.session.create', { project_id: projectId, kind: 'chat', title });
    setShared('chat.stick', Date.now());
    await client.call('chatflow.turn', { session_id: created.id, text, mode });
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
  const rows = (findings.data ?? []).filter((finding) => !severity || severityOf(finding) === severity);
  const noAuditYet = data !== undefined && !lastAudit && findings.data !== undefined && findings.data.length === 0;

  return (
    <div className="cx-security" data-build={__CORTEX_BUILD__}>
      <div className="bar cx-security-head">
        {lastAudit ? (
          <span className="ap faint" title={lastAudit.scope ? `scope: ${lastAudit.scope}` : undefined}>
            Audit {ago(lastAudit.finished_ms ?? lastAudit.started_ms, now)}
            {lastAudit.finished_ms ? '' : ' · running'} · {lastAudit.units} unit{lastAudit.units === 1 ? '' : 's'} · {lastAudit.lines_judged} line{lastAudit.lines_judged === 1 ? '' : 's'} judged
          </span>
        ) : (
          <span className="ap faint">{data ? 'No audit yet' : 'Reading…'}</span>
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
        <button className="detent" type="button" aria-pressed={showFixed} onClick={() => setShowFixed(!showFixed)}>Show fixed</button>
      </div>
      {auditError && <p className="inline-error" role="alert">{auditError}</p>}
      <div className="cx-security-scroll">
        {summary.error && !summary.data && <ServiceProblem service="memory" state={summary} className="inline-error" />}
        {findings.error && !findings.data && !summary.error && <ServiceProblem service="memory" state={findings} className="inline-error" />}
        {noAuditYet && (
          <div className="cx-security-empty">
            <p className="empty-line">No audit yet. An audit maps the stack, hunts each attack class and records what it finds here.</p>
            <div className="cx-security-empty-acts">{auditButton}</div>
          </div>
        )}
        {!noAuditYet && findings.data && findings.data.length === 0 && (
          <p className="empty-line">{showFixed ? 'No findings.' : 'No active findings.'}</p>
        )}
        {!noAuditYet && findings.data && findings.data.length > 0 && rows.length === 0 && (
          <p className="empty-line">No {severity ? severityLabel(severity) : ''} findings in this list.</p>
        )}
        {rows.length > 0 && (
          <ul className="lst cx-security-list">
            {rows.map((finding) => {
              const sev = severityOf(finding);
              const open = selected === finding.id;
              const place = placeOf(finding.sink_path, finding.sink_line);
              return (
                <li key={finding.id} className={open ? 'sel' : undefined} data-finding={finding.id} data-status={finding.status} data-severity={sev}>
                  <button type="button" aria-expanded={open} onClick={() => setSelected(open ? null : finding.id)}>
                    <div className="t">{finding.title}</div>
                    <div className="r">
                      <span className="ap cx-security-sev" data-sev={sev}>{severityLabel(sev)}</span>
                      {place && <span className="cx-security-place">{place}</span>}
                      <span className="ap faint">{finding.class}</span>
                      <span className={`ap ${finding.status === 'fixing' ? 'live' : 'faint'}`}>{finding.status}</span>
                      {finding.quick_fix && <span className="ap origin">quick fix</span>}
                      <span className="grow" />
                      <span className="ap faint">{ago(finding.last_seen_ms, now)}</span>
                    </div>
                  </button>
                  {open && (
                    <FindingDetail projectId={projectId} root={project.root} row={finding} now={now}
                      startSession={startSession} onSession={(sessionId) => select(projectId, sessionId)}
                      onChanged={changed} onForgotten={() => { setSelected(null); changed(); }} />
                  )}
                </li>
              );
            })}
          </ul>
        )}
      </div>
    </div>
  );
}

type Pending = 'accept' | 'gone' | 'reopen';

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

  const move = () => {
    if (!pending) return;
    const text = note.trim();
    if (pending !== 'reopen' && !text) return;
    const next: SecurityStatus = pending === 'accept' ? 'accepted' : pending === 'gone' ? 'gone' : 'open';
    void run(pending, async () => {
      await update(next, text);
      setPending(null);
      setNote('');
      await detail.refresh();
      onChanged();
    });
  };

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
  const pendingLabel = pending === 'accept' ? 'Accept' : pending === 'gone' ? 'Mark not an issue' : 'Reopen';

  return (
    <div className="cx-security-open" data-status={status}>
      <div className="cx-security-grade">
        <span className="ap cx-security-sev" data-sev={sev}>{severityLabel(sev)}</span>
        <span className="ap faint">{finding.verdict.replace(/_/g, ' ')}</span>
        {finding.severity_likelihood && <span className="ap faint">likelihood {finding.severity_likelihood}</span>}
        {finding.severity_impact && <span className="ap faint">impact {finding.severity_impact}</span>}
        <span className="ap faint">seen in {finding.seen_in} audit{finding.seen_in === 1 ? '' : 's'}</span>
        <span className="grow" />
        <span className="cx-security-id">{finding.id}</span>
      </div>
      {detail.error && !detail.data && <ServiceProblem service="memory" state={detail} className="inline-error" />}

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
                <span className={`ap ${change.status === 'fixing' ? 'live' : ''}`}>{change.status}</span>
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
          <textarea className="write one" rows={1} value={note} aria-label="Note"
            placeholder={pending === 'accept' ? 'Why the risk is accepted' : pending === 'gone' ? 'Why it is not an issue' : 'Why it is open again (optional)'}
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
            <button className="detent send" type="button" disabled={busy !== null || (pending !== 'reopen' && !note.trim())} onClick={move}>
              {busy === pending ? 'Saving…' : pendingLabel}
            </button>
          </div>
        </div>
      )}
    </div>
  );
}

export default definePlugin({ slot: 'pane.security@1', component: Security });
