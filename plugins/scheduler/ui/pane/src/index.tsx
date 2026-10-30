/**
 * pane.schedule@1 — every routine of the shown project (or the global ones): what it does, when,
 * its state and its runs. A routine is edited in place (when, title, brief or command), paused,
 * resumed, run now or removed; a run opens its session. A form adds a routine.
 */

import { useState } from 'react';
import { Menu, ago, definePlugin, errorMessage, openTarget, useClient, useNow, useQuery, useSelection, useShared, useSlotState, ServiceProblem } from '@cortex/sdk';
import { soon, type Routine, type RoutineRun } from './types';
import './schedule.css';

type Scope = 'project' | 'global';
const KINDS = ['handoff', 'command', 'wake'] as const;
const KIND_HINT: Record<string, string> = {
  handoff: 'An agent does the brief in a session of its own; the result comes back there.',
  command: 'The command runs in the project; a failure wakes the session.',
  wake: 'The open conversation is woken with the brief.',
};

function Schedule() {
  const { projectId, project, sessionId } = useSelection();
  const [scope, setScope] = useShared<Scope>('schedule.scope', 'project');
  const shown = scope === 'project' && projectId ? projectId : 'global';
  const [adding, setAdding] = useState(false);
  const listed = useQuery<Routine[]>(
    `schedule.list:${shown}:only`,
    async (c) => (await c.call<{ routines: Routine[] }>('schedule.list', { scope: shown }))?.routines ?? [],
    { topics: ['schedule.changed'] },
  );
  const routines = [...(listed.data ?? [])].sort((a, b) => order(a) - order(b) || (a.next_ms ?? 0) - (b.next_ms ?? 0));
  return (
    <div className="cx-schedule" data-build={__CORTEX_BUILD__} data-scope={scope}>
      <div className="bar">
        <div className="seg-set" role="group" aria-label="Schedule">
          <button type="button" aria-pressed={scope === 'project'} disabled={!projectId} onClick={() => setScope('project')}>{project?.name ?? 'This project'}</button>
          <button type="button" aria-pressed={scope === 'global'} onClick={() => setScope('global')}>Global</button>
        </div>
        <div className="grow" />
        <button type="button" className="detent" aria-expanded={adding} onClick={() => setAdding((open) => !open)}>{adding ? 'Close' : 'New routine'}</button>
      </div>
      <div className="cx-schedule-scroll">
        {adding && <AddForm scope={shown} sessionId={scope === 'project' ? sessionId : null} onDone={() => { setAdding(false); void listed.refresh(); }} />}
        {listed.error && !listed.data && <ServiceProblem service="schedule" state={listed} className="inline-error" />}
        {listed.data && routines.length === 0 && !adding && (
          <p className="empty-line">No routines {scope === 'global' ? 'outside projects' : 'in this project'}. The model makes them when asked (“check the agents every 5 minutes”, “a security search every week”), or add one here.</p>
        )}
        <ul className="lst cx-routines">
          {routines.map((routine) => <RoutineRow key={routine.id} routine={routine} />)}
        </ul>
      </div>
    </div>
  );
}

function order(routine: Routine): number {
  return routine.state === 'active' ? 0 : routine.state === 'paused' ? 1 : 2;
}

function AddForm({ scope, sessionId, onDone }: { scope: string; sessionId: string | null; onDone: () => void }) {
  const client = useClient();
  const [kind, setKind] = useState<(typeof KINDS)[number]>('handoff');
  const [when, setWhen] = useState('');
  const [title, setTitle] = useState('');
  const [what, setWhat] = useState('');
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const kinds = KINDS.filter((entry) => entry !== 'wake' || sessionId);
  const add = async () => {
    setBusy(true);
    setError(null);
    try {
      await client.call('schedule.add', {
        scope, kind, when, title: title || undefined, by: 'user',
        brief: kind === 'command' ? undefined : what, command: kind === 'command' ? what : undefined,
        session_id: kind === 'wake' ? sessionId : undefined,
      });
      onDone();
    } catch (cause) {
      setError(errorMessage(cause));
    } finally {
      setBusy(false);
    }
  };
  return (
    <div className="cx-routine-form">
      <div className="cx-routine-fields">
        <Menu label="Kind" value={kind} items={kinds.map((entry) => ({ key: entry, label: entry, selected: entry === kind, onSelect: () => setKind(entry) }))} />
        <input type="text" value={when} placeholder="every mon at 09:00" aria-label="When" onChange={(event) => setWhen(event.target.value)} />
        <input type="text" value={title} placeholder="Title (optional)" aria-label="Title" onChange={(event) => setTitle(event.target.value)} />
      </div>
      <p className="ap faint">{KIND_HINT[kind]}</p>
      <textarea rows={kind === 'command' ? 1 : 3} value={what} aria-label={kind === 'command' ? 'Command' : 'Brief'}
        placeholder={kind === 'command' ? 'cargo audit' : 'Search for new security advisories that affect this project’s dependencies and report them.'}
        onChange={(event) => setWhat(event.target.value)} />
      {error && <p className="inline-error" role="alert">{error}</p>}
      <div className="cx-routine-acts">
        <span className="ap faint">every 5m · every day at 09:00 · weekdays at 8am · once at 2026-11-02 09:00 · in 20m · cron 0 9 * * 1</span>
        <div className="grow" />
        <button type="button" className="detent send" disabled={busy || !when.trim() || !what.trim()} onClick={() => void add()}>{busy ? 'Adding…' : 'Add'}</button>
      </div>
    </div>
  );
}

function RoutineRow({ routine }: { routine: Routine }) {
  const client = useClient();
  const now = useNow(30_000, true);
  const [openIds, setOpenIds] = useSlotState<string[]>('openRoutines', []);
  const open = openIds.includes(routine.id);
  const [busy, setBusy] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const act = async (key: string, method: string, params: Record<string, unknown>) => {
    setBusy(key);
    setError(null);
    try {
      await client.call(method, { id: routine.id, ...params });
    } catch (cause) {
      setError(errorMessage(cause));
    } finally {
      setBusy(null);
    }
  };
  const status = routine.state === 'active'
    ? (routine.next_ms ? soon(routine.next_ms, now) : 'due')
    : `${routine.state}${routine.state_reason && routine.state_reason !== routine.state ? `: ${routine.state_reason}` : ''}`;
  return (
    <li data-routine={routine.id} data-state={routine.state}>
      <button type="button" aria-expanded={open} onClick={() => setOpenIds((list) => (list.includes(routine.id) ? list.filter((id) => id !== routine.id) : [...list, routine.id]))}>
        <div className="t">{routine.title}</div>
        <div className="r">
          <span className="ap">{routine.kind}</span>
          <span className="ap faint">{routine.when}</span>
          {routine.last_outcome && <span className={`ap ${routine.last_outcome === 'failed' ? 'hot' : 'faint'}`}>last: {routine.last_outcome}</span>}
          <span className="grow" />
          <span className={`ap ${routine.state === 'active' ? 'live' : 'faint'}`}>{status}</span>
        </div>
      </button>
      {open && (
        <div className="cx-routine-open">
          <Editor routine={routine} busy={busy !== null} onSave={(fields) => void act('save', 'schedule.edit', fields)} />
          {error && <p className="inline-error" role="alert">{error}</p>}
          <div className="cx-routine-acts">
            {routine.state === 'active'
              ? <button type="button" className="detent" disabled={busy !== null} onClick={() => void act('pause', 'schedule.pause', {})}>{busy === 'pause' ? 'Pausing…' : 'Pause'}</button>
              : <button type="button" className="detent" disabled={busy !== null} onClick={() => void act('resume', 'schedule.resume', {})}>{busy === 'resume' ? 'Resuming…' : 'Resume'}</button>}
            <button type="button" className="detent" disabled={busy !== null} onClick={() => void act('run', 'schedule.run_now', {})}>{busy === 'run' ? 'Starting…' : 'Run now'}</button>
            <div className="grow" />
            <span className="ap faint">{routine.run_count} run{routine.run_count === 1 ? '' : 's'} · by {routine.by} · {ago(routine.created_ms, now)}</span>
            <button type="button" className="detent warn" disabled={busy !== null} onClick={() => void act('remove', 'schedule.remove', {})}>{busy === 'remove' ? 'Removing…' : 'Remove'}</button>
          </div>
          <Runs routine={routine} />
        </div>
      )}
    </li>
  );
}

function Editor({ routine, busy, onSave }: { routine: Routine; busy: boolean; onSave: (fields: Record<string, unknown>) => void }) {
  const [when, setWhen] = useState(routine.when);
  const [title, setTitle] = useState(routine.title);
  const what0 = routine.kind === 'command' ? routine.command : routine.brief;
  const [what, setWhat] = useState(what0);
  const changed = when !== routine.when || title !== routine.title || what !== what0;
  const save = () => onSave({ when: when !== routine.when ? when : undefined, title: title !== routine.title ? title : undefined, ...(routine.kind === 'command' ? { command: what } : { brief: what }) });
  return (
    <div className="cx-routine-form">
      <div className="cx-routine-fields">
        <input type="text" value={when} aria-label="When" onChange={(event) => setWhen(event.target.value)} />
        <input type="text" value={title} aria-label="Title" onChange={(event) => setTitle(event.target.value)} />
      </div>
      {!routine.manage && (
        <textarea rows={routine.kind === 'command' ? 1 : 3} value={what} aria-label={routine.kind === 'command' ? 'Command' : 'Brief'} onChange={(event) => setWhat(event.target.value)} />
      )}
      {routine.manage && <p className="ap faint">Wakes the conversation to check on the agents it started; ends when their work is finished.</p>}
      {changed && (
        <div className="cx-routine-acts">
          <div className="grow" />
          <button type="button" className="detent" disabled={busy} onClick={() => { setWhen(routine.when); setTitle(routine.title); setWhat(what0); }}>Undo</button>
          <button type="button" className="detent send" disabled={busy || !when.trim()} onClick={save}>Save</button>
        </div>
      )}
    </div>
  );
}

function Runs({ routine }: { routine: Routine }) {
  const now = useNow(30_000, true);
  const runs = useQuery<RoutineRun[]>(
    `schedule.runs:${routine.id}`,
    async (c) => (await c.call<{ runs: RoutineRun[] }>('schedule.runs', { id: routine.id, limit: 10 }))?.runs ?? [],
    { topics: ['schedule.changed', 'schedule.ran'] },
  );
  if (!runs.data || runs.data.length === 0) return <p className="ap faint">No runs yet.</p>;
  return (
    <ul className="cx-routine-runs">
      {runs.data.map((run) => (
        <li key={run.id} data-outcome={run.outcome}>
          <span className={`ap ${run.outcome === 'failed' ? 'hot' : run.outcome === 'running' ? 'live' : 'faint'}`}>{run.outcome}</span>
          <span className="ap faint">{ago(run.started_ms, now)}</span>
          <span className="cx-run-summary" title={run.summary}>{run.summary.split('\n').find((line) => line.trim()) ?? ''}</span>
          {run.session_id && <button type="button" className="detent tiny" onClick={() => void openTarget({ open: 'session', session_id: run.session_id })}>Open</button>}
        </li>
      ))}
    </ul>
  );
}

export default definePlugin({ slot: 'pane.schedule@1', component: Schedule });
