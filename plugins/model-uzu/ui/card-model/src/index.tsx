/**
 * card.model@1 — the model's live state, one line: the variant, slots busy of all, the queue when
 * something waits, and the decode speed: live while a request writes (`model.progress`), else of
 * the last model call in the conversation in view (quant and speculative decoding in the tooltip).
 * How full the KV pool is sits in the card's foot. `model.slots` is read again whenever a request
 * is queued, starts or ends (the model plugin's `model.progress` events), so busy and queued follow
 * the requests as they come and go.
 */

import { useState } from 'react';
import { definePlugin, useEvent, useJournal, useModelState, useQuery, useSelection, ServiceProblem } from '@cortex/sdk';
import './model.css';

/** One `model.progress` event, as far as the card reads it. */
export interface Progress { request: string; phase: string; prefill_done?: number; tok_s?: number; work?: string }

/** Whether an event changes which requests hold a slot or wait (queued, started, ended). */
export function changesSlots(data: unknown): boolean {
  const event = data as Progress | null;
  if (!event || typeof event !== 'object') return false;
  return event.phase === 'queued' || event.phase === 'done' || (event.phase === 'prefill' && (event.prefill_done ?? 0) === 0);
}

/** Requests writing now, by id (each with its latest rate). */
export function writing(current: ReadonlyMap<string, number>, data: unknown): ReadonlyMap<string, number> {
  const event = data as Progress | null;
  if (!event || typeof event !== 'object' || typeof event.request !== 'string') return current;
  if (event.phase === 'decode') return new Map(current).set(event.request, event.tok_s ?? 0);
  if (!current.has(event.request)) return current;
  const next = new Map(current);
  next.delete(event.request);
  return next;
}

interface Slots { slots: Array<{ slot: number; busy: boolean; n_tokens: number; cache_key?: string | null }>; queued: number; hibernated?: string[] }

function ModelSection() {
  const state = useModelState();
  const info = state.info;
  const slots = useQuery<Slots>('model.slots', (c) => c.call<Slots>('model.slots', {}), { topics: ['model.progress'], when: (_topic, data) => changesSlots(data) });
  const [rates, setRates] = useState<ReadonlyMap<string, number>>(new Map());
  useEvent('model.progress', (_topic, data) => setRates((current) => writing(current, data)));
  const live = rates.size > 0 ? [...rates.values()].reduce((sum, rate) => sum + rate, 0) : null;
  const { sessionId } = useSelection();
  const journal = useJournal(sessionId);
  const usage = [...journal.entries].reverse().find((entry) => entry.kind === 'usage' && typeof entry.data.decode_ms === 'number' && (entry.data.decode_ms as number) > 0);
  const speed = usage ? (usage.data.completion_tokens as number) / ((usage.data.decode_ms as number) / 1000) : null;
  const list = slots.data?.slots ?? [];
  const busy = list.filter((slot) => slot.busy).length;
  const loading = state.phase === 'loading' || state.phase === 'starting';
  const detail = info.data ? [`${info.data.variant} · ${info.data.quant}`, info.data.speculative ? `speculative: ${info.data.speculative}` : '', speed !== null ? 'speed of the last model call in this conversation' : ''].filter(Boolean).join('\n') : undefined;
  return (
    <section className="cx-cs" data-section="model" data-build={__CORTEX_BUILD__}>
      <div className="cx-cs-l"><span className="ap">Model</span></div>
      <div className="cx-cs-b">
        {info.error && !info.data && !loading && <ServiceProblem service="model" state={info} className="hint" />}
        <div className="cx-cs-row" title={detail}>
          {(info.data || state.variant) && <span className="ap cx-model-name">{info.data?.variant ?? state.variant}</span>}
          {state.testVariant && <span className="ap live" data-test-variant={state.variant ?? ''} title="Not the configured model: a test instance">test</span>}
          <span className="grow" />
          {loading && <span className="ap hot" data-model-phase={state.phase}>loading</span>}
          {info.data && slots.data && <span className="ap faint" data-busy={busy}><span className="num cx-model-num">{busy}/{list.length || info.data.slots}</span> busy</span>}
          {slots.data && slots.data.queued > 0 && <span className="ap hot"><span className="num">{slots.data.queued}</span> queued</span>}
          {live !== null
            ? <span className="ap live" data-writing={rates.size}><span className="num cx-model-num">{live.toFixed(1)}</span> tok/s</span>
            : speed !== null && <span className="ap faint"><span className="num cx-model-num">{speed.toFixed(1)}</span> tok/s</span>}
        </div>
      </div>
    </section>
  );
}

export default definePlugin({ slot: 'card.model@1', component: ModelSection });
