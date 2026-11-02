import { memo } from 'react';
import Markdown, { type Components } from 'react-markdown';
import remarkGfm from 'remark-gfm';
import { CodeBlock } from './CodeBlock';

/** File names the answers write as code: a path with a folder, or a name with a known extension,
 *  each with an optional `:line` (`src/a.rs:42`, `README.md`). */
const FILE_EXTENSIONS = 'rs|ts|tsx|js|jsx|mjs|cjs|py|go|java|kt|swift|c|h|cc|cpp|hpp|cs|rb|php|sh|zsh|bash|md|markdown|mdx|txt|toml|json|jsonl|yaml|yml|xml|html|css|scss|sql|lock|cfg|ini|env|gradle';
const FILE_REF = new RegExp(`^(?:\\./)?(/?(?:[\\w.@+-]+/)*[\\w.@+-]+\\.(?:${FILE_EXTENSIONS})|(?:[\\w.@+-]+/)+[\\w.@+-]+)(?::(\\d+)(?::\\d+)?|#L(\\d+))?$`, 'i');

/** The file and line a link or code span names, or null when it names none. */
export function fileRef(text: string): { path: string; line: number | null } | null {
  const match = FILE_REF.exec(text.trim());
  if (!match || /^[a-z][a-z0-9+.-]*:\/\//i.test(text)) return null;
  const line = Number(match[2] ?? match[3]);
  return { path: match[1]!, line: Number.isFinite(line) && line > 0 ? line : null };
}

/**
 * An answer, set as reading. Links open in the workbench's own browser pane rather than leaving
 * the app (`onLink`); a file named in a link or a code span opens in the app (`onFile`: code in the
 * editor, Markdown in its view); tables take the design's `.tbl`; fenced code is a `CodeBlock`.
 */
export const MarkdownBlock = memo(function MarkdownBlock({ text, onLink, onFile }: { text: string; onLink: (url: string) => void; onFile: (path: string, line: number | null) => void }) {
  const components: Components = {
    a: ({ href, children }) => (
      <a
        href={href}
        onClick={(event) => {
          event.preventDefault();
          if (!href) return;
          if (/^https?:/i.test(href)) {
            onLink(href);
            return;
          }
          const file = fileRef(decodeURI(href.replace(/^file:\/\//i, '')));
          if (file) onFile(file.path, file.line);
        }}
      >
        {children}
      </a>
    ),
    code: ({ children }) => {
      const file = typeof children === 'string' ? fileRef(children) : null;
      if (!file) return <code>{children}</code>;
      return (
        <code className="cx-file-ref" role="link" tabIndex={0} title={`Open ${file.path}`}
          onClick={() => onFile(file.path, file.line)}
          onKeyDown={(event) => { if (event.key === 'Enter') onFile(file.path, file.line); }}>
          {children}
        </code>
      );
    },
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
