import { memo } from 'react';
import Markdown, { type Components } from 'react-markdown';
import remarkGfm from 'remark-gfm';

/**
 * An answer, set as reading. Links open in the workbench's own browser pane rather than leaving
 * the app (`onLink`); tables take the design's `.tbl`.
 */
export const MarkdownBlock = memo(function MarkdownBlock({ text, onLink }: { text: string; onLink: (url: string) => void }) {
  const components: Components = {
    a: ({ href, children }) => (
      <a
        href={href}
        onClick={(event) => {
          event.preventDefault();
          if (href && /^https?:/i.test(href)) onLink(href);
        }}
      >
        {children}
      </a>
    ),
    table: ({ children }) => <table className="tbl">{children}</table>,
    pre: ({ children }) => <pre className="cx-code">{children}</pre>,
  };
  return (
    <Markdown remarkPlugins={[remarkGfm]} components={components}>
      {text}
    </Markdown>
  );
});
