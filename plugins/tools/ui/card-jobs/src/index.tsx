/**
 * card.jobs@1 — the project's running background jobs, under the model: each job's command, how
 * long it has run and how much memory its processes hold (read every two seconds while the card is
 * open), with Stop (SIGTERM: the program may clean up) and Kill (SIGKILL now). The job's output
 * then ends saying the user stopped it, so the model does not start it again unasked. Nothing
 * running: nothing.
 */

import { useEffect, useState } from 'react';
import { definePlugin, errorMessage, useClient, useHost, useQuery, useSelection, useShared } from '@cortex/sdk';
import './jobs.css';

interface Job {
  job_id: string;
  state: string;
  started_ms: number;
  command?: string;
  cwd?: string;
  memory_bytes?: number;
}

/** Bytes in words: `41 GB`, `3.2 GB`, `640 MB`. */
export function size(bytes: number): string {
  const gb = bytes / 1024 ** 3;
  if (gb >= 10) return `${gb.toFixed(0)} GB`;
  if (gb >= 1) return `${gb.toFixed(1)} GB`;
  return `${Math.round(bytes / 1024 ** 2)} MB`;
}

/** How long since `ms`: `40 s`, `9 min`, `2 h 5 min`. */
export function age(ms: number, now: number): string {
  const seconds = Math.max(0, Math.floor((now - ms) / 1000));
  if (seconds < 60) return `${seconds} s`;
  if (seconds < 3600) return `${Math.floor(seconds / 60)} min`;
  return `${Math.floor(seconds / 3600)} h ${Math.floor((seconds % 3600) / 60)} min`;
}

function JobsSection() {
  const client = useClient();
  const host = useHost();
  const { projectId } = useSelection();
  const [open] = useShared<boolean>('card.open', false);
  const jobs = useQuery<Job[]>(
    projectId ? `tools.job.list:running:${projectId}` : null,
    async (c) => (await c.call<{ jobs: Job[] }>('tools.job.list', { running: true, project_id: projectId }))?.jobs ?? [],
    { topics: ['tools.job'] },
  );
  const [asked, setAsked] = useState<Record<string, 'stop' | 'kill'>>({});
  const [now, setNow] = useState(() => Date.now());

  useEffect(() => {
    if (!open || !projectId) return;
    void jobs.refresh();
    const timer = setInterval(() => {
      setNow(Date.now());
      void jobs.refresh();
    }, 2000);
    return () => clearInterval(timer);
    // `jobs.refresh` reads the current query.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [open, projectId]);

  const list = (jobs.data ?? []).filter((job) => job.state === 'running');
  if (!projectId || list.length === 0) return null;

  const signal = async (job: Job, mode: 'stop' | 'kill') => {
    setAsked((current) => ({ ...current, [job.job_id]: mode }));
    try {
      await client.call('tools.job.kill', { job_id: job.job_id, mode });
    } catch (cause) {
      host.notify(errorMessage(cause));
    } finally {
      void jobs.refresh();
    }
  };

  return (
    <section className="cx-cs" data-section="jobs" data-build={__CORTEX_BUILD__}>
      <div className="cx-cs-l"><span className="ap">Jobs</span></div>
      <div className="cx-cs-b">
        {list.map((job) => {
          const command = job.command ?? job.job_id;
          const pending = asked[job.job_id];
          const title = [command, job.cwd ? `in ${job.cwd}` : '', `started ${new Date(job.started_ms).toLocaleString()}`, job.job_id].filter(Boolean).join('\n');
          return (
            <div key={job.job_id} className="cx-cs-row cx-job" data-job={job.job_id} title={title}>
              <span className="t cx-mono">{command}</span>
              <span className="grow" />
              <span className="ap faint">{age(job.started_ms, now)}</span>
              {job.memory_bytes !== undefined && job.memory_bytes > 0 && <span className="ap faint" data-memory={job.memory_bytes}>{size(job.memory_bytes)}</span>}
              {pending === 'stop' && <span className="ap hot">stopping</span>}
              <button type="button" className="detent tiny" data-action="stop" disabled={pending !== undefined} onClick={() => void signal(job, 'stop')} title="Ask it to stop (SIGTERM): it may clean up first">Stop</button>
              <button type="button" className="detent tiny warn" data-action="kill" disabled={pending === 'kill'} onClick={() => void signal(job, 'kill')} title="End it and everything it started now (SIGKILL)">Kill</button>
            </div>
          );
        })}
      </div>
    </section>
  );
}

export default definePlugin({ slot: 'card.jobs@1', component: JobsSection });
