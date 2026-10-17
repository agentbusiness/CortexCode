/**
 * Lines the code graph and the journal write into the editor, above the code they are about:
 * who made a change (and the turn it came from, with a jump to that step), and, for a changed
 * symbol, how many call sites reach it, expandable inline to the sites themselves.
 */

import { StateEffect, StateField, type EditorState, type Range } from '@codemirror/state';
import { Decoration, EditorView, WidgetType, type DecorationSet } from '@codemirror/view';

export interface CallSiteNote {
  path: string;
  line: number;
  caller: string;
}

export type Annotation =
  | { kind: 'hunk'; line: number; index: number; who: string; turn: string; when: string; exact: boolean; sessionId: string | null; seq: number | null }
  | { kind: 'symbol'; line: number; sid: string; name: string; symbolKind: string; sites: CallSiteNote[]; tests: number; open: boolean };

export interface AnnotationActions {
  showStep(sessionId: string, seq: number): void;
  toggleSymbol(sid: string): void;
  openSite(path: string, line: number): void;
}

/** The current actions (React re-creates them; widgets read the latest). */
export const actionsRef: { current: AnnotationActions | null } = { current: null };

class AnnotationWidget extends WidgetType {
  constructor(readonly note: Annotation) {
    super();
  }

  override eq(other: AnnotationWidget): boolean {
    return JSON.stringify(other.note) === JSON.stringify(this.note);
  }

  override toDOM(): HTMLElement {
    const note = this.note;
    const dom = document.createElement('div');
    dom.className = `cx-anno cx-anno-${note.kind}`;
    const ap = (text: string, extra = '') => {
      const span = document.createElement('span');
      span.className = `ap${extra ? ` ${extra}` : ''}`;
      span.textContent = text;
      return span;
    };
    if (note.kind === 'hunk') {
      dom.dataset.hunk = String(note.index);
      dom.appendChild(ap(note.exact ? 'Written by' : 'Last edited by', 'faint'));
      dom.appendChild(ap(note.who, 'origin'));
      if (note.turn) {
        const turn = document.createElement('span');
        turn.className = 'cx-anno-turn';
        turn.textContent = `“${note.turn.length > 90 ? `${note.turn.slice(0, 90)}…` : note.turn}”`;
        turn.title = note.turn;
        dom.appendChild(turn);
      }
      if (note.when) dom.appendChild(ap(note.when, 'faint'));
      if (note.sessionId && note.seq !== null) {
        const button = document.createElement('button');
        button.type = 'button';
        button.className = 'cx-anno-go';
        button.textContent = 'Show the step';
        const { sessionId, seq } = note;
        button.onmousedown = (event) => {
          event.preventDefault();
          actionsRef.current?.showStep(sessionId, seq);
        };
        dom.appendChild(button);
      }
      return dom;
    }
    dom.dataset.sid = note.sid;
    dom.appendChild(ap(note.symbolKind, 'faint'));
    const name = document.createElement('span');
    name.className = 'cx-anno-sym';
    name.textContent = note.name;
    dom.appendChild(name);
    const toggle = document.createElement('button');
    toggle.type = 'button';
    toggle.className = 'cx-anno-go';
    toggle.textContent = `${note.sites.length} call site${note.sites.length === 1 ? '' : 's'}${note.tests ? ` · ${note.tests} test${note.tests === 1 ? '' : 's'}` : ''} ${note.open ? '▾' : '▸'}`;
    toggle.setAttribute('aria-expanded', String(note.open));
    toggle.onmousedown = (event) => {
      event.preventDefault();
      actionsRef.current?.toggleSymbol(note.sid);
    };
    dom.appendChild(toggle);
    if (note.open) {
      const list = document.createElement('ul');
      list.className = 'cx-anno-sites';
      for (const site of note.sites) {
        const item = document.createElement('li');
        const link = document.createElement('button');
        link.type = 'button';
        link.className = 'cx-file-link';
        link.textContent = `${site.path}:${site.line}`;
        link.onmousedown = (event) => {
          event.preventDefault();
          actionsRef.current?.openSite(site.path, site.line);
        };
        item.appendChild(link);
        item.appendChild(ap(site.caller, 'faint'));
        list.appendChild(item);
      }
      if (note.sites.length === 0) list.appendChild(Object.assign(document.createElement('li'), { textContent: 'No call sites in the graph.', className: 'hint' }));
      dom.appendChild(list);
    }
    return dom;
  }

  override ignoreEvent(): boolean {
    return false;
  }
}

export const setAnnotations = StateEffect.define<Annotation[]>();

function build(state: EditorState, notes: readonly Annotation[]): DecorationSet {
  const ranges: Range<Decoration>[] = [];
  for (const note of notes) {
    if (note.line < 1 || note.line > state.doc.lines) continue;
    const from = state.doc.line(note.line).from;
    ranges.push(Decoration.widget({ widget: new AnnotationWidget(note), block: true, side: -1 }).range(from));
  }
  return Decoration.set(ranges, true);
}

export const annotationField = StateField.define<{ notes: Annotation[]; decorations: DecorationSet }>({
  create: () => ({ notes: [], decorations: Decoration.none }),
  update(value, tr) {
    for (const effect of tr.effects) if (effect.is(setAnnotations)) return { notes: effect.value, decorations: build(tr.state, effect.value) };
    if (tr.docChanged) return { notes: value.notes, decorations: value.decorations.map(tr.changes) };
    return value;
  },
  provide: (field) => EditorView.decorations.from(field, (value) => value.decorations),
});
