/** Small formatting helpers shared by the plugins. */

export function errorMessage(error: unknown): string {
  if (error instanceof Error) return error.message;
  if (typeof error === 'string') return error;
  try {
    return JSON.stringify(error);
  } catch {
    return String(error);
  }
}

/** `840ms`, `3.2s`, `2m 05s`. */
export function duration(ms: number): string {
  if (!Number.isFinite(ms) || ms < 0) return '';
  if (ms < 1000) return `${Math.round(ms)}ms`;
  if (ms < 60_000) return `${(ms / 1000).toFixed(1)}s`;
  const minutes = Math.floor(ms / 60_000);
  const seconds = Math.floor((ms % 60_000) / 1000);
  return `${minutes}m ${String(seconds).padStart(2, '0')}s`;
}

/** Clock time for today, date and time otherwise. */
export function clock(ms: number): string {
  const date = new Date(ms);
  const now = new Date();
  const time = date.toLocaleTimeString([], { hour: '2-digit', minute: '2-digit' });
  if (date.toDateString() === now.toDateString()) return time;
  return `${date.toLocaleDateString([], { month: 'short', day: 'numeric' })} ${time}`;
}

/** `just now`, `4 min ago`, `3 h ago`, else the date. */
export function ago(ms: number, now = Date.now()): string {
  const delta = Math.max(0, now - ms);
  if (delta < 45_000) return 'just now';
  if (delta < 3_600_000) return `${Math.round(delta / 60_000)} min ago`;
  if (delta < 86_400_000) return `${Math.round(delta / 3_600_000)} h ago`;
  return new Date(ms).toLocaleDateString([], { month: 'short', day: 'numeric' });
}

export function bytes(size: number): string {
  if (size < 1024) return `${size} B`;
  if (size < 1024 * 1024) return `${(size / 1024).toFixed(1)} KB`;
  if (size < 1024 ** 3) return `${(size / (1024 * 1024)).toFixed(1)} MB`;
  return `${(size / 1024 ** 3).toFixed(1)} GB`;
}

/** Last path component. */
export function basename(path: string): string {
  const clean = path.replace(/\/+$/, '');
  const index = clean.lastIndexOf('/');
  return index >= 0 ? clean.slice(index + 1) : clean;
}

/** Everything before the last path component. */
export function dirname(path: string): string {
  const clean = path.replace(/\/+$/, '');
  const index = clean.lastIndexOf('/');
  return index > 0 ? clean.slice(0, index) : '';
}

/** Join a workspace root and a relative path. */
export function joinPath(root: string, relative: string): string {
  if (relative.startsWith('/')) return relative;
  return `${root.replace(/\/+$/, '')}/${relative.replace(/^\.\//, '')}`;
}

/** A path relative to `root` when it is inside it. */
export function relativePath(root: string, path: string): string {
  const base = root.replace(/\/+$/, '') + '/';
  return path.startsWith(base) ? path.slice(base.length) : path;
}

/**
 * What to say when a service's call failed: that the service is not running (the frame answered
 * `unavailable`), with why its last start failed when the frame says, or what it said.
 * `ServiceProblem` (parts) adds the way to the plugins panel.
 */
export function serviceProblem(service: string, state: { error?: string | undefined; errorCode?: string | undefined }): string {
  if (state.errorCode === 'unavailable') {
    // The frame's reply carries the slot's state: `… (restarting in 30000 ms; last attempt failed: <why>)`.
    const why = state.error?.match(/\((?:restarting in \d+ ms; last attempt )?failed: ([\s\S]*)\)$/)?.[1];
    return why ? `No ${service} service is running; its last start failed: ${why}` : `No ${service} service is running.`;
  }
  return `The ${service} service failed: ${state.error ?? 'no reason given'}`;
}
