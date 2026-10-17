/**
 * pane.markdown@1 — a Markdown file of the workspace, rendered as reading (the design's `.prose`).
 * Links to other Markdown files open here; web links open in the browser pane.
 */

import Markdown, { type Components } from 'react-markdown';
import remarkGfm from 'remark-gfm';
import {
  decodeRead,
  definePlugin,
  openInCode,
  dirname,
  setShared,
  useClient,
  useQuery,
  useSelection,
  useShared,
  type FileEntry,
  type ReadResult,
  showPane,
} from '@cortex/sdk';
import './markdown.css';

interface OpenFile {
  root: string;
  path: string;
}

/** Resolve a relative link against the file it appears in (`docs/a.md` + `../b.md` -> `b.md`). */
export function resolveRelative(from: string, link: string): string {
  const parts = (link.startsWith('/') ? [] : dirname(from).split('/')).filter(Boolean);
  for (const part of link.replace(/^\//, '').split('/')) {
    if (part === '' || part === '.') continue;
    if (part === '..') parts.pop();
    else parts.push(part);
  }
  return parts.join('/');
}

function MarkdownPane() {
  const client = useClient();
  const { workspace } = useSelection();
  const [open] = useShared<OpenFile | null>('markdown.open', null);
  const file = open && open.root === workspace ? open : null;
  const content = useQuery<string>(
    file ? `md:${file.root}:${file.path}` : null,
    async (c) => decodeRead(await c.call<ReadResult>('tools.read', { workspace: file!.root, target: file!.path }), file!.path).text,
    { topics: ['tools.files_changed'], when: (_topic, data) => Boolean(file && ((data as { paths?: string[] } | null)?.paths ?? []).includes(file.path)) },
  );
  const listing = useQuery<FileEntry[]>(
    workspace ? `tools.files:${workspace}:` : null,
    async (c) => (await c.call<{ entries: FileEntry[] }>('tools.files', { workspace }))?.entries ?? [],
    { topics: ['tools.files_changed'] },
  );
  const documents = (listing.data ?? []).filter((entry) => !entry.is_dir && /\.(md|markdown)$/i.test(entry.path)).map((entry) => entry.path.replace(/^\.\//, ''));

  if (!workspace) return <div className="cx-md"><p className="empty-line">Open a project to read its documents.</p></div>;
  const components: Components = {
    a: ({ href, children }) => (
      <a href={href} onClick={(event) => {
        event.preventDefault();
        if (!href || !file) return;
        if (/^https?:/i.test(href)) {
          showPane('browser');
          void client.call('browser.open', { url: href }).catch(() => undefined);
        } else if (/\.(md|markdown)(#.*)?$/i.test(href)) {
          setShared('markdown.open', { root: file.root, path: resolveRelative(file.path, href.replace(/#.*$/, '')) });
        }
      }}>{children}</a>
    ),
    img: ({ alt }) => <span className="ap faint">[image: {alt || 'untitled'}]</span>,
    table: ({ children }) => <table className="tbl">{children}</table>,
    pre: ({ children }) => <pre className="cx-code">{children}</pre>,
  };
  return (
    <div className="cx-md" data-build={__CORTEX_BUILD__} data-path={file?.path ?? ''}>
      <div className="bar">
        <span className="ap faint cx-md-path">{file ? file.path : 'No document open'}</span>
        <div className="grow" />
        {file && (
          <button className="detent" type="button" onClick={() => openInCode({ root: file.root, path: file.path })}>Edit</button>
        )}
      </div>
      <div className="cx-md-scroll">
        {!file && (
          <>
            <p className="hint">Documents in this folder:</p>
            {documents.length === 0 && <p className="empty-line">No Markdown files at the top of {workspace}.</p>}
            <ul className="lst">
              {documents.map((path) => (
                <li key={path}>
                  <button type="button" onClick={() => setShared('markdown.open', { root: workspace, path })}>
                    <div className="t">{path}</div>
                  </button>
                </li>
              ))}
            </ul>
          </>
        )}
        {file && content.error && !content.data && <p className="inline-error">Could not read {file.path}: {content.error}</p>}
        {file && content.data !== undefined && (
          <article className="prose cx-md-doc">
            <Markdown remarkPlugins={[remarkGfm]} components={components}>{content.data}</Markdown>
          </article>
        )}
        {file && content.data === undefined && !content.error && <p className="empty-line">Reading…</p>}
      </div>
    </div>
  );
}

export default definePlugin({ slot: 'pane.markdown@1', component: MarkdownPane });
