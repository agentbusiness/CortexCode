/**
 * Hashline anchors (`protocol/src/hashline.rs`, the single definition): a line rendered as
 * `N:hh|text`, where `hh` is a two-character tag of the line's content. Ported here only to
 * recognise anchored text structurally — every line must carry the right number and the right
 * tag — so a file shown with anchors can be turned back into its exact content.
 */

const ALPHABET = '0123456789abcdefghjkmnpqrstvwxyz';
const encoder = new TextEncoder();

/** FNV-1a 32-bit over the UTF-8 bytes. */
function fnv1a(bytes: Uint8Array): number {
  let hash = 0x811c9dc5;
  for (const byte of bytes) {
    hash ^= byte;
    hash = Math.imul(hash, 0x01000193) >>> 0;
  }
  return hash >>> 0;
}

/** The two-character tag of one line (content without `\n` / `\r\n`). */
export function tag(line: string): string {
  const content = line.endsWith('\r') ? line.slice(0, -1) : line;
  const hash = fnv1a(encoder.encode(content));
  const folded = (hash ^ (hash >>> 10) ^ (hash >>> 20) ^ (hash >>> 30)) >>> 0;
  return ALPHABET[folded & 31]! + ALPHABET[(folded >>> 5) & 31]!;
}

const ANCHORED = /^(\d+):([0-9a-z]{2})\|(.*)$/s;

/**
 * The content lines of anchored text, or null when the text is not entirely anchored lines
 * numbered consecutively from 1 with matching tags.
 */
export function parseAnchored(text: string): string[] | null {
  const body = text.endsWith('\n') ? text.slice(0, -1) : text;
  if (body === '') return null;
  const lines = body.split('\n');
  const out: string[] = [];
  for (let index = 0; index < lines.length; index++) {
    const match = ANCHORED.exec(lines[index]!);
    if (!match || Number(match[1]) !== index + 1) return null;
    const content = match[3]!;
    if (tag(content) !== match[2]) return null;
    out.push(content);
  }
  return out;
}

/** Lines of a file the way the tools count them: split on `\n`, no element for a final newline. */
export function countLines(text: string): number {
  if (text === '') return 0;
  const parts = text.split('\n');
  return text.endsWith('\n') ? parts.length - 1 : parts.length;
}

export interface FileText {
  text: string;
  /** False when what came back is not the whole file's exact content (then it must not be saved). */
  editable: boolean;
  reason: string;
}

/**
 * Turn a `tools.read` result into a file's exact content. The result may be the raw text or the
 * anchored model view; anything that is not provably the whole file (a range, an outline, a note
 * about an empty or binary file) is returned read-only, so saving can never write it back.
 */
export function decodeRead(result: { text: string; total_lines?: number | null }, target?: string): FileText {
  const total = typeof result.total_lines === 'number' ? result.total_lines : null;
  // The model view opens with a header line naming the file and its line count
  // (`src/app.js (4 lines):`); it is apparatus, not content.
  let body = result.text;
  if (target !== undefined && total !== null) {
    const header = `${target} (${total} line${total === 1 ? '' : 's'}):\n`;
    if (body.startsWith(header)) body = body.slice(header.length);
  }
  const anchored = parseAnchored(body);
  if (anchored) {
    if (total !== null && anchored.length !== total) {
      return { text: result.text, editable: false, reason: `Only part of the file came back (${anchored.length} of ${total} lines).` };
    }
    // The model view drops the final newline information; files nearly always end with one.
    return { text: anchored.join('\n') + '\n', editable: true, reason: '' };
  }
  if (total === 0 && result.text !== '') {
    return { text: result.text, editable: false, reason: 'The tools describe this file instead of showing it (empty or binary).' };
  }
  if (total !== null && countLines(result.text) !== total) {
    return { text: result.text, editable: false, reason: `Only part of the file came back (${countLines(result.text)} of ${total} lines).` };
  }
  return { text: result.text, editable: true, reason: '' };
}
