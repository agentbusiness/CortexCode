/**
 * card.knowledge@1 — what the project knows (docs/PROJECT-KNOWLEDGE-PLAN.md P6): the knowledge
 * items of the project (`memory.knowledge.list`, history included) by kind, each with its source,
 * date, confidence, bound code and evidence; Withdraw a live item, mark a theory supported or
 * refuted, answer a question, add an item by hand. Refreshes on `memory.knowledge.changed`.
 */

import { useState } from 'react';
import { definePlugin, errorMessage, ServiceProblem, useClient, useHost, useQuery, useSelection } from '@cortex/sdk';
import './index.css';

interface KnowledgeLink { rel: string; target: string; id?: string; name?: string }
interface KnowledgeItem {
  id: string; kind: string; name: string; text: string; state: string; source: string; confidence: number;
  as_of?: string | null; by?: string; evidence?: string[]; aliases?: string[]; links?: KnowledgeLink[];
  superseded_by?: string | null; reason?: string | null; created_ms: number; updated_ms: number;
}

const TABS: Array<[string, string]> = [['concept', 'Concepts'], ['fact', 'Facts'], ['decision', 'Decisions'], ['hypothesis', 'Theories'], ['question', 'Questions']];
const LIVE = (state: string) => state !== 'withdrawn' && state !== 'superseded';

function dateOf(item: KnowledgeItem): string {
  if (item.as_of) return item.as_of;
  const d = new Date(item.created_ms);
  return Number.isFinite(d.getTime()) ? d.toISOString().slice(0, 10) : '';
}

function KnowledgeSection() {
  const client = useClient();
  const host = useHost();
  const { projectId } = useSelection();
  const [tab, setTab] = useState('concept');
  const [busy, setBusy] = useState<string | null>(null);
  const [adding, setAdding] = useState(false);
  const [draft, setDraft] = useState({ kind: 'fact', name: '', text: '' });
  const items = useQuery<{ items: KnowledgeItem[] }>(
    projectId ? `memory.knowledge.list:${projectId}` : null,
    (c) => c.call<{ items: KnowledgeItem[] }>('memory.knowledge.list', { project_id: projectId, include_history: true }),
    { topics: ['memory.knowledge.changed'] },
  );
  if (!projectId) return null;
  const run = async (key: string, method: string, params: Record<string, unknown>, done: string) => {
    setBusy(key);
    try {
      await client.call(method, { project_id: projectId, ...params });
      await items.refresh();
      if (done) host.notify(done);
    } catch (cause) {
      host.notify(errorMessage(cause));
    } finally {
      setBusy(null);
    }
  };
  const all = items.data?.items ?? [];
  const shown = all.filter((item) => item.kind === tab).sort((a, b) => Number(LIVE(b.state)) - Number(LIVE(a.state)) || b.updated_ms - a.updated_ms);
  const count = (kind: string) => all.filter((item) => item.kind === kind && LIVE(item.state)).length;
  const codeOf = (item: KnowledgeItem) => (item.links ?? []).filter((l) => l.rel === 'implemented_by' || l.rel === 'about' || l.rel === 'described_in').map((l) => l.name || l.id || l.target);
  return (
    <section className="cx-cs" data-section="knowledge" data-build={__CORTEX_BUILD__}>
      <div className="cx-cs-l"><span className="ap">Memory</span></div>
      <div className="cx-cs-b">
        {items.error && items.errorCode === 'unavailable' ? <ServiceProblem service="memory" state={items} /> : null}
        <div className="cx-kn-tabs" role="tablist" aria-label="Kinds">
          {TABS.map(([kind, label]) => (
            <button key={kind} type="button" role="tab" aria-pressed={tab === kind} data-kind={kind} onClick={() => setTab(kind)}>{label} · {count(kind)}</button>
          ))}
          <span className="grow" />
          <button type="button" className="detent tiny" data-action="add" onClick={() => setAdding((v) => !v)}>{adding ? 'Close' : 'Add'}</button>
        </div>
        {adding && (
          <form className="cx-kn-add" aria-label="Add an item" onSubmit={(event) => {
            event.preventDefault();
            if (!draft.name.trim() || !draft.text.trim()) return;
            void run('add', 'memory.knowledge.upsert', { kind: draft.kind, name: draft.name.trim(), text: draft.text.trim(), source: 'user', by: 'user' }, 'Knowledge item added').then(() => {
              setDraft({ kind: draft.kind, name: '', text: '' });
              setAdding(false);
            });
          }}>
            <select aria-label="Kind" value={draft.kind} onChange={(event) => setDraft({ ...draft, kind: event.target.value })}>
              {TABS.map(([kind, label]) => <option key={kind} value={kind}>{label.replace(/s$/, '').replace('Theorie', 'Theory')}</option>)}
            </select>
            <input aria-label="Name" placeholder="Name (a few words)" value={draft.name} onChange={(event) => setDraft({ ...draft, name: event.target.value })} />
            <textarea aria-label="Text" placeholder="One or two sentences" rows={2} value={draft.text} onChange={(event) => setDraft({ ...draft, text: event.target.value })} />
            <div className="cx-kn-acts"><button type="submit" className="detent tiny" disabled={busy !== null}>Keep</button></div>
          </form>
        )}
        {shown.length === 0 ? (
          <p className="ap faint">{items.data ? 'Nothing yet. Items come from what you tell the assistant, from accepted decisions, from agents and from research.' : items.error ? `not available: ${items.error}` : '…'}</p>
        ) : (
          <ul className="cx-kn-list" aria-label={TABS.find(([kind]) => kind === tab)?.[1]}>
            {shown.map((item) => {
              const live = LIVE(item.state);
              const code = codeOf(item);
              const evidence = item.evidence ?? [];
              return (
                <li key={item.id} className="cx-kn-item" data-id={item.id} data-state={item.state} data-live={String(live)}>
                  <div className="cx-kn-head">
                    <span className="cx-kn-name">{item.name}</span>
                    <span className="ap faint cx-kn-meta">{item.state}</span>
                    <span className="ap faint cx-kn-meta">{item.source}{item.by && item.by !== item.source ? ` (${item.by})` : ''} · {dateOf(item)} · {Math.round(item.confidence * 100)}%</span>
                  </div>
                  <p className="cx-kn-text">{item.text}</p>
                  {(item.aliases ?? []).length > 0 && <span className="ap faint cx-kn-meta">also: {(item.aliases ?? []).join(', ')}</span>}
                  {code.length > 0 && <span className="ap faint cx-kn-meta">code: {code.join(', ')}</span>}
                  {evidence.length > 0 && <span className="ap faint cx-kn-meta" title={evidence.join('\n')}>evidence: {evidence.length} · knowledge://{item.id}</span>}
                  {item.superseded_by && <span className="ap faint cx-kn-meta">superseded by knowledge://{item.superseded_by}</span>}
                  {item.reason && <span className="ap faint cx-kn-meta">withdrawn: {item.reason}</span>}
                  {live && (
                    <div className="cx-kn-acts">
                      {item.kind === 'hypothesis' && item.state !== 'supported' && (
                        <button type="button" className="detent tiny" disabled={busy !== null} data-action="supported" onClick={() => void run(item.id, 'memory.knowledge.upsert', { kind: item.kind, name: item.name, text: '', state: 'supported', by: 'user' }, 'Theory marked supported')}>Supported</button>
                      )}
                      {item.kind === 'hypothesis' && item.state !== 'refuted' && (
                        <button type="button" className="detent tiny" disabled={busy !== null} data-action="refuted" onClick={() => void run(item.id, 'memory.knowledge.upsert', { kind: item.kind, name: item.name, text: '', state: 'refuted', by: 'user' }, 'Theory marked refuted')}>Refuted</button>
                      )}
                      {item.kind === 'question' && item.state === 'open' && (
                        <button type="button" className="detent tiny" disabled={busy !== null} data-action="answered" onClick={() => void run(item.id, 'memory.knowledge.upsert', { kind: item.kind, name: item.name, text: '', state: 'answered', by: 'user' }, 'Question marked answered')}>Answered</button>
                      )}
                      <button type="button" className="detent tiny warn" disabled={busy !== null} data-action="withdraw" onClick={() => void run(item.id, 'memory.knowledge.withdraw', { id: item.id, reason: 'withdrawn in the Knowledge section', by: 'user' }, 'Item withdrawn')}>Withdraw</button>
                    </div>
                  )}
                </li>
              );
            })}
          </ul>
        )}
      </div>
    </section>
  );
}

export default definePlugin({ slot: 'card.knowledge@1', component: KnowledgeSection });
