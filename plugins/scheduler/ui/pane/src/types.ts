/** A routine as `schedule.*` answers it (memory's `Routine`). */
export interface Routine {
  id: string;
  scope: string;
  title: string;
  kind: 'wake' | 'handoff' | 'command' | string;
  brief: string;
  command: string;
  when: string;
  next_ms?: number | null;
  last_due_ms?: number | null;
  state: 'active' | 'paused' | 'ended' | string;
  state_reason: string;
  report_to?: string | null;
  home_session?: string | null;
  watches: string[];
  manage: boolean;
  until_ms?: number | null;
  max_runs?: number | null;
  run_count: number;
  fail_streak: number;
  last_outcome: string;
  last_summary: string;
  last_ok_ms?: number | null;
  mode: string;
  by: string;
  created_ms: number;
  updated_ms: number;
}

export interface RoutineRun {
  id: string;
  routine_id: string;
  due_ms: number;
  started_ms: number;
  ended_ms?: number | null;
  outcome: string;
  summary: string;
  session_id?: string | null;
  task_id?: string | null;
}

/** How soon: `in 4m`, `in 2h`, then the day and time. */
export function soon(ms: number, now: number): string {
  const left = ms - now;
  if (left <= 30_000) return 'now';
  if (left < 3_600_000) return `in ${Math.round(left / 60_000)}m`;
  if (left < 12 * 3_600_000) return `in ${Math.round(left / 3_600_000)}h`;
  const date = new Date(ms);
  return date.toLocaleString(undefined, { weekday: 'short', day: 'numeric', month: 'short', hour: '2-digit', minute: '2-digit' });
}
