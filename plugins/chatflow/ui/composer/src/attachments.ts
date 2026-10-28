/**
 * Message attachments in the composer: what the chat flow's `chatflow.attachment.put` makes of a
 * pasted screenshot, a dropped or picked file, or a folder, and the checks made before anything is
 * sent. Limits mirror `chatflow@1`'s (`protocol/src/contracts/chatflow.rs`, `limits`).
 */

/** Largest file put into the attachment store (pasted, dropped, or copied from outside the project). */
export const MAX_ATTACHMENT_BYTES = 20 * 1024 * 1024;
/** Attachments one message may carry. */
export const MAX_ATTACHMENTS = 20;
/** Largest preview kept with an image for the transcript (a small data: URL). */
export const MAX_THUMB_BYTES = 32 * 1024;
/**
 * Pasted text longer than this (characters or lines) becomes a text-file attachment instead of
 * message text (`chatflow@1` `limits::PASTE_AS_FILE_*`); the workbench setting
 * `composer.pasteAsFile` overrides it.
 */
export interface PasteAsFile {
  chars: number;
  lines: number;
}
export const PASTE_AS_FILE: PasteAsFile = { chars: 8000, lines: 200 };

/** Lines of a text (a trailing newline does not start another). */
export function lineCount(text: string): number {
  if (!text) return 0;
  let count = 1;
  for (let i = 0; i < text.length; i++) if (text.charCodeAt(i) === 10) count++;
  return text.endsWith('\n') ? count - 1 : count;
}

/** Whether pasted text is long enough to go as a file. */
export function isLongPaste(text: string, limits: PasteAsFile = PASTE_AS_FILE): boolean {
  return text.length > limits.chars || lineCount(text) > limits.lines;
}

/** The preview's longest side, in pixels. */
const THUMB_SIDE = 160;

/** A message attachment as `chatflow.turn` / `chatflow.steer` / `agents.steer` take it. */
export interface Attachment {
  kind: 'image' | 'file' | 'folder';
  path: string;
  name: string;
  mime: string;
  bytes: number;
  sha256?: string;
  stored?: boolean;
  width?: number;
  height?: number;
  thumb?: string;
  /** Long pasted text the composer made a file (the model gets its path and a head/tail preview). */
  pasted_text?: boolean;
  lines?: number;
}

/** `chatflow.attachment.put`'s answer (`{sha256, path, mime, size, width?, height?}` and more). */
export interface PutResult {
  sha256?: string | null;
  path: string;
  mime: string;
  size: number;
  width?: number | null;
  height?: number | null;
  kind: string;
  name: string;
  stored: boolean;
  thumb?: string | null;
}

export interface PutParams {
  path?: string;
  data?: string;
  name?: string;
  mime?: string;
  project_id?: string | null;
  session_id?: string | null;
  thumb?: string;
}

/** What the Electron shell offers for attachments (each member absent in older shells). */
export interface AttachShell {
  pickFolder(title?: string): Promise<string | null>;
  pathForFile?(file: File): string;
  saveAttachment?(bytes: Uint8Array, name: string): Promise<{ path?: string; error?: string }>;
}

export function toAttachment(put: PutResult): Attachment {
  const kind = put.kind === 'image' || put.kind === 'folder' ? put.kind : 'file';
  const attachment: Attachment = { kind, path: put.path, name: put.name, mime: put.mime, bytes: put.size, stored: put.stored };
  if (put.sha256) attachment.sha256 = put.sha256;
  if (typeof put.width === 'number') attachment.width = put.width;
  if (typeof put.height === 'number') attachment.height = put.height;
  if (put.thumb) attachment.thumb = put.thumb;
  return attachment;
}

/** Human-readable size, the way the chat flow writes it (B, KB, MB, GB with one decimal). */
export function humanSize(bytes: number): string {
  const units = ['B', 'KB', 'MB', 'GB'];
  let value = bytes;
  let unit = 0;
  while (value >= 1024 && unit + 1 < units.length) {
    value /= 1024;
    unit++;
  }
  return unit === 0 ? `${bytes} B` : `${value.toFixed(1)} ${units[unit]}`;
}

export function tooBig(name: string, bytes: number, what = 'attachments'): string {
  return `${name} is ${humanSize(bytes)}; ${what} may be at most ${humanSize(MAX_ATTACHMENT_BYTES)}.`;
}

/** Whether `path` is inside one of `roots` (plain path prefixes; the chat flow decides for real). */
export function insideAny(path: string, roots: readonly string[]): boolean {
  return roots.some((root) => root && (path === root || path.startsWith(root.endsWith('/') ? root : `${root}/`)));
}

/** A name for bytes that never had one (a pasted screenshot is usually just `image.png`). */
export function pastedName(file: File, now: Date): string {
  const extension = (file.type.split('/')[1] ?? 'png').replace(/[^a-z0-9]/gi, '').replace(/^jpeg$/, 'jpg') || 'png';
  const pad = (n: number) => String(n).padStart(2, '0');
  const stamp = `${now.getFullYear()}-${pad(now.getMonth() + 1)}-${pad(now.getDate())} ${pad(now.getHours())}.${pad(now.getMinutes())}.${pad(now.getSeconds())}`;
  return `Pasted image ${stamp}.${extension}`;
}

/** Base64 of bytes (chunked: `String.fromCharCode` takes a bounded argument list). */
export function toBase64(bytes: Uint8Array): string {
  let binary = '';
  for (let i = 0; i < bytes.length; i += 0x8000) binary += String.fromCharCode(...bytes.subarray(i, i + 0x8000));
  return btoa(binary);
}

/**
 * A small preview of an image for the transcript, or undefined where the browser cannot decode it
 * (no `createImageBitmap`, or an undecodable file). Never the image itself.
 */
export async function makeThumb(file: Blob): Promise<string | undefined> {
  if (typeof createImageBitmap !== 'function' || typeof document === 'undefined') return undefined;
  try {
    const bitmap = await createImageBitmap(file);
    const scale = Math.min(1, THUMB_SIDE / Math.max(bitmap.width, bitmap.height));
    const canvas = document.createElement('canvas');
    canvas.width = Math.max(1, Math.round(bitmap.width * scale));
    canvas.height = Math.max(1, Math.round(bitmap.height * scale));
    const context = canvas.getContext('2d');
    if (!context) return undefined;
    context.drawImage(bitmap, 0, 0, canvas.width, canvas.height);
    bitmap.close?.();
    for (const quality of [0.8, 0.6, 0.4]) {
      const url = canvas.toDataURL('image/webp', quality);
      if (url.startsWith('data:image/') && url.length <= MAX_THUMB_BYTES) return url;
    }
    return undefined;
  } catch {
    return undefined;
  }
}

/** One thing to attach, as the composer received it. */
export type Incoming =
  | { type: 'file'; file: File; pasted: boolean }
  | { type: 'text'; text: string; name: string }
  | { type: 'folder'; name: string; path: string }
  /** A local path whose kind the chat flow decides (a file or folder copied in Finder and pasted). */
  | { type: 'path'; name: string; path: string };

export interface AttachContext {
  put: (params: PutParams) => Promise<PutResult>;
  shell: AttachShell | null;
  /** The project's root and the session's workspace: their files are referenced, not copied. */
  roots: readonly string[];
  projectId: string | null;
  sessionId: string | null;
  now?: () => Date;
}

/**
 * Make one attachment: a folder or a file with a local path is put by its path (a project path is
 * referenced, anything else copied into the store); bytes without a path are written by the shell
 * into the store's intake and put from there, or, without a shell, sent as base64. Rejects with a
 * message for the person when a limit says no.
 */
export async function attachOne(item: Incoming, context: AttachContext): Promise<Attachment> {
  const scope = { project_id: context.projectId, session_id: context.sessionId };
  if (item.type === 'text') {
    const bytes = new TextEncoder().encode(item.text);
    if (bytes.length > MAX_ATTACHMENT_BYTES) throw new Error(tooBig('The pasted text', bytes.length));
    const extra = { name: item.name, mime: 'text/plain', ...scope };
    let put: PutResult;
    if (context.shell?.saveAttachment) {
      const saved = await context.shell.saveAttachment(bytes, item.name);
      if (saved.error || !saved.path) throw new Error(`The pasted text could not be saved: ${saved.error ?? 'no path'}`);
      put = await context.put({ path: saved.path, ...extra });
    } else {
      put = await context.put({ data: toBase64(bytes), ...extra });
    }
    return { ...toAttachment(put), kind: 'file', pasted_text: true, lines: lineCount(item.text) };
  }
  if (item.type === 'folder' || item.type === 'path') {
    if (!item.path) throw new Error(`${item.name}: a folder can be attached only where its path is known (in the CortexCode app).`);
    return toAttachment(await context.put({ path: item.path, name: item.name, ...scope }));
  }
  const { file } = item;
  // A file copied in Finder and pasted still has its path and name; a screenshot has neither.
  const path = context.shell?.pathForFile?.(file) ?? '';
  const generic = !file.name || /^image\.[a-z0-9]+$/i.test(file.name);
  const name = item.pasted && !path && generic ? pastedName(file, context.now?.() ?? new Date()) : file.name || 'attachment';
  const copied = !path || !insideAny(path, context.roots);
  if (copied && file.size > MAX_ATTACHMENT_BYTES) {
    throw new Error(path ? tooBig(name, file.size, 'files from outside the project (they are copied for the model)') : tooBig(name, file.size));
  }
  const thumb = file.type.startsWith('image/') ? await makeThumb(file) : undefined;
  const mime = file.type || undefined;
  const extra = { name, ...(mime ? { mime } : {}), ...(thumb ? { thumb } : {}), ...scope };
  if (path) return toAttachment(await context.put({ path, ...extra }));
  const bytes = new Uint8Array(await file.arrayBuffer());
  if (context.shell?.saveAttachment) {
    const saved = await context.shell.saveAttachment(bytes, name);
    if (saved.error || !saved.path) throw new Error(`${name} could not be saved: ${saved.error ?? 'no path'}`);
    return toAttachment(await context.put({ path: saved.path, ...extra }));
  }
  return toAttachment(await context.put({ data: toBase64(bytes), ...extra }));
}

/** What a drop carries: files, and folders (by their entries) with their local paths. */
export function fromDataTransfer(transfer: DataTransfer, shell: AttachShell | null): Incoming[] {
  const out: Incoming[] = [];
  const items = transfer.items ? Array.from(transfer.items) : [];
  if (items.length > 0) {
    for (const item of items) {
      if (item.kind !== 'file') continue;
      const file = item.getAsFile();
      const entry = (item as DataTransferItem & { webkitGetAsEntry?: () => { isDirectory: boolean; name: string } | null }).webkitGetAsEntry?.() ?? null;
      if (entry?.isDirectory) {
        out.push({ type: 'folder', name: entry.name || file?.name || 'folder', path: file ? shell?.pathForFile?.(file) ?? '' : '' });
      } else if (file) {
        out.push({ type: 'file', file, pasted: false });
      }
    }
    return out;
  }
  for (const file of Array.from(transfer.files ?? [])) out.push({ type: 'file', file, pasted: false });
  return out;
}

/** Local paths a paste carries as `file://` URLs (Finder's copy of files and folders), else none. */
export function pastedPaths(transfer: DataTransfer): Incoming[] {
  const list = transfer.getData('text/uri-list') || '';
  const out: Incoming[] = [];
  for (const line of list.split(/\r?\n/)) {
    const url = line.trim();
    if (!url.startsWith('file://') || !URL.canParse(url)) continue;
    const path = decodeURIComponent(new URL(url).pathname);
    if (path) out.push({ type: 'path', name: baseName(path), path });
  }
  return out;
}

/** The last path segment of a path (a folder picked in the native dialog). */
export function baseName(path: string): string {
  return path.replace(/\/+$/, '').split('/').pop() || path;
}
