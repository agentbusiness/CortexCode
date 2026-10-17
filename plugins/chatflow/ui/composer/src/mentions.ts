/**
 * Mentions of other projects in a message (docs/IDEATION-WORKFLOW.md part 2): `@<project>` names a
 * project memory knows (`memory.project.list`), `@<project>:<symbol>` a symbol in it. A mention
 * starts at the start of the text or after whitespace (so `a@b` is not one) and names a known
 * project exactly; the longest known name wins. They are sent as `mentions` with every message
 * (`chatflow.turn`, `chatflow.steer`, `agents.steer`).
 */

/** A project as `memory.project.list` answers it. */
export interface KnownProject {
  project_id: string;
  name: string;
  root: string;
  reference?: boolean;
  /** indexing | ready | error | closed */
  state: string;
  error?: string | null;
}

/** A mention as `chatflow.turn` takes it (`mentions`). */
export interface Mention {
  project: string;
  symbol?: string;
}

/** One mention found in a text, with where it is. */
export interface Found extends Mention {
  name: string;
  start: number;
  end: number;
}

/** A symbol hit of `memory.search` (the fields the completion shows). */
export interface SymbolHit {
  kind: string;
  name: string;
  qname: string;
  path: string;
  start_line: number;
}

/** Characters of a project name (memory's `ProjectInfo.name`). */
const NAME_CHAR = /[A-Za-z0-9._-]/;
/** A symbol: identifiers joined by `.`, `::` or `#` (a name or a qualified name). */
const SYMBOL = /^[A-Za-z_$][\w$]*(?:(?:\.|::|#)[A-Za-z_$][\w$]*)*/;

function startsMention(text: string, at: number): boolean {
  return text[at] === '@' && (at === 0 || /\s/.test(text[at - 1]!));
}

/**
 * Whether a known name ends at `at`: the text ends there or goes on with a character a name cannot
 * have, or with a `.` that ends a sentence (not followed by another name character).
 */
function nameEnds(text: string, at: number): boolean {
  const next = text[at];
  if (next === undefined) return true;
  if (next === '.') return text[at + 1] === undefined || !NAME_CHAR.test(text[at + 1]!);
  return !NAME_CHAR.test(next);
}

/** The mentions in `text`, in order (`own`: the session's project, never a reference). */
export function findMentions(text: string, projects: readonly KnownProject[], own: string | null = null): Found[] {
  const byLength = projects.filter((project) => project.project_id !== own && project.name).slice().sort((a, b) => b.name.length - a.name.length);
  const out: Found[] = [];
  for (let at = 0; at < text.length; at++) {
    if (!startsMention(text, at)) continue;
    const project = byLength.find((candidate) => text.startsWith(candidate.name, at + 1) && nameEnds(text, at + 1 + candidate.name.length));
    if (!project) continue;
    let end = at + 1 + project.name.length;
    const found: Found = { project: project.project_id, name: project.name, start: at, end };
    if (text[end] === ':') {
      const symbol = SYMBOL.exec(text.slice(end + 1))?.[0];
      if (symbol) {
        found.symbol = symbol;
        end += 1 + symbol.length;
        found.end = end;
      }
    }
    out.push(found);
    at = end - 1;
  }
  return out;
}

/** The mentions `chatflow.turn` carries: each project and symbol once, in order. */
export function mentionsOf(text: string, projects: readonly KnownProject[], own: string | null = null): Mention[] {
  const out: Mention[] = [];
  for (const found of findMentions(text, projects, own)) {
    if (out.some((mention) => mention.project === found.project && mention.symbol === found.symbol)) continue;
    out.push(found.symbol ? { project: found.project, symbol: found.symbol } : { project: found.project });
  }
  return out;
}

/** What is being completed at the caret: a project name after `@`, or a symbol after `@<project>:`. */
export type Completing =
  | { kind: 'project'; from: number; to: number; query: string }
  | { kind: 'symbol'; from: number; to: number; query: string; project: KnownProject };

/** The mention being written where the caret is, if any. */
export function completingAt(text: string, caret: number, projects: readonly KnownProject[], own: string | null = null): Completing | null {
  const before = text.slice(0, caret);
  const match = /(^|\s)@([A-Za-z0-9._-]*)(?::([\w$.:#]*))?$/.exec(before);
  if (!match) return null;
  const from = match.index + match[1]!.length;
  const name = match[2]!;
  if (match[3] === undefined) return { kind: 'project', from, to: caret, query: name };
  const project = projects.find((candidate) => candidate.name === name && candidate.project_id !== own);
  return project ? { kind: 'symbol', from, to: caret, query: match[3], project } : null;
}

/** The projects offered for `query`: names starting with it (any case), the session's own left out. */
export function projectChoices(projects: readonly KnownProject[], query: string, own: string | null = null): KnownProject[] {
  const lower = query.toLowerCase();
  return projects.filter((project) => project.project_id !== own && project.name.toLowerCase().startsWith(lower));
}

/** What a chosen symbol is written as: its qualified name when it is one, else its name. */
export function symbolText(hit: SymbolHit): string {
  const qualified = SYMBOL.exec(hit.qname)?.[0];
  return qualified === hit.qname ? hit.qname : hit.name;
}

/** The text with the completed mention in place of what was typed, and the caret after it. */
export function complete(text: string, completing: Completing, inserted: string): { text: string; caret: number } {
  const before = text.slice(0, completing.from);
  const after = text.slice(completing.to);
  const spaced = after.startsWith(' ') ? inserted : `${inserted} `;
  return { text: before + spaced + after, caret: before.length + spaced.length + (after.startsWith(' ') ? 1 : 0) };
}
