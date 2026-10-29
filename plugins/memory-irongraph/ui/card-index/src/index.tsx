/**
 * card.index@1 — the code graph's index of the project, one line: its state, files and symbols
 * (edges, described symbols, the last pass and languages in the tooltip), and refresh — or Index
 * when the graph has not seen the project.
 */

import { useState } from 'react';
import { definePlugin, duration, errorMessage, ServiceProblem, useClient, useHost, useQuery, useSelection } from '@cortex/sdk';
import './index.css';

interface EngineStatus { state: string; bytes_done: number; bytes_total: number; error?: string | null }
interface ProjectStatus { project_id: string; root: string; state: string; files: number; symbols: number; edges: number; described: number; last_index_ms: number; languages?: string[]; error?: string | null }

function IndexSection() {
  const client = useClient();
  const host = useHost();
  const { projectId, project } = useSelection();
  const [busy, setBusy] = useState(false);
  const status = useQuery<ProjectStatus>(projectId ? `memory.project.status:${projectId}` : null, (c) => c.call<ProjectStatus>('memory.project.status', { project_id: projectId }), { topics: ['memory.indexed'] });
  const run = async (method: string, params: Record<string, unknown>, done: string) => {
    setBusy(true);
    try {
      await client.call(method, params);
      await status.refresh();
      host.notify(done);
    } catch (cause) {
      host.notify(errorMessage(cause));
    } finally {
      setBusy(false);
    }
  };
  // The embedding model memory searches with: downloaded on a first start, before any project.
  const engine = useQuery<EngineStatus>('memory.engine.status', (c) => c.call<EngineStatus>('memory.engine.status', {}), { topics: ['memory.engine'] });
  const preparing = engine.data && (engine.data.state === 'installing' || engine.data.state === 'loading') ? engine.data : null;
  const data = status.data;
  if (!projectId && !preparing) return null;
  const mb = (value: number) => (value / 1e6).toFixed(0);
  const engineLine = preparing
    ? preparing.state === 'installing'
      ? `Downloading the search model · ${preparing.bytes_total > 0 ? Math.floor((preparing.bytes_done / preparing.bytes_total) * 100) : 0} % · ${mb(preparing.bytes_done)} of ${mb(preparing.bytes_total)} MB`
      : 'Loading the search model'
    : null;
  const detail = data ? [
    `${data.edges.toLocaleString()} edges · ${data.described.toLocaleString()} described`,
    data.last_index_ms > 0 ? `last pass ${duration(data.last_index_ms)}` : '',
    (data.languages ?? []).join(' · '),
  ].filter(Boolean).join('\n') : undefined;
  return (
    <section className="cx-cs" data-section="index" data-build={__CORTEX_BUILD__}>
      <div className="cx-cs-l"><span className="ap">Index</span></div>
      <div className="cx-cs-b">
        {engineLine && preparing && (
          <div className="cx-index-engine" data-engine-state={preparing.state}>
            {preparing.state === 'installing' && <progress max={preparing.bytes_total || 1} value={preparing.bytes_done} />}
            <span className="ap hot">{engineLine}</span>
          </div>
        )}
        {projectId && <div className="cx-cs-row" title={detail}>
          {data ? (
            <>
              <span className={`ap ${data.state === 'ready' ? 'origin' : data.state === 'error' ? 'live' : 'hot'}`} data-state={data.state} title={data.error ?? undefined}>{data.state}</span>
              <span className="ap faint"><span className="num cx-index-num">{data.files.toLocaleString()}</span> files</span>
              <span className="ap faint"><span className="num cx-index-num">{data.symbols.toLocaleString()}</span> symbols</span>
            </>
          ) : status.error ? (
            status.errorCode === 'unavailable' ? <ServiceProblem service="memory" state={status} /> : <span className="ap faint" title={status.error}>not indexed</span>
          ) : <span className="ap faint">…</span>}
          <span className="grow" />
          {data ? (
            <button className="detent tiny" type="button" disabled={busy} title="Read changed files into the index again" data-action="refresh"
              onClick={() => void run('memory.refresh', { project_id: projectId }, 'Index refreshed')}>{busy ? 'Indexing…' : 'Refresh'}</button>
          ) : (
            <button className="detent tiny" type="button" disabled={busy || !project} onClick={() => void run('memory.project.open', { root: project!.root, project_id: projectId }, `Indexing ${project!.name}`)}>Index</button>
          )}
        </div>}
      </div>
    </section>
  );
}

export default definePlugin({ slot: 'card.index@1', component: IndexSection });
