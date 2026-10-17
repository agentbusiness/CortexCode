//! Markdown documents as code-graph facts: one `section` symbol per heading (ATX `#` headings and
//! setext underlines, never inside a fenced code block), nested by heading level, with the section's
//! own text, and a `Mention` reference for every backtick identifier the section's prose names.
//!
//! Purely structural: headings make sections, backticks make mentions. What a mention links to is
//! the code graph's decision (exactly one symbol of that name or qualified name), not this file's.

use cc_lang_kit::cc_protocol::contracts::lang::{FileFacts, RefFact, RefKind, RefTarget, SymbolFact};
use cc_lang_kit::syntax::hash_hex;

/// Characters of a section's own text kept as its `code`.
pub const CODE_CHARS: usize = 4000;
/// Characters of a section's own text kept as its `doc` (what it is embedded and searched by).
pub const DOC_CHARS: usize = 600;
/// The longest identifier a mention may be.
const MENTION_CHARS: usize = 80;

struct Heading {
    level: usize,
    text: String,
    /// 1-based line of the heading (the underline's for a setext heading is `line + 1`).
    line: usize,
    /// Lines the heading itself takes (2 for setext).
    height: usize,
}

/// A fenced code block's opening: its fence character and length.
fn fence_open(line: &str) -> Option<(char, usize)> {
    let trimmed = line.trim_start_matches(' ');
    if line.len() - trimmed.len() > 3 {
        return None;
    }
    let first = trimmed.chars().next()?;
    if first != '`' && first != '~' {
        return None;
    }
    let run = trimmed.chars().take_while(|c| *c == first).count();
    (run >= 3 && (first == '~' || !trimmed[run..].contains('`'))).then_some((first, run))
}

fn fence_closes(line: &str, fence: (char, usize)) -> bool {
    let trimmed = line.trim();
    trimmed.chars().take_while(|c| *c == fence.0).count() >= fence.1 && trimmed.chars().all(|c| c == fence.0)
}

/// An ATX heading: `#`..`######`, a space, the text (closing `#`s dropped).
fn atx(line: &str) -> Option<(usize, String)> {
    let trimmed = line.trim_start_matches(' ');
    if line.len() - trimmed.len() > 3 {
        return None;
    }
    let level = trimmed.chars().take_while(|c| *c == '#').count();
    if level == 0 || level > 6 {
        return None;
    }
    let rest = &trimmed[level..];
    if !rest.is_empty() && !rest.starts_with([' ', '\t']) {
        return None;
    }
    let text = rest.trim().trim_end_matches('#').trim();
    Some((level, text.to_owned()))
}

/// A setext underline: `===` (level 1) or `---` (level 2), alone on its line.
fn setext_underline(line: &str) -> Option<usize> {
    let trimmed = line.trim();
    if trimmed.is_empty() || line.len() - line.trim_start_matches(' ').len() > 3 {
        return None;
    }
    if trimmed.chars().all(|c| c == '=') {
        Some(1)
    } else if trimmed.len() >= 3 && trimmed.chars().all(|c| c == '-') {
        Some(2)
    } else {
        None
    }
}

/// The headings of a document, in order, with fenced code blocks skipped.
fn headings(lines: &[&str]) -> Vec<Heading> {
    let mut out = Vec::new();
    let mut fence: Option<(char, usize)> = None;
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        if let Some(open) = fence {
            if fence_closes(line, open) {
                fence = None;
            }
            i += 1;
            continue;
        }
        if let Some(open) = fence_open(line) {
            fence = Some(open);
            i += 1;
            continue;
        }
        if let Some((level, text)) = atx(line) {
            if !text.is_empty() {
                out.push(Heading { level, text, line: i + 1, height: 1 });
            }
            i += 1;
            continue;
        }
        // Setext: a paragraph line (not blank, not a heading, not a list marker) under `===` / `---`.
        if i + 1 < lines.len()
            && let Some(level) = setext_underline(lines[i + 1])
            && !line.trim().is_empty()
            && !line.trim_start().starts_with(['-', '*', '+', '>', '#', '|'])
            && atx(line).is_none()
            && fence_open(line).is_none()
            && (i == 0 || lines[i - 1].trim().is_empty() || out.last().is_some_and(|h| h.line + h.height - 1 == i))
        {
            out.push(Heading { level, text: line.trim().to_owned(), line: i + 1, height: 2 });
            i += 2;
            continue;
        }
        i += 1;
    }
    out
}

/// Backtick identifiers in prose (fenced code blocks left out): `compute_hdcp`, `Score.total`,
/// `frame::ten()`; nothing with spaces, slashes or dashes, nothing starting with a digit.
fn mentions(text: &str) -> Vec<(String, usize)> {
    let mut out = Vec::new();
    let mut fence: Option<(char, usize)> = None;
    for (offset, line) in text.lines().enumerate() {
        if let Some(open) = fence {
            if fence_closes(line, open) {
                fence = None;
            }
            continue;
        }
        if let Some(open) = fence_open(line) {
            fence = Some(open);
            continue;
        }
        let mut rest = line;
        while let Some(start) = rest.find('`') {
            let after = &rest[start + 1..];
            let Some(end) = after.find('`') else { break };
            let span = &after[..end];
            rest = &after[end + 1..];
            if let Some(name) = identifier(span) {
                out.push((name, offset));
            }
        }
    }
    out
}

/// The identifier a backtick span names, if it is one.
fn identifier(span: &str) -> Option<String> {
    let name = span.trim().trim_end_matches("()").trim_end_matches("!");
    let first = name.chars().next()?;
    if name.len() > MENTION_CHARS || !(first.is_ascii_alphabetic() || first == '_') {
        return None;
    }
    if !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.' || c == ':') || name.ends_with(['.', ':']) {
        return None;
    }
    Some(name.to_owned())
}

fn cut(text: &str, chars: usize) -> String {
    text.chars().take(chars).collect()
}

/// One space between words.
fn squash(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Fill `facts` with the document's sections and their mentions.
pub fn extract(text: &str, facts: &mut FileFacts) {
    let lines: Vec<&str> = text.lines().collect();
    let found = headings(&lines);
    // Each section's own text ends at the next heading of any level; its span at the next heading
    // of its level or above (it encloses its subsections, as a class encloses its methods).
    let mut path: Vec<(usize, String)> = Vec::new();
    let mut seen: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for (index, heading) in found.iter().enumerate() {
        while path.last().is_some_and(|(level, _)| *level >= heading.level) {
            path.pop();
        }
        let parent = path.last().and_then(|(_, qname)| facts.symbols.iter().position(|s| &s.qname == qname));
        let mut qname = path.iter().map(|(_, q)| q.rsplit(" > ").next().unwrap_or(q).to_owned()).chain(std::iter::once(heading.text.clone())).collect::<Vec<_>>().join(" > ");
        let n = seen.entry(qname.clone()).or_insert(0);
        *n += 1;
        if *n > 1 {
            qname = format!("{qname} ({n})");
        }
        let own_end = found.get(index + 1).map(|next| next.line - 1).unwrap_or(lines.len());
        let span_end = found[index + 1..].iter().find(|next| next.level <= heading.level).map(|next| next.line - 1).unwrap_or(lines.len());
        let body_lines = &lines[(heading.line - 1 + heading.height).min(own_end)..own_end];
        let body = body_lines.join("\n");
        let own = lines[heading.line - 1..own_end].join("\n");
        for (name, offset) in mentions(&body) {
            facts.refs.push(RefFact {
                from: Some(facts.symbols.len()),
                kind: RefKind::Mention,
                target: RefTarget { name, qualifier: None },
                args: Vec::new(),
                line: (heading.line + heading.height + offset) as u32,
            });
        }
        let doc = squash(&body);
        facts.symbols.push(SymbolFact {
            kind: "section".into(),
            name: heading.text.clone(),
            qname: qname.clone(),
            signature: qname.clone(),
            doc: (!doc.is_empty()).then(|| cut(&doc, DOC_CHARS)),
            code: cut(&own, CODE_CHARS),
            start_line: heading.line as u32,
            end_line: span_end.max(heading.line) as u32,
            start_byte: 0,
            end_byte: 0,
            body_hash: if body.trim().is_empty() { String::new() } else { hash_hex(body.as_bytes()) },
            sig_hash: hash_hex(qname.as_bytes()),
            parent,
            exported: heading.level == 1,
            ..SymbolFact::default()
        });
        path.push((heading.level, qname));
    }
    // Mentions in the text before the first heading belong to the file.
    let preamble_end = found.first().map(|h| h.line - 1).unwrap_or(lines.len());
    for (name, offset) in mentions(&lines[..preamble_end].join("\n")) {
        facts.refs.push(RefFact { from: None, kind: RefKind::Mention, target: RefTarget { name, qualifier: None }, args: Vec::new(), line: (offset + 1) as u32 });
    }
}
