import { useState } from 'react';
import Markdown, { type Components } from 'react-markdown';
import remarkGfm from 'remark-gfm';
import { errorMessage, openInCode, showPane, useClient, useSelection } from '@cortex/sdk';

/** Task prose uses the same Markdown dialect as chat, with links kept inside the workbench. */
export function TaskText({ text }: { text: string }) {
  const client = useClient();
  const { workspace } = useSelection();
  const [error, setError] = useState<string | null>(null);
  const components: Components = {
    a: ({ href, children }) => (
      <a href={href} onClick={(event) => {
        event.preventDefault();
        if (!href) return;
        setError(null);
        if (/^https?:\/\//i.test(href)) {
          showPane('browser');
          void client.call('browser.open', { url: href }).catch((cause) => setError(errorMessage(cause)));
        } else if (workspace && !/^(?:[a-z][a-z\d+.-]*:|\/\/|#)/i.test(href)) {
          const [path = '', anchor] = href.split('#');
          const line = anchor?.match(/^L(\d+)/i)?.[1];
          openInCode({ root: workspace, path, line: line ? Number(line) : undefined });
        }
      }}>{children}</a>
    ),
    // Do not fetch remote images while reading generated task text.
    img: ({ alt }) => <span className="cx-task-image">[Image: {alt || 'untitled'}]</span>,
    table: ({ children }) => <div className="cx-task-table" role="region" aria-label="Task table" tabIndex={0}><table>{children}</table></div>,
  };
  return (
    <div className="cx-task-text">
      <Markdown remarkPlugins={[remarkGfm]} components={components}>{text}</Markdown>
      {error && <p className="inline-error" role="alert">Could not open link: {error}</p>}
    </div>
  );
}
