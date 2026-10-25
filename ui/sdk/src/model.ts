/**
 * The model's state as the person should see it, from model.info, the `model.state` event and
 * (while something waits on the model) model.slots: loading, waiting for a slot, ready. A slow
 * model must never look like a frozen workbench.
 */

import { useEffect, useState } from 'react';
import { useEvent, useQuery, type QueryResult } from './runtime';

/** The variant CortexCode is configured to run (the model plugin's default). Anything else is a
 * test instance and is labelled as one. (Contract request: model.variants says which is default.) */
export const DEFAULT_MODEL_VARIANT = 'qwen3.8-27b-m';

export interface ModelInfoData {
  model_id: string;
  variant: string;
  path: string;
  family: string;
  quant: string;
  file_bytes: number;
  ctx_total: number;
  ctx_native: number;
  slots: number;
  kv_type: string;
  speculative: string;
  backend: string;
  tensor_api?: boolean | null;
  engine_revision: string;
  extra?: { configured_variant?: string } | null;
}

export interface ModelSlotsData {
  slots: Array<{ slot: number; busy: boolean; n_tokens: number; cache_key?: string | null }>;
  queued: number;
  busy?: number;
  queued_by_priority?: { foreground: number; agent: number; background: number };
}

export type ModelPhase = 'unknown' | 'unavailable' | 'starting' | 'installing' | 'loading' | 'ready' | 'failed';

export interface ModelState {
  phase: ModelPhase;
  variant: string | null;
  /** The variant the model plugin is configured to run. */
  configuredVariant: string;
  /** Not the configured default variant: a test instance. */
  testVariant: boolean;
  /** Why it failed, as the model plugin says it. */
  reason: string | null;
  ctx: number | null;
  ctxNative: number | null;
  slots: number | null;
  /** Requests waiting for a slot (read only while `watchSlots`). */
  queued: number | null;
  busy: number | null;
  /** A model download or check under way (the model plugin's `model.install` events), else null. */
  install: InstallProgress | null;
  info: QueryResult<ModelInfoData>;
}

/** One `model.install` event: a variant's files being downloaded or checked. */
export interface InstallProgress {
  variant: string;
  /** download | verify | done | failed */
  phase: string;
  file: string;
  /** Bytes done of the whole variant. */
  bytes: number;
  total: number;
  detail?: string;
  /** Bytes per second over the last events of the same phase, when known. */
  rate: number | null;
  at: number;
}

function nextInstall(previous: InstallProgress | null, data: unknown): InstallProgress | null {
  const event = data as Partial<InstallProgress> | null;
  if (!event || typeof event.variant !== 'string' || typeof event.phase !== 'string') return previous;
  const at = Date.now();
  const bytes = typeof event.bytes === 'number' ? event.bytes : 0;
  let rate: number | null = null;
  if (previous && previous.variant === event.variant && previous.phase === event.phase && at > previous.at && bytes >= previous.bytes) {
    const now = (bytes - previous.bytes) / ((at - previous.at) / 1000);
    rate = previous.rate === null ? now : previous.rate * 0.7 + now * 0.3;
  }
  return { variant: event.variant, phase: event.phase, file: event.file ?? '', bytes, total: typeof event.total === 'number' ? event.total : 0, detail: event.detail, rate, at };
}

/** `Downloading qwen3.8-27b-m · 42 % · 6.1 of 14.7 GB · 38 MB/s`; null when nothing installs. */
export function installLine(install: InstallProgress | null): string | null {
  if (!install || (install.phase !== 'download' && install.phase !== 'verify')) return null;
  const gb = (value: number) => (value / 1e9).toFixed(1);
  const percent = install.total > 0 ? Math.floor((install.bytes / install.total) * 100) : 0;
  const what = install.phase === 'download' ? 'Downloading' : 'Checking';
  const rate = install.rate !== null && install.rate > 0 ? ` · ${(install.rate / 1e6).toFixed(0)} MB/s` : '';
  return `${what} ${install.variant} · ${percent} % · ${gb(install.bytes)} of ${gb(install.total)} GB${rate}`;
}

/**
 * The model's state. `watchSlots`: something waits on the model now (a run in this conversation),
 * so the queue is read every 2 s (the model plugin publishes no event for its slots).
 */
export function useModelState({ watchSlots = false }: { watchSlots?: boolean } = {}): ModelState {
  const info = useQuery<ModelInfoData>('model.info', (c) => c.call<ModelInfoData>('model.info', {}), { topics: ['model.state', 'plugin.swapped'] });
  const [event, setEvent] = useState<{ state: string; variant?: string; reason?: string; error?: string } | null>(null);
  useEvent('model.state', (_topic, data) => setEvent(data as { state: string }));
  const [install, setInstall] = useState<InstallProgress | null>(null);
  useEvent('model.install', (_topic, data) => setInstall((previous) => nextInstall(previous, data)));
  const slots = useQuery<ModelSlotsData>(watchSlots ? 'model.slots' : null, (c) => c.call<ModelSlotsData>('model.slots', {}));
  useEffect(() => {
    if (!watchSlots) return;
    const timer = setInterval(() => void slots.refresh(), 2000);
    return () => clearInterval(timer);
    // `refresh` is stable per key.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [watchSlots]);

  const data = info.data;
  let phase: ModelPhase = 'unknown';
  if (info.errorCode === 'unavailable') phase = 'unavailable';
  else if (event && ['starting', 'installing', 'loading', 'failed'].includes(event.state)) phase = event.state as ModelPhase;
  else if (!data && install && (install.phase === 'download' || install.phase === 'verify')) phase = 'installing';
  else if (data) phase = 'ready';
  const variant = data?.variant ?? event?.variant ?? null;
  const configured: string = data?.extra?.configured_variant ?? DEFAULT_MODEL_VARIANT;
  return {
    phase,
    variant,
    configuredVariant: configured,
    // A test instance: the loaded variant differs from the one the model plugin is configured to run.
    testVariant: variant !== null && variant !== configured,
    reason: (phase === 'failed' ? event?.error : null) ?? null,
    ctx: data?.ctx_total ?? null,
    ctxNative: data?.ctx_native ?? null,
    slots: data?.slots ?? null,
    queued: slots.data?.queued ?? null,
    busy: slots.data ? slots.data.busy ?? slots.data.slots.filter((slot) => slot.busy).length : null,
    install: install && (install.phase === 'download' || install.phase === 'verify') ? install : null,
    info,
  };
}

/** `1 slot`, `4 slots`. */
export function plural(count: number, one: string, many = `${one}s`): string {
  return `${count.toLocaleString()} ${count === 1 ? one : many}`;
}
