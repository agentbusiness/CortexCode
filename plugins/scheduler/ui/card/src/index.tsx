/**
 * card.schedule@1 — the next routines of the shown project and the global ones, one line each:
 * what, how soon, and its state when it is not running on time. A line opens the Schedule pane.
 * No routine: nothing.
 */

import { definePlugin, showPane, useNow, useQuery, useSelection, ServiceProblem } from '@cortex/sdk';
import { soon, type Routine } from './types';
import './card.css';

function ScheduleSection() {
  const { projectId } = useSelection();
  const now = useNow(30_000, true);
  const listed = useQuery<Routine[]>(
    `schedule.list:${projectId ?? 'global'}`,
    async (c) => (await c.call<{ routines: Routine[] }>('schedule.list', { scope: projectId ?? 'global', with_global: true }))?.routines ?? [],
    { topics: ['schedule.changed'] },
  );
  const all = (listed.data ?? []).filter((routine) => routine.state !== 'ended');
  if (listed.error && !listed.data) return <section className="cx-cs" data-section="schedule"><div className="cx-cs-b"><ServiceProblem service="schedule" state={listed} className="hint" /></div></section>;
  if (all.length === 0) return null;
  const next = [...all].sort((a, b) => (a.state === 'active' ? a.next_ms ?? Infinity : Infinity) - (b.state === 'active' ? b.next_ms ?? Infinity : Infinity)).slice(0, 4);
  return (
    <section className="cx-cs" data-section="schedule" data-build={__CORTEX_BUILD__}>
      <div className="cx-cs-l">
        <span className="ap">Schedule</span>
        {all.length > next.length && <span className="ap faint sub">{all.length}</span>}
      </div>
      <div className="cx-cs-b">
        {next.map((routine) => (
          <button key={routine.id} type="button" className="cx-cs-row" data-routine={routine.id} data-state={routine.state} onClick={() => showPane('schedule')}
            title={`${routine.title}\n${routine.kind} · ${routine.when}${routine.scope === 'global' ? ' · global' : ''}${routine.last_outcome ? `\nlast run: ${routine.last_outcome}` : ''}\nOpen the schedule`}>
            <span className={routine.state === 'active' ? 'ap' : 'ap faint'}>{routine.kind === 'command' ? '$' : routine.kind === 'handoff' ? '→' : '⏱'}</span>
            <span className="t">{routine.title}</span>
            <span className="grow" />
            {routine.state === 'active' && routine.next_ms
              ? <span className={routine.last_outcome === 'failed' ? 'ap hot' : 'ap faint'}>{soon(routine.next_ms, now)}</span>
              : <span className="ap faint">{routine.state}</span>}
          </button>
        ))}
      </div>
    </section>
  );
}

export default definePlugin({ slot: 'card.schedule@1', component: ScheduleSection });
