/**
 * Unified-diff mechanics (structural, no judgement): parsing `diff --git` text into hunks and
 * reconstructing a file's original text from its current text and the hunks that produced it.
 */

import type { DiffLine, FileDiff, Hunk } from './types';

const HUNK_HEADER = /^@@ -(\d+)(?:,(\d+))? \+(\d+)(?:,(\d+))? @@/;

/** Parse unified diff text (one or more files) into the vcs contract's `FileDiff` shape. */
export function parseUnifiedDiff(text: string): FileDiff[] {
  const files: FileDiff[] = [];
  let file: FileDiff | null = null;
  let hunk: Hunk | null = null;
  let oldLine = 0;
  let newLine = 0;
  const lines = text.split('\n');
  for (let i = 0; i < lines.length; i++) {
    const line = lines[i]!;
    if (line.startsWith('diff --git ')) {
      const match = /^diff --git a\/(.+) b\/(.+)$/.exec(line);
      file = { path: match?.[2] ?? line.slice(11), old_path: match?.[1] ?? null, status: 'modified', binary: false, hunks: [] };
      files.push(file);
      hunk = null;
      continue;
    }
    if (line.startsWith('--- ') && !hunk) {
      const path = stripPrefix(line.slice(4));
      if (!file) {
        file = { path: path ?? '', old_path: path, status: 'modified', binary: false, hunks: [] };
        files.push(file);
      }
      if (path === null) file.status = 'added';
      continue;
    }
    if (line.startsWith('+++ ') && file && !hunk) {
      const path = stripPrefix(line.slice(4));
      if (path === null) file.status = 'deleted';
      else file.path = path;
      continue;
    }
    if (file && /^new file mode/.test(line)) { file.status = 'added'; continue; }
    if (file && /^deleted file mode/.test(line)) { file.status = 'deleted'; continue; }
    if (file && /^Binary files /.test(line)) { file.binary = true; continue; }
    const header = HUNK_HEADER.exec(line);
    if (header) {
      if (!file) {
        file = { path: '', old_path: null, status: 'modified', binary: false, hunks: [] };
        files.push(file);
      }
      hunk = {
        header: line,
        old_start: Number(header[1]),
        old_lines: header[2] === undefined ? 1 : Number(header[2]),
        new_start: Number(header[3]),
        new_lines: header[4] === undefined ? 1 : Number(header[4]),
        lines: [],
      };
      oldLine = hunk.old_start;
      newLine = hunk.new_start;
      file.hunks.push(hunk);
      continue;
    }
    if (!hunk) continue;
    const mark = line[0];
    if (mark === '+') {
      hunk.lines.push({ kind: '+', text: line.slice(1), old_line: null, new_line: newLine++ });
    } else if (mark === '-') {
      hunk.lines.push({ kind: '-', text: line.slice(1), old_line: oldLine++, new_line: null });
    } else if (mark === ' ') {
      hunk.lines.push({ kind: ' ', text: line.slice(1), old_line: oldLine++, new_line: newLine++ });
    } else if (mark === '\\') {
      // "\ No newline at end of file": a property of the previous line, not a line.
      const previous = hunk.lines[hunk.lines.length - 1];
      if (previous) previous.no_eol = true;
    } else if (line === '' && i === lines.length - 1) {
      // Trailing newline of the diff text itself.
    } else {
      hunk = null;
    }
  }
  return files;
}

function stripPrefix(path: string): string | null {
  const clean = path.replace(/\t.*$/, '');
  if (clean === '/dev/null') return null;
  return clean.replace(/^[ab]\//, '');
}

/**
 * The text a file had before `hunks` were applied to it, given its current text.
 *
 * Each hunk's new-side range in the current text is replaced by its context and removed lines,
 * from the last hunk to the first so earlier positions stay valid.
 */
export function reverseApply(current: string, hunks: readonly Hunk[]): string {
  const endsWithNewline = current.endsWith('\n');
  const lines = current.length === 0 ? [] : (endsWithNewline ? current.slice(0, -1) : current).split('\n');
  const ordered = [...hunks].sort((a, b) => b.new_start - a.new_start);
  for (const hunk of ordered) {
    const original = hunk.lines.filter((line: DiffLine) => line.kind !== '+').map((line) => line.text);
    // With zero new lines, `new_start` names the line before the gap.
    const start = hunk.new_lines === 0 ? hunk.new_start : hunk.new_start - 1;
    lines.splice(Math.max(0, start), hunk.new_lines, ...original);
  }
  if (lines.length === 0) return '';
  // End of file: a hunk that reaches it says whether each side ends without a newline.
  const oldNoEol = hunks.some((hunk) => hunk.lines.some((line) => line.kind !== '+' && line.no_eol));
  const newNoEol = hunks.some((hunk) => hunk.lines.some((line) => line.kind !== '-' && line.no_eol));
  const originalEndsWithNewline = oldNoEol ? false : newNoEol ? true : endsWithNewline || current.length === 0;
  return lines.join('\n') + (originalEndsWithNewline ? '\n' : '');
}

/** Count added and removed lines of a set of hunks. */
export function hunkCounts(hunks: readonly Hunk[]): { added: number; removed: number } {
  let added = 0;
  let removed = 0;
  for (const hunk of hunks) {
    for (const line of hunk.lines) {
      if (line.kind === '+') added++;
      else if (line.kind === '-') removed++;
    }
  }
  return { added, removed };
}

/** 1-based line of the first change on the new side of a file's hunks (for bringing it into view). */
export function firstChangedLine(hunks: readonly Hunk[]): number | null {
  for (const hunk of hunks) {
    if (hunk.new_lines === 0) return Math.max(1, hunk.new_start);
    let line = hunk.new_start;
    for (const entry of hunk.lines) {
      if (entry.kind === ' ') line++;
      else return Math.max(1, line);
    }
  }
  return null;
}

/** Index of the hunk whose new-side range holds `line` (1-based), or -1. A pure deletion holds
 *  the line before it and the one after it (where its removed text is shown). */
export function hunkAtLine(hunks: readonly Hunk[], line: number): number {
  return hunks.findIndex((hunk) => {
    if (hunk.new_lines === 0) return line === hunk.new_start || line === hunk.new_start + 1;
    return line >= hunk.new_start && line <= hunk.new_start + hunk.new_lines - 1;
  });
}
