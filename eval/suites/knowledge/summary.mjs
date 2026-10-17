// Per-run summary of the knowledge suite: one row per turn from results.jsonl, and a line per group.

function table(header, rows) {
  const line = (cells) => `| ${cells.map((c) => String(c ?? '-').replaceAll('|', '\\|')).join(' | ')} |`;
  return [line(header), `| ${header.map(() => '---').join(' | ')} |`, ...rows.map(line)].join('\n');
}

export function renderRunSummary(rows) {
  const L = ['# Project knowledge: one row per turn', ''];
  L.push('Checks: ✓ held, ✗ failed. `brief` = tokens of the `## What the project knows` section of the session prefix (model tokenizer when reachable, else characters / 4); `note` = lines of the longest knowledge note of the turn; `attached` = distinct items that rode along with reads.', '');
  L.push(table(
    ['scenario', 'group', 'turn', 'route', 'teaches', 'cards', 'decisions', 'brief tok (items)', 'note', 'attached', 'checks', 'met'],
    rows.map((r) => [r.scenario, r.group, r.turn + 1, r.route ?? '-', r.teaches ? 'yes' : 'no', r.cards, r.decisions, `${r.brief_tokens} (${r.brief_items})`, r.note_lines, r.attached, (r.checks ?? []).join('; '), r.passed ? 'yes' : 'no']),
  ), '');
  const groups = new Map();
  for (const r of rows) {
    const g = groups.get(r.group) ?? { turns: 0, met: 0 };
    g.turns += 1;
    g.met += r.passed ? 1 : 0;
    groups.set(r.group, g);
  }
  L.push('## Per group', '');
  L.push(table(['group', 'turns', 'met'], [...groups.entries()].map(([g, v]) => [g, v.turns, `${v.met}/${v.turns}`])), '');
  const tempting = rows.filter((r) => r.group === 'tempting');
  if (tempting.length) {
    L.push(`K3 (a general statement): a card was proposed in ${tempting.filter((r) => r.cards > 0).length} of ${tempting.length} turn(s). This is the measurement.`, '');
  }
  return L.join('\n');
}
