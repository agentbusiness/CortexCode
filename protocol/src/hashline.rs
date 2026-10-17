//! Hashline: content-addressed line anchors shared by every component that shows or edits code.
//!
//! A line is rendered as `N:hh|text`, where `N` is the 1-based line number and `hh` a two-character
//! tag derived from the exact line content (without its line terminator). An edit names lines by
//! anchor (`N:hh`); if the file changed since the model read it, the tag no longer matches and the
//! edit is rejected before anything is written. This module is the single definition of the tag
//! function and the anchor syntax; the tools and memory plugins must both use it.

/// Alphabet without easily confused characters (no i, l, o, u).
const ALPHABET: &[u8; 32] = b"0123456789abcdefghjkmnpqrstvwxyz";

/// FNV-1a 32-bit over the line bytes.
fn fnv1a(bytes: &[u8]) -> u32 {
    let mut hash: u32 = 0x811c_9dc5;
    for byte in bytes {
        hash ^= u32::from(*byte);
        hash = hash.wrapping_mul(0x0100_0193);
    }
    hash
}

/// The two-character tag of one line (content without `\n` / `\r\n`).
pub fn tag(line: &str) -> String {
    let line = line.strip_suffix('\r').unwrap_or(line);
    let hash = fnv1a(line.as_bytes());
    // Fold the high bits in so the tag depends on the whole hash, not only the low 10 bits.
    let folded = hash ^ (hash >> 10) ^ (hash >> 20) ^ (hash >> 30);
    let first = ALPHABET[(folded & 31) as usize] as char;
    let second = ALPHABET[((folded >> 5) & 31) as usize] as char;
    let mut out = String::with_capacity(2);
    out.push(first);
    out.push(second);
    out
}

/// Render one line as `N:hh|text`.
pub fn render_line(number: usize, line: &str) -> String {
    let content = line.strip_suffix('\r').unwrap_or(line);
    format!("{number}:{}|{content}", tag(content))
}

/// Render `text` (whole file) between 1-based inclusive `start..=end`, clamped to the file.
pub fn render(text: &str, start: usize, end: usize) -> String {
    let lines = file_lines(text);
    let mut out = String::new();
    for (index, line) in lines.iter().enumerate() {
        let number = index + 1;
        if number < start {
            continue;
        }
        if number > end {
            break;
        }
        out.push_str(&render_line(number, line));
        out.push('\n');
    }
    out
}

/// The lines of a file: split on `\n`, without the empty element a trailing newline produces.
pub fn file_lines(text: &str) -> Vec<&str> {
    let mut lines: Vec<&str> = text.split('\n').collect();
    if text.ends_with('\n') {
        lines.pop();
    }
    lines
}

/// A parsed anchor `N:hh`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Anchor {
    pub line: usize,
    pub tag: [u8; 2],
}

impl Anchor {
    pub fn tag_str(&self) -> String {
        String::from_utf8_lossy(&self.tag).into_owned()
    }
}

impl std::fmt::Display for Anchor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}", self.line, self.tag_str())
    }
}

/// Parse `N:hh`, also accepting a full rendered line `N:hh|text` (models often copy the whole line).
pub fn parse_anchor(text: &str) -> Option<Anchor> {
    let text = text.trim();
    let text = text.split('|').next().unwrap_or(text);
    let (number, tag) = text.split_once(':')?;
    let line: usize = number.trim().parse().ok()?;
    let tag = tag.trim().as_bytes();
    if line == 0 || tag.len() != 2 || !tag.iter().all(|byte| ALPHABET.contains(byte)) {
        return None;
    }
    Some(Anchor { line, tag: [tag[0], tag[1]] })
}

/// Check an anchor against the current file lines. `lines` are the file split on `\n`.
pub fn anchor_matches(anchor: &Anchor, lines: &[&str]) -> bool {
    lines.get(anchor.line - 1).is_some_and(|line| tag(line).as_bytes() == anchor.tag)
}
