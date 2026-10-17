// Minimal TOML subset parser/serializer for the frame config (structural parsing only):
// comments, [table], [[array.of.tables]], [nested.table] inside the last array element,
// and values: basic strings, literal strings, integers, floats, booleans, inline arrays of those.

function parseValue(src, lineNo) {
  const s = src.trim();
  if (s.startsWith('"')) {
    const m = s.match(/^"((?:[^"\\]|\\.)*)"$/);
    if (!m) throw new Error(`toml line ${lineNo}: bad string ${s}`);
    return JSON.parse(`"${m[1]}"`);
  }
  if (s.startsWith("'")) {
    const m = s.match(/^'([^']*)'$/);
    if (!m) throw new Error(`toml line ${lineNo}: bad literal string ${s}`);
    return m[1];
  }
  if (s === 'true') return true;
  if (s === 'false') return false;
  if (s.startsWith('[')) {
    const inner = s.slice(1, -1).trim();
    if (!inner) return [];
    const items = [];
    let depth = 0, cur = '', inStr = null;
    for (let i = 0; i < inner.length; i++) {
      const ch = inner[i];
      if (inStr) {
        cur += ch;
        if (ch === '\\' && inStr === '"') { cur += inner[++i]; continue; }
        if (ch === inStr) inStr = null;
        continue;
      }
      if (ch === '"' || ch === "'") { inStr = ch; cur += ch; continue; }
      if (ch === '[') depth++;
      if (ch === ']') depth--;
      if (ch === ',' && depth === 0) { if (cur.trim()) items.push(parseValue(cur, lineNo)); cur = ''; continue; }
      cur += ch;
    }
    if (cur.trim()) items.push(parseValue(cur, lineNo));
    return items;
  }
  if (/^[+-]?\d[\d_]*$/.test(s)) return Number(s.replace(/_/g, ''));
  if (/^[+-]?\d[\d_]*(\.\d+)?([eE][+-]?\d+)?$/.test(s)) return Number(s.replace(/_/g, ''));
  throw new Error(`toml line ${lineNo}: unsupported value ${s}`);
}

function stripComment(line) {
  let inStr = null;
  for (let i = 0; i < line.length; i++) {
    const ch = line[i];
    if (inStr) {
      if (ch === '\\' && inStr === '"') { i++; continue; }
      if (ch === inStr) inStr = null;
      continue;
    }
    if (ch === '"' || ch === "'") inStr = ch;
    else if (ch === '#') return line.slice(0, i);
  }
  return line;
}

export function parseToml(text) {
  const root = {};
  let cur = root;
  const lines = text.split('\n');
  for (let i = 0; i < lines.length; i++) {
    const line = stripComment(lines[i]).trim();
    if (!line) continue;
    let m;
    if ((m = line.match(/^\[\[([A-Za-z0-9_.-]+)\]\]$/))) {
      const parts = m[1].split('.');
      let t = root;
      for (const p of parts.slice(0, -1)) {
        t[p] ??= {};
        t = Array.isArray(t[p]) ? t[p][t[p].length - 1] : t[p];
      }
      const last = parts[parts.length - 1];
      t[last] ??= [];
      const obj = {};
      t[last].push(obj);
      cur = obj;
    } else if ((m = line.match(/^\[([A-Za-z0-9_.-]+)\]$/))) {
      const parts = m[1].split('.');
      let t = root;
      for (const p of parts) {
        t[p] ??= {};
        t = Array.isArray(t[p]) ? t[p][t[p].length - 1] : t[p];
      }
      cur = t;
    } else if ((m = line.match(/^([A-Za-z0-9_-]+|"[^"]+")\s*=\s*(.+)$/))) {
      const key = m[1].startsWith('"') ? JSON.parse(m[1]) : m[1];
      cur[key] = parseValue(m[2], i + 1);
    } else {
      throw new Error(`toml line ${i + 1}: cannot parse: ${line}`);
    }
  }
  return root;
}

function fmtValue(v) {
  if (typeof v === 'string') return JSON.stringify(v);
  if (typeof v === 'boolean' || typeof v === 'number') return String(v);
  if (Array.isArray(v)) return `[${v.map(fmtValue).join(', ')}]`;
  throw new Error(`unsupported toml value ${JSON.stringify(v)}`);
}

function isTable(v) {
  return v && typeof v === 'object' && !Array.isArray(v);
}

function isTableArray(v) {
  return Array.isArray(v) && v.length > 0 && v.every(isTable);
}

/** Serialize an object produced by parseToml (or built the same way). */
export function stringifyToml(obj) {
  const out = [];
  function emit(table, prefix) {
    const scalars = Object.entries(table).filter(([, v]) => !isTable(v) && !isTableArray(v));
    for (const [k, v] of scalars) out.push(`${k} = ${fmtValue(v)}`);
    for (const [k, v] of Object.entries(table)) {
      const name = prefix ? `${prefix}.${k}` : k;
      if (isTable(v)) {
        out.push('', `[${name}]`);
        emit(v, name);
      } else if (isTableArray(v)) {
        for (const item of v) {
          out.push('', `[[${name}]]`);
          emit(item, name);
        }
      }
    }
  }
  emit(obj, '');
  return out.join('\n').trim() + '\n';
}
