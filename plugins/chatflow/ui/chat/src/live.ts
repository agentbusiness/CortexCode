/**
 * What the model is doing for this conversation right now, from two event streams:
 * `model.progress` (the model plugin: each request queued, reading its prompt, writing, done; at
 * most four a second) keyed by `cache_key` (the session id), and `chatflow.live` (the chat flow: a
 * decision's free text — ROUTE's `work`, DECIDE's `missing`, `brief`, `note` — while its JSON
 * streams). Pure reducers plus one hook; nothing is polled.
 */

import { useEffect, useState } from 'react';
import { useEvent } from '@cortex/sdk';

export interface Progress {
  request: string;
  cache_key: string;
  priority: string;
  /** generate | prewarm */
  work: string;
  /** queued | prefill | decode | done */
  phase: string;
  prompt_tokens: number;
  cached_tokens: number;
  prefill_done: number;
  prefill_total: number;
  generated: number;
  tok_s: number;
  ahead: number;
  finish?: string;
}

export interface LiveFields {
  session_id: string;
  run_id: string;
  stage: string;
  fields: Array<[string, string]>;
  done: boolean;
}

export interface LiveRun {
  /** The newest request of the conversation that has not ended (a generation before a prefill). */
  progress: Progress | null;
  /** The newest decision text of the conversation. */
  decision: LiveFields | null;
}

export const NO_LIVE: LiveRun = { progress: null, decision: null };

/** Requests in flight per conversation, by request id. */
export type Requests = ReadonlyMap<string, Progress>;

/** The requests in flight after one `model.progress` event of `sessionId` (others are ignored). */
export function trackProgress(requests: Requests, sessionId: string, event: unknown): Requests {
  const progress = event as Progress | null;
  if (!progress || typeof progress !== 'object' || progress.cache_key !== sessionId || typeof progress.request !== 'string') return requests;
  const next = new Map(requests);
  if (progress.phase === 'done') next.delete(progress.request);
  else next.set(progress.request, progress);
  return next;
}

/** The request to show: the newest generation in flight, else the newest prefill (a prewarm). */
export function current(requests: Requests): Progress | null {
  const all = [...requests.values()];
  return all.filter((p) => p.work !== 'prewarm').at(-1) ?? all.at(-1) ?? null;
}

const n = (value: number) => Math.max(0, Math.round(value)).toLocaleString('en-US');

/** One line of what the request is doing, in the person's terms. */
export function progressLine(progress: Progress | null): string {
  if (!progress) return '';
  const prewarm = progress.work === 'prewarm';
  switch (progress.phase) {
    case 'queued':
      return progress.ahead > 0 ? `Waiting for the model · ${n(progress.ahead)} ahead` : 'Waiting for the model';
    case 'prefill':
      return `${prewarm ? 'Preparing the conversation' : 'Reading the conversation'} · ${n(progress.prefill_done)} / ${n(progress.prefill_total)} tokens`;
    case 'decode':
      return `Writing · ${n(progress.generated)} token${progress.generated === 1 ? '' : 's'}${progress.tok_s > 0 ? ` · ${n(progress.tok_s)} tok/s` : ''}`;
    default:
      return '';
  }
}

/** The decision text to show: its non-empty fields, in the order written. */
export function decisionLines(decision: LiveFields | null): string[] {
  if (!decision) return [];
  return decision.fields.map(([, text]) => text.trim()).filter(Boolean);
}

/**
 * The conversation's live run: its model requests and its newest decision text. Cleared when the
 * conversation changes; a decision's text is kept until the next one replaces it, and dropped once
 * the run ends (`running` false).
 */
export function useLiveRun(sessionId: string | null, running: boolean): LiveRun {
  const [requests, setRequests] = useState<Requests>(new Map());
  const [decision, setDecision] = useState<LiveFields | null>(null);
  useEffect(() => {
    setRequests(new Map());
    setDecision(null);
  }, [sessionId]);
  useEffect(() => {
    // A finished run leaves nothing in flight (an event lost in a restart must not linger).
    if (!running) {
      setDecision(null);
      setRequests(new Map());
    }
  }, [running]);
  useEvent(sessionId ? 'model.progress' : null, (_topic, data) => {
    if (sessionId) setRequests((current) => trackProgress(current, sessionId, data));
  });
  useEvent(sessionId ? 'chatflow.live' : null, (_topic, data) => {
    const live = data as LiveFields | null;
    if (sessionId && live && live.session_id === sessionId && Array.isArray(live.fields)) setDecision(live);
  });
  return { progress: current(requests), decision: running ? decision : null };
}
