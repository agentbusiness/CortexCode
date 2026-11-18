//! Hashline: content-addressed line anchors shared by every component that shows or edits code.
//!
//! A line is rendered as `N:hhhh|text`, where `N` is the 1-based line number and `hhhh` a
//! four-character tag derived from the exact line content (without its line terminator). An edit
//! names lines by anchor (`N:hhhh`); if the file changed since the model read it, the tag no longer
//! matches and the edit is rejected before anything is written. This module is the single
//! definition of the tag function and the anchor syntax; the tools and memory plugins must both use
//! it.
//!
//! Four characters are 20 bits, about a million values: a tag that is not the line's own names
//! another line of a 1000-line file once in a thousand times, so a line that moved can be found
//! again by its tag alone, and a tag the model made up almost never names a line. (Two characters,
//! 1024 values, named exactly one other line of a 300-line file one time in five.)

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

/// Characters of a tag.
pub const TAG_LEN: usize = 4;

/// Characters of the tags shown before tags grew to [`TAG_LEN`]: an anchor of that form still
/// reads as an anchor (see [`parse_old_anchor`]).
const OLD_TAG_LEN: usize = 2;

/// The tag of one line (content without `\n` / `\r\n`) as its [`TAG_LEN`] alphabet bytes.
pub fn tag_bytes(line: &str) -> [u8; TAG_LEN] {
    let line = line.strip_suffix('\r').unwrap_or(line);
    let hash = fnv1a(line.as_bytes());
    // Fold the high 12 bits into the low 20, so the tag depends on the whole hash; then five bits
    // per character, lowest first.
    let folded = hash ^ (hash >> 20);
    let mut out = [0u8; TAG_LEN];
    for (index, byte) in out.iter_mut().enumerate() {
        *byte = ALPHABET[((folded >> (5 * index)) & 31) as usize];
    }
    out
}

/// The four-character tag of one line (content without `\n` / `\r\n`).
pub fn tag(line: &str) -> String {
    tag_bytes(line).iter().map(|byte| *byte as char).collect()
}

/// Render one line as `N:hhhh|text`.
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

/// A parsed anchor `N:hhhh`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Anchor {
    pub line: usize,
    pub tag: [u8; TAG_LEN],
}

impl Anchor {
    /// The anchor of 1-based line `line` whose content is `content`.
    pub fn of(line: usize, content: &str) -> Anchor {
        Anchor { line, tag: tag_bytes(content) }
    }

    pub fn tag_str(&self) -> String {
        String::from_utf8_lossy(&self.tag).into_owned()
    }
}

impl std::fmt::Display for Anchor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}", self.line, self.tag_str())
    }
}

/// The line number and tag bytes of `N:<tag>` (also from a full rendered line `N:<tag>|text`), when
/// the tag has `len` alphabet characters.
fn parse_tagged(text: &str, len: usize) -> Option<(usize, &[u8])> {
    let text = text.trim();
    let text = text.split('|').next().unwrap_or(text);
    let (number, tag) = text.split_once(':')?;
    let line: usize = number.trim().parse().ok()?;
    let tag = tag.trim().as_bytes();
    if line == 0 || tag.len() != len || !tag.iter().all(|byte| ALPHABET.contains(byte)) {
        return None;
    }
    Some((line, tag))
}

/// Parse `N:hhhh`, also accepting a full rendered line `N:hhhh|text` (models often copy the whole
/// line).
pub fn parse_anchor(text: &str) -> Option<Anchor> {
    let (line, bytes) = parse_tagged(text, TAG_LEN)?;
    let mut tag = [0u8; TAG_LEN];
    tag.copy_from_slice(bytes);
    Some(Anchor { line, tag })
}

/// The line number of an anchor in the two-character form shown before tags had [`TAG_LEN`]
/// characters (`N:hh`, kept in older conversations). It names no line any more: whoever takes
/// anchors treats it as stale and shows the lines there with their anchors, instead of calling it
/// malformed.
pub fn parse_old_anchor(text: &str) -> Option<usize> {
    parse_tagged(text, OLD_TAG_LEN).map(|(line, _)| line)
}

/// Check an anchor against the current file lines. `lines` are the file split on `\n`.
pub fn anchor_matches(anchor: &Anchor, lines: &[&str]) -> bool {
    lines.get(anchor.line - 1).is_some_and(|line| tag(line).as_bytes() == anchor.tag)
}
