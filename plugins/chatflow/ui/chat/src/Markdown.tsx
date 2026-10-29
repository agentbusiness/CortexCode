import { memo } from 'react';
import Markdown, { type Components } from 'react-markdown';
import remarkGfm from 'remark-gfm';
import { CodeBlock } from './CodeBlock';

/**
 * An answer, set as reading. Links open in the workbench's own browser pane rather than leaving
 * the app (`onLink`); tables take the design's `.tbl`; fenced code is a `CodeBlock`.
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
    pre: ({ node, children }) => {
      const code = node?.children[0];
      if (code?.type !== 'element' || code.tagName !== 'code') return <pre className="cx-code">{children}</pre>;
      const classes = code.properties.className;
      const lang = (Array.isArray(classes) ? classes : []).map(String).find((name) => name.startsWith('language-'));
      return <CodeBlock text={textOf(code)} lang={lang ? lang.slice('language-'.length) : ''} />;
    },
  };
  return (
    <Markdown remarkPlugins={[remarkGfm]} components={components}>
      {text}
    </Markdown>
  );
});

type HastNode = { type: string; value?: string; children?: HastNode[] };
function textOf(node: HastNode): string {
  return node.type === 'text' ? (node.value ?? '') : (node.children ?? []).map(textOf).join('');
}
