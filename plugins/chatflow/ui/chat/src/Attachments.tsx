import { useHost, type Json } from '@cortex/sdk';

/** A message attachment as the chat flow journals it (`user_message.attachments`, `steer.attachments`). */
export interface AttachmentView {
  kind: string;
  path: string;
  name: string;
  bytes: number;
  stored: boolean;
  width?: number;
  height?: number;
  thumb?: string;
  /** Long pasted text made a file: its line count and the head/tail the model was shown. */
  pastedText: boolean;
  lines?: number;
  preview?: string;
}

/** The attachments of an entry's data (malformed items are left out). */
export function attachmentsOf(data: Record<string, Json>): AttachmentView[] {
  const list = data.attachments;
  if (!Array.isArray(list)) return [];
  const out: AttachmentView[] = [];
  for (const item of list) {
    if (!item || typeof item !== 'object' || Array.isArray(item)) continue;
    const record = item as Record<string, Json>;
    if (typeof record.path !== 'string' || !record.path) continue;
    out.push({
      kind: typeof record.kind === 'string' ? record.kind : 'file',
      path: record.path,
      name: typeof record.name === 'string' && record.name ? record.name : record.path,
      bytes: typeof record.bytes === 'number' ? record.bytes : 0,
      stored: record.stored === true,
      width: typeof record.width === 'number' ? record.width : undefined,
      height: typeof record.height === 'number' ? record.height : undefined,
      thumb: typeof record.thumb === 'string' && record.thumb.startsWith('data:image/') ? record.thumb : undefined,
      pastedText: record.pasted_text === true,
      lines: typeof record.lines === 'number' ? record.lines : undefined,
      preview: typeof record.preview === 'string' ? record.preview : undefined,
    });
  }
  return out;
}

function size(bytes: number): string {
  const units = ['B', 'KB', 'MB', 'GB'];
  let value = bytes;
  let unit = 0;
  while (value >= 1024 && unit + 1 < units.length) {
    value /= 1024;
    unit++;
  }
  return unit === 0 ? `${bytes} B` : `${value.toFixed(1)} ${units[unit]}`;
}

/** Test results read from pasted test output (Jest/Vitest, pytest, Go, cargo, Gradle/JUnit), for
 * display only; null when the text names no test. */
export interface TestResults {
  passed: string[];
  failed: string[];
  passedCount: number;
  failedCount: number;
}

export function parseTestResults(text: string): TestResults | null {
  const passed: string[] = [];
  const failed: string[] = [];
  let summaryPassed = 0;
  let summaryFailed = 0;
  const add = (list: string[], name: string) => {
    const clean = name.replace(/\s+\(\d+(\.\d+)?\s*m?s\)$/, '').trim();
    if (clean && !list.includes(clean)) list.push(clean);
  };
  for (const line of text.split('\n')) {
    let m: RegExpExecArray | null;
    if ((m = /^\s*[✓✔√]\s+(.+)$/.exec(line))) add(passed, m[1]!);
    else if ((m = /^\s*[✕✗×✖]\s+(.+)$/.exec(line))) add(failed, m[1]!);
    else if ((m = /^(\S+::\S+)\s+(PASSED|FAILED|ERROR)\b/.exec(line))) add(m[2] === 'PASSED' ? passed : failed, m[1]!);
    else if ((m = /^(?:FAILED|ERROR) (\S+::\S+)/.exec(line))) add(failed, m[1]!);
    else if ((m = /^\s*--- (PASS|FAIL): (\S+)/.exec(line))) add(m[1] === 'PASS' ? passed : failed, m[2]!);
    else if ((m = /^test (\S+) \.\.\. (ok|FAILED)$/.exec(line))) add(m[2] === 'ok' ? passed : failed, m[1]!);
    else if ((m = /^(.+? > .+?) (PASSED|FAILED)$/.exec(line))) add(m[2] === 'PASSED' ? passed : failed, m[1]!);
    if ((m = /(\d+) passed/.exec(line)) && /failed|passed in|Tests:/.test(line)) summaryPassed = Math.max(summaryPassed, Number(m[1]));
    if ((m = /(\d+) failed/.exec(line)) && /passed|failed in|Tests:/.test(line)) summaryFailed = Math.max(summaryFailed, Number(m[1]));
  }
  if (passed.length + failed.length + summaryPassed + summaryFailed === 0) return null;
  return { passed, failed, passedCount: Math.max(passed.length, summaryPassed), failedCount: Math.max(failed.length, summaryFailed) };
}

/** Pasted test output as a pass/fail view: failures listed in red, passes folded into one line. */
function TestResultsView({ results }: { results: TestResults }) {
  return (
    <div className="cx-test-results" data-failed={results.failedCount > 0 ? 'true' : 'false'}>
      <div className="cx-test-results-head">
        <span className="ap">Test results</span>
        <span className="cx-add">{results.passedCount} passed</span>
        {results.failedCount > 0 && <span className="cx-del"> · {results.failedCount} failed</span>}
      </div>
      {results.failed.length > 0 && (
        <ul className="cx-test-list">
          {results.failed.map((name) => <li key={name} className="cx-test-failed"><span className="cx-del" aria-label="failed">✗</span> {name}</li>)}
        </ul>
      )}
      {results.passed.length > 0 && (
        <details className="cx-test-passed">
          <summary><span className="cx-add" aria-label="passed">✓</span> {results.passed.length} passed</summary>
          <ul className="cx-test-list">
            {results.passed.map((name) => <li key={name}><span className="cx-add">✓</span> {name}</li>)}
          </ul>
        </details>
      )}
    </div>
  );
}

/** What was attached to a message: chips with a preview for images; a click shows it in Finder. */
export function Attachments({ data }: { data: Record<string, Json> }) {
  const host = useHost();
  const items = attachmentsOf(data);
  if (items.length === 0) return null;
  const pasted = items.filter((item) => item.pastedText && item.preview);
  return (
    <>
    <ul className="attached cx-attached" aria-label="Attachments">
      {items.map((item, index) => {
        const facts = [item.kind === 'folder' ? '' : size(item.bytes), item.width && item.height ? `${item.width}×${item.height}` : '']
          .filter(Boolean).join(' · ');
        return (
          <li key={`${item.path}:${index}`} className="chip cx-chip" data-kind={item.kind}>
            <button type="button" className="cx-chip-open" disabled={!host.shell}
              title={`${item.path}${item.stored ? '' : ' (your file, not copied)'}${host.shell ? ' · show in Finder' : ''}`}
              onClick={() => void host.shell?.reveal(item.path)}>
              {item.thumb && <img className="cx-thumb" src={item.thumb} alt={`Preview of ${item.name}`} />}
              {!item.pastedText && <span className="cx-chip-kind">{item.kind}</span>}
              <span className="cx-chip-name">{item.pastedText ? `Pasted text · ${(item.lines ?? 0).toLocaleString('en-US')} lines` : item.name}</span>
              {facts && <span className="cx-chip-size">{facts}</span>}
            </button>
          </li>
        );
      })}
    </ul>
    {pasted.map((item, index) => {
      const results = parseTestResults(item.preview ?? '');
      // Shown whole when the preview holds every line of the file.
      const whole = (item.preview ?? '').split('\n').length >= (item.lines ?? 0);
      return (
        <div key={`${item.path}:preview:${index}`} className="cx-pasted-block">
          {results && <TestResultsView results={results} />}
          <details className="cx-pasted">
            <summary className="ap faint">{whole ? `Raw text of ${item.name}` : `What the model was shown of ${item.name} (it reads the rest from the file)`}</summary>
            <pre className="run-output">{item.preview}</pre>
          </details>
        </div>
      );
    })}
    </>
  );
}
