/**
 * settings.storage@1 — what CortexCode keeps on this Mac (`source.storage`): its home folder, then
 * each part (the model, conversations, build caches, history, …) with the room it takes, each
 * with Show in Finder.
 */

import { useState } from 'react';
import { ServiceProblem, bytes, definePlugin, useHost, useQuery } from '@cortex/sdk';
import './storage.css';

/** `source.storage` (`protocol/src/contracts/source.rs`, `Storage`). */
interface Storage {
  home: string;
  items: Array<{ name: string; path: string; bytes: number }>;
}

function StoragePanel() {
  const host = useHost();
  const storage = useQuery<Storage>('source.storage', (c) => c.call<Storage>('source.storage', {}), { topics: ['source.changed'] });
  const [error, setError] = useState<string | null>(null);
  const reveal = (path: string) => {
    setError(null);
    void host.shell?.reveal(path).then((problem) => setError(problem || null));
  };
  const show = (path: string) => (host.shell ? <button className="detent" type="button" data-action="reveal" onClick={() => reveal(path)}>Show in Finder</button> : null);
  const s = storage.data;
  const total = (s?.items ?? []).reduce((sum, item) => sum + item.bytes, 0);
  return (
    <div className="cx-settings-panel-body" data-build={__CORTEX_BUILD__} data-panel="storage">
      <h3 className="cx-settings-title">Storage</h3>
      <p className="cx-settings-line">What CortexCode keeps on this Mac.</p>
      {storage.error && !s && <ServiceProblem service="source manager" state={storage} className="inline-error" />}
      {!s && !storage.error && <p className="hint">Measuring…</p>}
      {s && (
        <>
          <div className="row" data-home>
            <div className="grow">
              <div className="nm">Home folder</div>
              <div className="cx-storage-note cx-path">{s.home}</div>
            </div>
            {show(s.home)}
          </div>
          <table className="tbl cx-storage">
            <tbody>
              {[...s.items].sort((a, b) => b.bytes - a.bytes).map((item) => (
                <tr key={item.path} data-item={item.name}>
                  <td>{item.name}<span className="cx-storage-note cx-path">{item.path}</span></td>
                  <td className="num">{bytes(item.bytes)}</td>
                  <td>{show(item.path)}</td>
                </tr>
              ))}
              <tr className="cx-storage-total">
                <td>In all</td>
                <td className="num">{bytes(total)}</td>
                <td />
              </tr>
            </tbody>
          </table>
        </>
      )}
      {error && <p className="inline-error">{error}</p>}
    </div>
  );
}

export default definePlugin({ slot: 'settings.storage@1', component: StoragePanel });
