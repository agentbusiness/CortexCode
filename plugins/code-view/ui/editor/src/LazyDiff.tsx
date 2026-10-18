/**
 * Every changed file's diff as one list, built for any size: a file's lines are drawn only once it
 * comes near the view (until then a block of about its height holds its place), and a file draws
 * at most LINES lines until asked for more. The design's diff grammar (`.cx-diff`): a removed line
 * struck through in the deep rubric, an added line on the wash; line numbers are apparatus.
 */

import { useEffect, useRef, useState, type ReactNode } from 'react';
import { hunkCounts, type FileDiff, type Hunk } from '@cortex/sdk';

/** Lines a file draws before "Show more"; each press adds MORE. */
const LINES = 400;
const MORE = 2000;
/** Approximate heights for a placeholder (px). */
const LINE_PX = 19;
const HUNK_HEAD_PX = 28;

export function LazyDiff({ files, hunkActions, fileActions, onOpenFile, scroller }: {
  files: readonly FileDiff[];
  hunkActions?: (file: FileDiff, hunk: Hunk, index: number) => ReactNode;
  fileActions?: (file: FileDiff) => ReactNode;
  onOpenFile?: (file: FileDiff) => void;
  /** The scrolling element the files are drawn in (their visibility is measured against it). */
  scroller: HTMLElement | null;
}) {
  return (
    <div className="cx-diff">
      {files.map((file) => (
        <FileSection key={`${file.old_path ?? ''}→${file.path}`} file={file} hunkActions={hunkActions} fileActions={fileActions} onOpenFile={onOpenFile} scroller={scroller} />
      ))}
    </div>
  );
}

function FileSection({ file, hunkActions, fileActions, onOpenFile, scroller }: {
  file: FileDiff;
  hunkActions?: (file: FileDiff, hunk: Hunk, index: number) => ReactNode;
  fileActions?: (file: FileDiff) => ReactNode;
  onOpenFile?: (file: FileDiff) => void;
  scroller: HTMLElement | null;
}) {
  const ref = useRef<HTMLElement>(null);
  const [near, setNear] = useState(false);
  const [limit, setLimit] = useState(LINES);
  const counts = hunkCounts(file.hunks);
  const total = file.hunks.reduce((sum, hunk) => sum + hunk.lines.length, 0);

  useEffect(() => {
    if (near) return;
    const element = ref.current;
    if (!element) return;
    const observer = new IntersectionObserver((seen) => {
      if (seen.some((entry) => entry.isIntersecting)) {
        setNear(true);
        observer.disconnect();
      }
    }, { root: scroller, rootMargin: '1200px 0px' });
    observer.observe(element);
    return () => observer.disconnect();
  }, [near, scroller]);

  let budget = limit;
  const shown = Math.min(total, limit);
  const placeholder = Math.min(total, limit) * LINE_PX + file.hunks.length * HUNK_HEAD_PX;
  return (
    <section className="cx-diff-file" ref={ref} data-path={file.path}>
      <div className="cx-diff-head">
        {onOpenFile ? (
          <button type="button" className="cx-diff-path" onClick={() => onOpenFile(file)} title={`Open ${file.path} at its change`}>{file.path}</button>
        ) : (
          <span className="cx-diff-path">{file.path}</span>
        )}
        {file.old_path && file.old_path !== file.path && <span className="ap faint">from {file.old_path}</span>}
        <span className="ap faint">{file.status}</span>
        <span className="grow" />
        <span className="ap cx-plus">+{counts.added}</span>
        <span className="ap cx-minus">−{counts.removed}</span>
        {fileActions?.(file)}
      </div>
      {file.binary && <p className="hint cx-diff-note">Binary file.</p>}
      {!near && !file.binary && <div className="cx-diff-placeholder" style={{ height: placeholder }} aria-hidden />}
      {near && file.hunks.map((hunk, index) => {
        if (budget <= 0) return null;
        const lines = hunk.lines.slice(0, budget);
        budget -= lines.length;
        return (
          <div className="cx-hunk" key={`${hunk.header}-${index}`} data-hunk={index}>
            <div className="cx-hunk-head">
              <span className="ap faint">{hunk.header.replace(/^(@@[^@]*@@).*$/, '$1')}</span>
              <span className="cx-hunk-ctx">{hunk.header.replace(/^@@[^@]*@@\s?/, '')}</span>
              <span className="grow" />
              {hunkActions?.(file, hunk, index)}
            </div>
            <table className="cx-hunk-lines">
              <tbody>
                {lines.map((line, at) => (
                  <tr key={at} className={line.kind === '+' ? 'add' : line.kind === '-' ? 'del' : 'ctx'}>
                    <td className="n">{line.old_line ?? ''}</td>
                    <td className="n">{line.new_line ?? ''}</td>
                    <td className="m">{line.kind === ' ' ? '' : line.kind === '-' ? '−' : '+'}</td>
                    <td className="c">{line.text === '' ? ' ' : line.text}</td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        );
      })}
      {near && total > shown && (
        <div className="cx-diff-more">
          <span className="ap faint">{(total - shown).toLocaleString()} more lines</span>
          <button type="button" className="detent" onClick={() => setLimit((current) => current + MORE)}>Show {Math.min(MORE, total - shown).toLocaleString()} more</button>
          {onOpenFile && <button type="button" className="detent" onClick={() => onOpenFile(file)}>Open the file</button>}
        </div>
      )}
    </section>
  );
}
