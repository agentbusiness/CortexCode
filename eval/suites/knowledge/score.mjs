// Project-knowledge scoring: everything structural, from the project's journals (the runner's
// `journal.json` snapshots), the graph (`memory.knowledge.list`, fetched by the suite) and the
// workspace, never from words in the prompt.
//
// What one turn is judged on (docs/PROJECT-KNOWLEDGE-PLAN.md P7, the bank's `expect`):
//   - teaches / settles: the ROUTE entry's `teaches` / `settles` flags, and the `knowledge` /
//     `decision` entries the turn proposed (cards);
//   - recalled: an item kept earlier in the task (its id or its text) appears in this session's
//     context entry (the brief), in a host note of purpose `knowledge`, or attached to a read
//     (`knowledge://<id>` in a tool result);
//   - read_carries_knowledge: a read whose output names the symbol also carries a knowledge line;
//   - section_read: a tool result shows a document section of the given file;
//   - edits_only: every edit tool call of the turn names one of the given files;
//   - file_contains: a workspace file holds one of the strings;
//   - answer_contains_any: the turn's assistant text holds one of the strings;
//   - brief_max_tokens / note_max_lines: the `## What the project knows` section of the context
//     entry, counted with the model's tokenizer (characters / 4 when it is not reachable), and the
//     longest knowledge note.
import fs from 'node:fs';
import path from 'node:path';

const BRIEF_HEADING = '## What the project knows';

function parseArgs(args) {
  if (typeof args !== 'string') return args && typeof args === 'object' ? args : {};
  try { const v = JSON.parse(args); return v && typeof v === 'object' ? v : {}; } catch { return {}; }
}

/** The entries of one session, sorted. */
export function entriesOf(journals, sessionId) {
  return ((journals?.[sessionId]?.entries) ?? []).filter((e) => e && Number.isFinite(e.seq)).sort((a, b) => a.seq - b.seq);
}

/** The entries of the turn: the main session's from `from_seq`, and those of the turn's run id. */
export function turnEntries(journals, sessionId, turn) {
  const from = turn?.from_seq ?? 1;
  return entriesOf(journals, sessionId).filter((e) => e.seq >= from && (!turn?.run_id || !e.data?.run_id || e.data.run_id === turn.run_id));
}

/** The brief section of a system prompt (text after the heading, up to the next `## `). */
export function briefOf(system) {
  const text = typeof system === 'string' ? system : '';
  const at = text.indexOf(BRIEF_HEADING);
  if (at < 0) return '';
  const rest = text.slice(at + BRIEF_HEADING.length);
  const end = rest.search(/^## /m);
  return (end >= 0 ? rest.slice(0, end) : rest).trim();
}

function textOf(value) {
  return typeof value === 'string' ? value : value == null ? '' : JSON.stringify(value);
}

/** Knowledge ids named in a text (`knowledge://k-...`). */
export function knowledgeIds(text) {
  return [...String(text ?? '').matchAll(/knowledge:\/\/(k-[0-9a-f]{16})/g)].map((m) => m[1]);
}

/**
 * Score one turn. `kept` = the knowledge items the graph holds for the project (with history);
 * `keptBefore` = ids and texts of items kept in earlier turns of this task (what `recalled` looks
 * for); `counts` = token counts by sha256 of text (the suite measures them with the model).
 */
export function scoreTurn({ journals, sessionId, turn, expect, kept = [], keptBefore = { ids: [], texts: [] }, workspace = null, tokensOf = null }) {
  const all = entriesOf(journals, sessionId);
  const inTurn = turnEntries(journals, sessionId, turn);
  const routes = inTurn.filter((e) => e.kind === 'route' && e.data?.stage === 'route');
  const route = routes.at(-1);
  const teaches = route?.data?.teaches === true;
  const settles = route?.data?.settles === true;
  const cards = inTurn.filter((e) => e.kind === 'knowledge');
  const decisions = inTurn.filter((e) => e.kind === 'decision');
  const notes = inTurn.filter((e) => e.kind === 'note' && e.data?.purpose === 'knowledge');
  const results = inTurn.filter((e) => e.kind === 'tool_result');
  const calls = inTurn.filter((e) => e.kind === 'tool_call');
  const context = all.find((e) => e.kind === 'context');
  const brief = briefOf(context?.data?.system);
  const answer = inTurn.filter((e) => e.kind === 'assistant_message').map((e) => textOf(e.data?.text)).join('\n');
  const attached = results.flatMap((e) => knowledgeIds(e.data?.output));
  const noted = notes.flatMap((e) => knowledgeIds(e.data?.text));
  const noteLines = Math.max(0, ...notes.map((e) => textOf(e.data?.text).split('\n').filter((l) => /knowledge:\/\//.test(l)).length));
  const briefTokens = brief ? (tokensOf ? tokensOf(brief) : Math.ceil(brief.length / 4)) : 0;
  const checks = [];
  const check = (name, ok, detail) => checks.push({ name, ok: !!ok, detail });

  if (expect.teaches !== undefined) {
    check('teaches', teaches === expect.teaches && (cards.length > 0) === expect.teaches, `route teaches=${teaches}, ${cards.length} card(s)`);
  }
  if (expect.kinds) {
    const kinds = cards.map((e) => String(e.data?.kind));
    check('kinds', kinds.some((k) => expect.kinds.includes(k)), `cards: ${kinds.join(', ') || 'none'}`);
  }
  if (expect.settles !== undefined) {
    check('settles', settles === expect.settles && (decisions.length > 0) === expect.settles, `route settles=${settles}, ${decisions.length} decision card(s)`);
  }
  if (expect.recalled) {
    const texts = keptBefore.texts.filter(Boolean);
    const inBrief = keptBefore.ids.some((id) => brief.includes(id)) || texts.some((t) => brief.includes(t));
    const inNote = noted.some((id) => keptBefore.ids.includes(id)) || texts.some((t) => notes.some((e) => textOf(e.data?.text).includes(t)));
    const inRead = attached.some((id) => keptBefore.ids.includes(id));
    check('recalled', inBrief || inNote || inRead, `brief=${inBrief} note=${inNote} read=${inRead} (${keptBefore.ids.length} kept earlier)`);
  }
  if (expect.read_carries_knowledge) {
    const symbol = expect.read_carries_knowledge;
    const carried = results.some((e) => textOf(e.data?.output).includes(symbol) && /knowledge:\/\//.test(textOf(e.data?.output)));
    check('read_carries_knowledge', carried, `reads naming ${symbol} with a knowledge line: ${carried}`);
  }
  if (expect.section_read) {
    const file = expect.section_read;
    const shown = results.some((e) => { const out = textOf(e.data?.output); return out.includes(file) && (/\bsection\b/.test(out) || out.includes(`md:${file}#`)); });
    check('section_read', shown, `a result shows a section of ${file}: ${shown}`);
  }
  if (expect.edits_only) {
    const edits = calls.filter((e) => ['edit', 'write'].includes(String(e.data?.tool ?? e.data?.raw_name))).map((e) => String(parseArgs(e.data?.args).path ?? ''));
    const stray = edits.filter((p) => !expect.edits_only.some((allowed) => p === allowed || p.endsWith(`/${allowed}`)));
    check('edits_only', edits.length > 0 && stray.length === 0, edits.length ? `edited: ${[...new Set(edits)].join(', ')}` : 'no edit');
  }
  if (expect.file_contains && workspace) {
    const file = path.join(workspace, expect.file_contains.path);
    let text = null;
    try { text = fs.readFileSync(file, 'utf8'); } catch { /* missing */ }
    check('file_contains', text !== null && expect.file_contains.any.some((s) => text.includes(s)), text === null ? `${expect.file_contains.path} missing` : `${expect.file_contains.path} written`);
  }
  if (expect.answer_contains_any) {
    const low = answer.toLowerCase();
    check('answer_contains_any', expect.answer_contains_any.some((s) => low.includes(s.toLowerCase())), `answer ${answer.length} chars`);
  }
  if (expect.brief_max_tokens !== undefined) {
    check('brief_max_tokens', briefTokens <= expect.brief_max_tokens, `brief ${briefTokens} tokens`);
  }
  if (expect.note_max_lines !== undefined) {
    check('note_max_lines', noteLines <= expect.note_max_lines, `longest knowledge note ${noteLines} line(s)`);
  }
  const passed = checks.every((c) => c.ok);
  return {
    passed, checks, route: route?.data?.route ?? null, teaches, settles,
    cards: cards.map((e) => ({ seq: e.seq, kind: e.data?.kind, name: e.data?.name, state: e.data?.state, knowledge_id: e.data?.knowledge_id ?? null })),
    decisions: decisions.map((e) => ({ seq: e.seq, decided: e.data?.decided, state: e.data?.state, knowledge_id: e.data?.knowledge_id ?? null })),
    brief_tokens: briefTokens, brief_items: brief ? brief.split('\n').filter((l) => l.startsWith('- ')).length : 0,
    note_lines: noteLines, attached: [...new Set(attached)].length, kept_total: kept.length,
    answer_chars: answer.length,
  };
}
