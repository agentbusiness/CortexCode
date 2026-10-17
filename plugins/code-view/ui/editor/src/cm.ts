/**
 * CodeMirror's setup. Its look comes from the active theme plugin (@cortex/sdk `ThemeData`): the
 * theme publishes CodeMirror's theme and highlight style as plain specs, this module turns them
 * into extensions, and the editor reconfigures when another theme version becomes active.
 */

import { HighlightStyle, bracketMatching, foldGutter, indentOnInput, syntaxHighlighting } from '@codemirror/language';
import { EditorView, drawSelection, highlightActiveLine, highlightActiveLineGutter, highlightSpecialChars, keymap, lineNumbers } from '@codemirror/view';
import { defaultKeymap, history, historyKeymap } from '@codemirror/commands';
import { tags, type Tag } from '@lezer/highlight';
import type { Extension } from '@codemirror/state';
import type { ThemeData } from '@cortex/sdk';

type TagTable = Record<string, Tag | ((tag: Tag) => Tag)>;

/** `function(definition(variableName))` -> tags.function(tags.definition(tags.variableName)). */
export function parseTag(spec: string): Tag | null {
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

export function themeExtensions(theme: ThemeData | null): Extension[] {
  if (!theme) return [];
  const rules = theme.codemirror.highlight
    .map((rule) => ({ tag: rule.tags.map(parseTag).filter((tag): tag is Tag => tag !== null), ...rule.style }))
    .filter((rule) => rule.tag.length > 0);
  return [EditorView.theme(theme.codemirror.theme), syntaxHighlighting(HighlightStyle.define(rules))];
}

/**
 * The editing basics. Keys here are text editing (moving, selecting, deleting, undo) — every action
 * also has a visible button in the editor's header.
 */
export function basics(): Extension[] {
  return [
    lineNumbers(),
    highlightActiveLineGutter(),
    foldGutter(),
    highlightSpecialChars(),
    history(),
    drawSelection(),
    indentOnInput(),
    bracketMatching(),
    highlightActiveLine(),
    keymap.of([...defaultKeymap, ...historyKeymap]),
  ];
}
