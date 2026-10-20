//! What a definition does, in the words already written around it: no model. A symbol's `summary`
//! is its doc's first sentence, what callers say right above their calls, what its tests' names say,
//! the documentation headings that mention it, and its body's own comments and message strings.
//!
//! The words are gathered from facts the sync already holds (the language plugins' notes, the
//! resolved edges): one pass over the edges that carry words, borrowing their text, then one
//! composed string per definition whose words may have changed.

use crate::facts::{FileFacts, SymbolFact};
use crate::model::{self, EdgeRow, Model};
use std::collections::HashMap;

/// Items kept per kind of words, and characters per summary.
const HEARD_PER_KIND: usize = 3;
const SUMMARY_CHARS: usize = 700;

/// What other files say about one definition.
#[derive(Default)]
pub struct Heard<'a> {
    /// Comments right above calls to it.
    callers: Vec<&'a str>,
    /// Qualified names of the tests that test it.
    tests: Vec<&'a str>,
    /// Heading paths of documentation sections that mention it.
    docs: Vec<&'a str>,
}

impl<'a> Heard<'a> {
    fn add(list: &mut Vec<&'a str>, text: &'a str) {
        if list.len() < HEARD_PER_KIND && !text.is_empty() && !list.contains(&text) {
            list.push(text);
        }
    }
}

/// The qualified name in `sid` of a definition in `file` (`<prefix>:<path>#<qname>`).
fn qname_in<'s>(sid: &'s str, file: &FileFacts) -> Option<&'s str> {
    sid.strip_prefix(model::sid_prefix(file))?.strip_prefix(':')?.strip_prefix(file.path.as_str())?.strip_prefix('#')
}

/// The words other files say about definitions, by target sid, and the targets each file's words
/// reach (by path). Only edges that carry words are looked at: calls under a comment, tests,
/// mentions.
pub fn heard<'a>(model: &'a Model, all_edges: &'a [(String, Vec<EdgeRow>, String)]) -> (HashMap<&'a str, Heard<'a>>, HashMap<&'a str, Vec<&'a str>>) {
    let mut heard: HashMap<&'a str, Heard<'a>> = HashMap::new();
    let mut reach: HashMap<&'a str, Vec<&'a str>> = HashMap::new();
    for (path, edges, _) in all_edges {
        let Some(file) = model.files.get(path) else { continue };
        let mut reached: Vec<&'a str> = Vec::new();
        for edge in edges {
            match edge.rel {
                "CALLS" | "INSTANTIATES" if !file.line_notes.is_empty() => {
                    let Some(line) = edge.props.get("line").and_then(|v| v.as_u64()) else { continue };
                    if let Ok(at) = file.line_notes.binary_search_by_key(&(line as u32), |note| note.line) {
                        Heard::add(&mut heard.entry(edge.to.as_str()).or_default().callers, file.line_notes[at].text.as_str());
                        reached.push(edge.to.as_str());
                    }
                }
                "TESTS" => {
                    if let Some(qname) = qname_in(&edge.from, file) {
                        Heard::add(&mut heard.entry(edge.to.as_str()).or_default().tests, qname);
                        reached.push(edge.to.as_str());
                    }
                }
                "MENTIONS" => {
                    let section = qname_in(&edge.from, file).and_then(|qname| file.symbols.iter().find(|s| s.qname == qname));
                    if let Some(section) = section {
                        Heard::add(&mut heard.entry(edge.to.as_str()).or_default().docs, section.signature.as_str());
                        reached.push(edge.to.as_str());
                    }
                }
                _ => {}
            }
        }
        if !reached.is_empty() {
            reached.sort_unstable();
            reached.dedup();
            reach.insert(path.as_str(), reached);
        }
    }
    (heard, reach)
}

/// The definition a sid names, when the model has it.
pub fn lookup<'m>(model: &'m Model, sid: &str) -> Option<&'m SymbolFact> {
    let (_, rest) = sid.split_once(':')?;
    let (path, _) = rest.split_once('#')?;
    let file = model.files.get(path)?;
    let qname = qname_in(sid, file)?;
    file.symbols.iter().find(|s| s.qname == qname)
}

/// Identifier words, lowercase: `evict_expired`, `evictExpired`, `EvictExpired` -> evict, expired.
fn words(name: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut word = String::new();
    let mut prev_lower = false;
    for ch in name.chars() {
        if !ch.is_alphanumeric() {
            if !word.is_empty() {
                out.push(std::mem::take(&mut word));
            }
            prev_lower = false;
            continue;
        }
        if ch.is_uppercase() && prev_lower && !word.is_empty() {
            out.push(std::mem::take(&mut word));
        }
        prev_lower = ch.is_lowercase() || ch.is_ascii_digit();
        word.extend(ch.to_lowercase());
    }
    if !word.is_empty() {
        out.push(word);
    }
    out
}

/// What a test's name says about `target`: its words without the test marker and the target's own
/// name (`test_evict_expired_keeps_fresh_entries` for `evict_expired` -> "keeps fresh entries").
fn test_phrase(test_qname: &str, target: &str) -> Option<String> {
    let last = test_qname.rsplit(['.', ':', '/']).find(|part| !part.is_empty()).unwrap_or(test_qname);
    let own = words(target);
    let kept: Vec<String> = words(last).into_iter().filter(|w| w != "test" && w != "tests" && !own.contains(w)).collect();
    (kept.len() >= 2).then(|| kept.join(" "))
}

/// The first sentence of a doc comment: its first line of prose, outside code fences.
fn first_sentence(doc: &str) -> &str {
    let mut fenced = false;
    let line = doc
        .lines()
        .map(str::trim)
        .find(|line| {
            if line.starts_with("```") || line.starts_with("~~~") {
                fenced = !fenced;
                return false;
            }
            !fenced && !line.is_empty()
        })
        .unwrap_or("");
    match line.find(". ") {
        Some(at) => &line[..=at],
        None => line,
    }
}

/// One definition's summary: the doc's first sentence first (what one-line results show), then
/// what callers, tests and documents say, then its body's own words. None when nothing is said.
pub fn compose(symbol: &SymbolFact, heard: Option<&Heard>) -> Option<String> {
    let mut lines: Vec<String> = Vec::new();
    if let Some(doc) = symbol.doc.as_deref() {
        let sentence = first_sentence(doc);
        if !sentence.is_empty() {
            lines.push(sentence.to_owned());
        }
    }
    if let Some(heard) = heard {
        if !heard.callers.is_empty() {
            lines.push(format!("Callers: {}", heard.callers.join("; ")));
        }
        let tests: Vec<String> = heard.tests.iter().filter_map(|test| test_phrase(test, &symbol.name)).collect();
        if !tests.is_empty() {
            lines.push(format!("Tests: {}", tests.join("; ")));
        }
        if !heard.docs.is_empty() {
            lines.push(format!("Docs: {}", heard.docs.join("; ")));
        }
    }
    if !symbol.notes.is_empty() {
        lines.push(format!("Says: {}", symbol.notes.join(" | ")));
    }
    if lines.is_empty() {
        return None;
    }
    let text = lines.join("\n");
    Some(match text.char_indices().nth(SUMMARY_CHARS) {
        Some((at, _)) => format!("{}…", text[..at].trim_end()),
        None => text,
    })
}

/// Short stable hash of a text or a sid, as stored and compared (`summary_hash`).
pub fn hash64(text: &str) -> u64 {
    let hash = blake3::hash(text.as_bytes());
    u64::from_le_bytes(hash.as_bytes()[..8].try_into().unwrap_or_default())
}
