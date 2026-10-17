/**
 * Journal materialization: a session's state is the fold of its patches (`JournalPatch`).
 *
 * A view subscribes to `journal.<session_id>` before it reads the journal, so nothing written in
 * between is lost. Patches that arrive while the read is in flight are held back: a `create` newer
 * than the read is applied, while `append`/`set` on an entry the read already returned are dropped,
 * because the read may already contain them and an `append` applied twice would duplicate text.
 * Whatever such a drop could lose is repaired when the entry finishes streaming (`streaming` set to
 * false): the view re-reads that entry and takes the frame's final copy.
 */

import type { Client } from './rpc';
import type { Json, JournalEntry, JournalPatch } from './types';

/** Apply one patch to an entry list ordered by seq. Returns a new list, or the same list if unchanged. */
export function applyPatch(entries: readonly JournalEntry[], patch: JournalPatch): readonly JournalEntry[] {
  switch (patch.op) {
    case 'create': {
      const { entry } = patch;
      const index = findIndex(entries, entry.seq);
      if (index >= 0) {
        const next = entries.slice();
        next[index] = entry;
        return next;
      }
      // Patches arrive in order, so a new entry nearly always goes at the end.
      const next = entries.slice();
      let at = next.length;
      while (at > 0 && next[at - 1]!.seq > entry.seq) at--;
      next.splice(at, 0, entry);
      return next;
    }
    case 'append': {
      const index = findIndex(entries, patch.seq);
      if (index < 0) return entries;
      const entry = entries[index]!;
      const current = entry.data[patch.field];
      const text = (typeof current === 'string' ? current : '') + patch.text;
      return replaceAt(entries, index, { ...entry, data: { ...entry.data, [patch.field]: text } });
    }
    case 'set': {
      const index = findIndex(entries, patch.seq);
      if (index < 0) return entries;
      const entry = entries[index]!;
      return replaceAt(entries, index, { ...entry, data: { ...entry.data, [patch.field]: patch.value } });
    }
    default:
      return entries;
  }
}

function replaceAt(entries: readonly JournalEntry[], index: number, entry: JournalEntry): readonly JournalEntry[] {
  const next = entries.slice();
  next[index] = entry;
  return next;
}

/** Binary search by seq (entries are kept sorted). */
export function findIndex(entries: readonly JournalEntry[], seq: number): number {
  let lo = 0;
  let hi = entries.length - 1;
  while (lo <= hi) {
    const mid = (lo + hi) >> 1;
    const value = entries[mid]!.seq;
    if (value === seq) return mid;
    if (value < seq) lo = mid + 1;
    else hi = mid - 1;
  }
  return -1;
}

/** The patch's target seq. */
function patchSeq(patch: JournalPatch): number {
  return patch.op === 'create' ? patch.entry.seq : patch.seq;
}

/**
 * Merge the patches that arrived while a read was in flight into the read's result.
 * Mechanical rule (see the module comment): creates newer than the read are applied; edits to
 * entries the read returned are dropped; edits to entries created after the read are applied.
 */
export function mergeBuffered(read: readonly JournalEntry[], buffered: readonly JournalPatch[]): readonly JournalEntry[] {
  const readMax = read.length ? read[read.length - 1]!.seq : 0;
  let entries = read;
  for (const patch of buffered) {
    if (patchSeq(patch) <= readMax) continue;
    entries = applyPatch(entries, patch);
  }
  return entries;
}

export type JournalStatus = 'loading' | 'ready' | 'error';

export interface JournalSnapshot {
  status: JournalStatus;
  entries: readonly JournalEntry[];
  error?: string;
}

type Listener = () => void;

/** A live, materialized journal of one session. Shared by every component that shows it. */
export class JournalView {
  readonly sessionId: string;
  private readonly client: Client;
  private snapshot: JournalSnapshot = { status: 'loading', entries: [] };
  private readonly listeners = new Set<Listener>();
  private buffer: JournalPatch[] | null = null;
  private unsubscribe: (() => void) | null = null;
  private stopReconnect: (() => void) | null = null;
  private notifyTimer: ReturnType<typeof setTimeout> | null = null;
  private readGeneration = 0;
  refs = 0;

  constructor(client: Client, sessionId: string) {
    this.client = client;
    this.sessionId = sessionId;
  }

  start(): void {
    this.unsubscribe = this.client.on(`journal.${this.sessionId}`, (topic, data) => {
      // `journal.<id>` is also a prefix of `journal.<id>x`; only this session's topic counts.
      if (topic !== `journal.${this.sessionId}`) return;
      this.receive(data as JournalPatch);
    });
    this.stopReconnect = this.client.onReconnect(() => void this.load());
    void this.load();
  }

  stop(): void {
    this.unsubscribe?.();
    this.stopReconnect?.();
    if (this.notifyTimer) clearTimeout(this.notifyTimer);
    this.listeners.clear();
  }

  get(): JournalSnapshot {
    return this.snapshot;
  }

  subscribe(listener: Listener): () => void {
    this.listeners.add(listener);
    return () => this.listeners.delete(listener);
  }

  /** Read the whole journal again (after a reconnect, or on request). */
  async load(): Promise<void> {
    const generation = ++this.readGeneration;
    this.buffer = [];
    try {
      const result = await this.client.call<{ entries: JournalEntry[] }>('frame.journal.read', {
        session_id: this.sessionId,
      });
      if (generation !== this.readGeneration) return;
      const read = [...result.entries].sort((a, b) => a.seq - b.seq);
      const merged = mergeBuffered(read, this.buffer ?? []);
      this.buffer = null;
      this.set({ status: 'ready', entries: merged }, true);
    } catch (error) {
      if (generation !== this.readGeneration) return;
      const buffered = this.buffer ?? [];
      this.buffer = null;
      let entries = this.snapshot.entries;
      for (const patch of buffered) entries = applyPatch(entries, patch);
      this.set({ status: 'error', entries, error: error instanceof Error ? error.message : String(error) }, true);
    }
  }

  private receive(patch: JournalPatch): void {
    if (!patch || typeof patch !== 'object' || !('op' in patch)) return;
    if (this.buffer) {
      this.buffer.push(patch);
      return;
    }
    const entries = applyPatch(this.snapshot.entries, patch);
    if (entries !== this.snapshot.entries) this.set({ ...this.snapshot, entries }, false);
    if (patch.op === 'set' && patch.field === 'streaming' && patch.value === false) {
      void this.refreshEntry(patch.seq);
    }
  }

  /** Take the frame's copy of a finished entry (repairs an edit dropped during a read). */
  private async refreshEntry(seq: number): Promise<void> {
    try {
      const result = await this.client.call<{ entries: JournalEntry[] }>('frame.journal.read', {
        session_id: this.sessionId,
        from: seq,
      });
      const fresh = result.entries.find((entry) => entry.seq === seq);
      if (!fresh || fresh.data.streaming === true) return;
      const index = findIndex(this.snapshot.entries, seq);
      if (index < 0) return;
      const current = this.snapshot.entries[index]!;
      if (sameData(current.data, fresh.data)) return;
      this.set({ ...this.snapshot, entries: replaceAt(this.snapshot.entries, index, fresh) }, false);
    } catch {
      // The live copy stays; the next full read repairs it.
    }
  }

  private set(snapshot: JournalSnapshot, immediate: boolean): void {
    this.snapshot = snapshot;
    if (immediate) {
      if (this.notifyTimer) clearTimeout(this.notifyTimer);
      this.notifyTimer = null;
      this.emit();
      return;
    }
    // Streamed tokens arrive far faster than a screen refreshes: coalesce them.
    if (this.notifyTimer) return;
    this.notifyTimer = setTimeout(() => {
      this.notifyTimer = null;
      this.emit();
    }, 16);
  }

  private emit(): void {
    for (const listener of [...this.listeners]) listener();
  }
}

function sameData(a: { [key: string]: Json }, b: { [key: string]: Json }): boolean {
  return JSON.stringify(a) === JSON.stringify(b);
}

/** Views are shared and reference counted per session, so swapping a plugin keeps its journal warm. */
export class JournalRegistry {
  private readonly views = new Map<string, JournalView>();
  private readonly releaseTimers = new Map<string, ReturnType<typeof setTimeout>>();
  private readonly client: Client;

  constructor(client: Client) {
    this.client = client;
  }

  acquire(sessionId: string): JournalView {
    const timer = this.releaseTimers.get(sessionId);
    if (timer) {
      clearTimeout(timer);
      this.releaseTimers.delete(sessionId);
    }
    let view = this.views.get(sessionId);
    if (!view) {
      view = new JournalView(this.client, sessionId);
      this.views.set(sessionId, view);
      view.start();
    }
    view.refs++;
    return view;
  }

  /** Peek without starting a view (used while a plugin pre-renders hidden). */
  peek(sessionId: string): JournalView | undefined {
    return this.views.get(sessionId);
  }

  release(view: JournalView): void {
    view.refs--;
    if (view.refs > 0) return;
    // Kept briefly: a hot-swapped plugin releases and re-acquires the same session within a frame.
    const timer = setTimeout(() => {
      this.releaseTimers.delete(view.sessionId);
      if (view.refs > 0) return;
      view.stop();
      this.views.delete(view.sessionId);
    }, 30_000);
    this.releaseTimers.set(view.sessionId, timer);
  }
}
