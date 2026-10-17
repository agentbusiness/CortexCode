/**
 * A unified diff, set in the design's grammar: a removed line is struck through in the deep rubric
 * and steps down to the third ink; an added line keeps the full ink on
 * the rubric wash, marked in the rubric (`.diff ins`). Line numbers are apparatus.
 */

import type { ReactNode } from 'react';
import type { FileDiff, Hunk } from './types';
import { hunkCounts } from './diff';

export function DiffView({ files, hunkActions, fileActions, onOpenFile }: {
  files: readonly FileDiff[];
  /** Controls drawn on a hunk's header (stage, revert, …). */
  hunkActions?: (file: FileDiff, hunk: Hunk, index: number) => ReactNode;
  fileActions?: (file: FileDiff) => ReactNode;
  onOpenFile?: (path: string) => void;
}) {
  return (
    <div className="cx-diff">
      {files.map((file) => {
        const counts = hunkCounts(file.hunks);
        return (
          <section className="cx-diff-file" key={`${file.old_path ?? ''}→${file.path}`} data-path={file.path}>
            <div className="cx-diff-head">
              {onOpenFile ? (
                <button type="button" className="cx-diff-path" onClick={() => onOpenFile(file.path)} title={`Open ${file.path}`}>
                  {file.path}
                </button>
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
            {file.binary && <p className="hint">Binary file.</p>}
            {file.hunks.map((hunk, index) => (
              <div className="cx-hunk" key={`${hunk.header}-${index}`} data-hunk={index}>
                <div className="cx-hunk-head">
                  <span className="ap faint">{hunk.header.replace(/^(@@[^@]*@@).*$/, '$1')}</span>
                  <span className="cx-hunk-ctx">{hunk.header.replace(/^@@[^@]*@@\s?/, '')}</span>
                  <span className="grow" />
                  {hunkActions?.(file, hunk, index)}
                </div>
                <table className="cx-hunk-lines">
                  <tbody>
                    {hunk.lines.map((line, at) => (
                      <tr key={at} className={line.kind === '+' ? 'add' : line.kind === '-' ? 'del' : 'ctx'}>
                        <td className="n">{line.old_line ?? ''}</td>
                        <td className="n">{line.new_line ?? ''}</td>
                        <td className="m">{line.kind === ' ' ? '' : line.kind === '-' ? '−' : '+'}</td>
                        <td className="c">{line.text === '' ? ' ' : line.text}</td>
                      </tr>
                    ))}
                  </tbody>
                </table>
              </div>
            ))}
          </section>
        );
      })}
    </div>
  );
}
