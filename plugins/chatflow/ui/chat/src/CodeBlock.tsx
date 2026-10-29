import { memo, useEffect, useMemo, useState, type CSSProperties, type ReactNode } from 'react';
import { LanguageDescription, type Language } from '@codemirror/language';
import { languages } from '@codemirror/language-data';
import { highlightTree, tagHighlighter, tags, type Tag } from '@lezer/highlight';
import { useThemeData, type ThemeData } from '@cortex/sdk';

/**
 * A fenced block in an answer, set as the code view sets a file: a header with the language and
 * Copy, line numbers, and the active theme's highlight style (the same rules the editor uses).
 */

type TagTable = Record<string, Tag | ((tag: Tag) => Tag)>;

/** `function(definition(variableName))` -> tags.function(tags.definition(tags.variableName)). */
function parseTag(spec: string): Tag | null {
  const table = tags as unknown as TagTable;
  const open = spec.indexOf('(');
  if (open < 0) {
    const tag = table[spec];
    return tag && typeof tag !== 'function' ? tag : null;
  }
  const outer = table[spec.slice(0, open)];
  const inner = parseTag(spec.slice(open + 1, spec.lastIndexOf(')')));
  return typeof outer === 'function' && inner ? outer(inner) : null;
}

/** The theme's rules as a highlighter whose classes are rule indexes; spans take the rule's style inline. */
interface Palette { highlighter: ReturnType<typeof tagHighlighter>; styles: CSSProperties[] }
const palettes = new WeakMap<ThemeData, Palette>();
function palette(theme: ThemeData): Palette {
  let found = palettes.get(theme);
  if (!found) {
    const styles: CSSProperties[] = [];
    const rules: { tag: Tag | Tag[]; class: string }[] = [];
    for (const rule of theme.codemirror.highlight) {
      const tag = rule.tags.map(parseTag).filter((t): t is Tag => t !== null);
      if (tag.length === 0) continue;
      rules.push({ tag, class: String(styles.length) });
      styles.push(rule.style as CSSProperties);
    }
    found = { highlighter: tagHighlighter(rules), styles };
    palettes.set(theme, found);
  }
  return found;
}

const loaded = new Map<string, Language | null>();
function useLanguage(name: string): Language | null {
  const [language, setLanguage] = useState<Language | null>(() => loaded.get(name) ?? null);
  useEffect(() => {
    if (!name) return setLanguage(null);
    if (loaded.has(name)) return setLanguage(loaded.get(name) ?? null);
    const description = LanguageDescription.matchLanguageName(languages, name, true);
    if (!description) {
      loaded.set(name, null);
      return setLanguage(null);
    }
    let live = true;
    description
      .load()
      .then((support) => {
        loaded.set(name, support.language);
        if (live) setLanguage(support.language);
      })
      .catch(() => loaded.set(name, null));
    return () => {
      live = false;
    };
  }, [name]);
  return language;
}

function highlighted(text: string, language: Language | null, theme: ThemeData | null): ReactNode[] {
  if (!language || !theme) return [text];
  const { highlighter, styles } = palette(theme);
  const out: ReactNode[] = [];
  let at = 0;
  highlightTree(language.parser.parse(text), highlighter, (from, to, classes) => {
    if (from > at) out.push(text.slice(at, from));
    const style = classes.split(' ').reduce<CSSProperties>((sum, index) => ({ ...sum, ...styles[Number(index)] }), {});
    out.push(<span key={from} style={style}>{text.slice(from, to)}</span>);
    at = to;
  });
  if (at < text.length) out.push(text.slice(at));
  return out;
}

export const CodeBlock = memo(function CodeBlock({ text, lang }: { text: string; lang: string }) {
  const code = text.replace(/\n$/, '');
  const theme = useThemeData();
  const language = useLanguage(lang.toLowerCase());
  const spans = useMemo(() => highlighted(code, language, theme), [code, language, theme]);
  const lines = useMemo(() => code.split('\n').length, [code]);
  const [copied, setCopied] = useState(false);
  useEffect(() => {
    if (!copied) return;
    const timer = setTimeout(() => setCopied(false), 1500);
    return () => clearTimeout(timer);
  }, [copied]);
  const copy = () => {
    void navigator.clipboard.writeText(code).then(() => setCopied(true));
  };
  return (
    <figure className="cx-codeblock">
      <figcaption className="cx-codeblock-head">
        <span className="cx-codeblock-lang">{lang || 'text'}</span>
        <button type="button" className="cx-codeblock-copy" onClick={copy}>
          {copied ? 'Copied' : 'Copy'}
        </button>
      </figcaption>
      <div className="cx-codeblock-body">
        <pre className="cx-codeblock-gutter" aria-hidden="true">
          {Array.from({ length: lines }, (_, index) => index + 1).join('\n')}
        </pre>
        <pre className="cx-codeblock-code">
          <code>{spans}</code>
        </pre>
      </div>
    </figure>
  );
});
