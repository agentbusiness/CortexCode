/** Lossless terminal transport shared by the Desk and terminal panes (and by CortexCode). */
import type { Client } from './rpc';
import type { TermSnapshot } from './types';

interface Disposable { dispose(): void }
interface TerminalSurface {
  cols: number;
  rows: number;
  options: { disableStdin?: boolean };
  write(data: string | Uint8Array, callback?: () => void): void;
  resize(cols: number, rows: number): void;
  onData(listener: (data: string) => void): Disposable;
  onBinary(listener: (data: string) => void): Disposable;
  onResize(listener: (size: { cols: number; rows: number }) => void): Disposable;
}
interface Output { term_id?: string; data?: string; offset?: number; seq?: number }

function decode(data: string): Uint8Array {
  return Uint8Array.from(atob(data), (character) => character.charCodeAt(0));
}
function encode(bytes: Uint8Array): string {
  let binary = '';
  for (let i = 0; i < bytes.length; i += 8192) {
    binary += String.fromCharCode(...bytes.subarray(i, i + 8192));
  }
  return btoa(binary);
}

/** No app shortcuts are installed here: xterm owns the focused program's entire keymap. */
export function connectTerminal(client: Client, term: TerminalSurface, termId: string, options: {
  onReady?: () => void;
  onError?: (message: string) => void;
} = {}): { dispose(): void; refresh(): void } {
  let disposed = false;
  let restoring = true;
  let generation = 0;
  let offset: number | undefined;
  let queue: Output[] = [];
  let queuedBytes = 0;
  let request: AbortController | undefined;
  let input: Uint8Array[] = [];
  let writing = false;
  let lastSize = '';
  let resizing = false;
  let supportsBinary = false;
  const encoder = new TextEncoder();
  const report = (cause: unknown) => { if (!disposed) options.onError?.(cause instanceof Error ? cause.message : String(cause)); };
  term.options.disableStdin = true;

  const resize = () => {
    if (disposed || restoring || resizing || client.state !== 'open') return;
    const cols = term.cols;
    const rows = term.rows;
    const size = `${cols}x${rows}`;
    if (size === lastSize) return;
    resizing = true;
    lastSize = size;
    void client.call('terminal.resize', { term_id: termId, cols, rows }).then(() => {
      resizing = false;
      resize(); // A drag may have changed the dimensions while this request was in flight.
    }, (cause) => {
      resizing = false;
      lastSize = '';
      report(cause);
    });
  };

  // One request in flight, with subsequent keystrokes batched: no overlapping writes and no
  // one-round-trip-per-character backlog on a remote computer. Never retry accepted input.
  const flushInput = async () => {
    if (writing || disposed || !input.length) return;
    writing = true;
    try {
      while (!disposed && input.length && !restoring && client.state === 'open') {
        const parts = input;
        input = [];
        const bytes = new Uint8Array(parts.reduce((size, part) => size + part.length, 0));
        let at = 0;
        for (const part of parts) { bytes.set(part, at); at += part.length; }
        await client.call('terminal.write', {
          term_id: termId,
          data: supportsBinary ? encode(bytes) : new TextDecoder().decode(bytes),
          base64: supportsBinary,
        });
      }
    } catch (cause) {
      input = [];
      report(cause);
    } finally {
      writing = false;
      if (input.length && !restoring && !disposed) void flushInput();
    }
  };
  const send = (data: string, binary: boolean) => {
    if (disposed || restoring || client.state !== 'open') return;
    if (binary && !supportsBinary) {
      report('This terminal backend needs an update for binary mouse input.');
      return;
    }
    input.push(binary ? Uint8Array.from(data, (character) => character.charCodeAt(0)) : encoder.encode(data));
    void flushInput();
  };

  const enqueue = (event: Output) => {
    queue.push(event);
    queuedBytes += event.data?.length ?? 0;
    // The next snapshot includes discarded bytes. Bound memory while a renderer is attaching.
    if (queuedBytes > 8 * 1024 * 1024) {
      queue = [];
      queuedBytes = 0;
      void refresh();
    }
  };
  const apply = (event: Output) => {
    if (!event.data) return;
    const bytes = decode(event.data);
    if (offset !== undefined && event.offset !== undefined) {
      if (event.offset <= offset) return;
      const begin = event.offset - bytes.length;
      if (begin > offset) {
        enqueue(event);
        void refresh();
        return;
      }
      term.write(bytes.subarray(Math.max(0, offset - begin)));
      offset = event.offset;
    } else {
      term.write(bytes);
    }
  };
  const refresh = async () => {
    if (disposed) return;
    const current = ++generation;
    restoring = true;
    term.options.disableStdin = true;
    input = [];
    request?.abort();
    request = new AbortController();
    try {
      const snapshot = await client.call<TermSnapshot>('terminal.snapshot', { term_id: termId, restore: true }, { signal: request.signal });
      if (disposed || current !== generation) return;
      if (snapshot.cols && snapshot.rows) term.resize(snapshot.cols, snapshot.rows);
      offset = snapshot.restore?.offset;
      supportsBinary = Boolean(snapshot.input_base64);
      const data = snapshot.restore ? decode(snapshot.restore.data) :
        `\x1bc${snapshot.alternate_screen ? '\x1b[?1049h' : ''}${snapshot.text.replace(/\n/g, '\r\n')}\x1b[${snapshot.cursor_row + 1};${snapshot.cursor_col + 1}H`;
      await new Promise<void>((resolve) => term.write(data, resolve));
      if (disposed || current !== generation) return;
      // Bytes queued before or during the snapshot are cut against its exact byte boundary.
      restoring = false;
      const events = queue;
      queue = [];
      queuedBytes = 0;
      for (const event of events) {
        if (restoring) enqueue(event);
        else apply(event);
      }
      if (restoring) return;
      term.options.disableStdin = false;
      lastSize = '';
      options.onReady?.();
      resize();
    } catch (cause) {
      if (disposed || current !== generation) return;
      report(cause);
      // Do not send keys to a screen whose state we could not restore. Reconnect retries it.
    }
  };

  const offOutput = client.on('terminal.output', (_topic, data) => {
    const event = data as Output;
    if (event?.term_id !== termId || typeof event.data !== 'string') return;
    if (restoring) enqueue(event);
    else apply(event);
  });
  const offState = client.onState((state) => {
    if (state !== 'open') {
      restoring = true;
      term.options.disableStdin = true;
      input = [];
    }
  });
  const offReconnect = client.onReconnect(() => { void refresh(); });
  const textInput = term.onData((data) => send(data, false));
  const binaryInput = term.onBinary((data) => send(data, true));
  const sizeInput = term.onResize(resize);
  void refresh();
  return { refresh() { void refresh(); }, dispose() {
    disposed = true;
    generation++;
    request?.abort();
    offOutput(); offState(); offReconnect();
    textInput.dispose(); binaryInput.dispose(); sizeInput.dispose();
    input = []; queue = [];
  } };
}
