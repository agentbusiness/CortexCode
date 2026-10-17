//! Positions and document URIs.
//!
//! CortexCode positions are 1-based lines and 1-based columns counted in characters (Unicode scalar
//! values), with lines split at `\n` like the hashline line store. LSP positions are 0-based lines
//! and 0-based columns counted in UTF-16 code units (the protocol's mandatory encoding). A `\r`
//! ending a line belongs to the line break, not to any column.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Position {
    pub line: u32,
    pub character: u32,
}

/// The text of 0-based line `index` without its line break, None past the end.
pub fn line_text(text: &str, index: usize) -> Option<&str> {
    let line = text.split('\n').nth(index)?;
    Some(line.strip_suffix('\r').unwrap_or(line))
}

/// UTF-16 code units of the first `chars` characters of `line` (clamped to the line).
pub fn chars_to_utf16(line: &str, chars: u32) -> u32 {
    line.chars().take(chars as usize).map(|c| c.len_utf16() as u32).sum()
}

/// Characters before UTF-16 offset `units` of `line` (an offset inside a surrogate pair rounds up
/// past that character; past the end clamps to the line's length).
pub fn utf16_to_chars(line: &str, units: u32) -> u32 {
    let mut seen = 0u32;
    let mut chars = 0u32;
    for c in line.chars() {
        if seen >= units {
            break;
        }
        seen += c.len_utf16() as u32;
        chars += 1;
    }
    chars
}

/// The LSP position of 1-based `line`, `column` (characters) in `text`. Lines past the end keep
/// their number (the server answers for them as it sees fit); columns clamp to the line.
pub fn to_lsp(text: &str, line: u32, column: u32) -> Position {
    let index = line.saturating_sub(1);
    let chars = column.saturating_sub(1);
    let character = match line_text(text, index as usize) {
        Some(content) => chars_to_utf16(content, chars),
        None => 0,
    };
    Position { line: index, character }
}

/// The 1-based (line, column in characters) of an LSP position in `text`; without the text (a file
/// that cannot be read), UTF-16 units are taken as characters.
pub fn from_lsp(text: Option<&str>, position: Position) -> (u32, u32) {
    let column = match text.and_then(|t| line_text(t, position.line as usize)) {
        Some(content) => utf16_to_chars(content, position.character),
        None => position.character,
    };
    (position.line + 1, column + 1)
}

/// An LSP position from JSON (`{"line": .., "character": ..}`).
pub fn parse_position(value: &Value) -> Option<Position> {
    Some(Position { line: value.get("line")?.as_u64()? as u32, character: value.get("character")?.as_u64()? as u32 })
}

/// `file://` URI of an absolute path (percent-encoded).
pub fn path_to_uri(path: &Path) -> Option<String> {
    url::Url::from_file_path(path).ok().map(|url| url.to_string())
}

/// The path of a `file://` URI.
pub fn uri_to_path(uri: &str) -> Option<PathBuf> {
    url::Url::parse(uri).ok()?.to_file_path().ok()
}

fn same_ignoring_case(a: &std::ffi::OsStr, b: &std::ffi::OsStr) -> bool {
    a == b || a.to_string_lossy().to_lowercase() == b.to_string_lossy().to_lowercase()
}

/// A path a server reported, spelled the way the workspace spells it. macOS's file systems are
/// case-insensitive, and a server may report a workspace file in another letter case (TypeScript
/// 7's server lower-cases paths), which would match neither the documents this client opened nor
/// the root it reports paths relative to. On macOS, see [`fold_case`]; elsewhere the path as it is.
pub fn workspace_case(root: &Path, path: &Path) -> PathBuf {
    if cfg!(target_os = "macos") { fold_case(root, path) } else { path.to_path_buf() }
}

/// `path` under `root` compared ignoring case, re-spelled: the root as `root` spells it, every
/// further component as the directory entry it names (the entry spelled exactly so first, else the
/// one equal ignoring case). A path that keeps the root's exact spelling, or is not under the root,
/// comes back as it is (a server that does not change the root's case keeps the rest too).
pub fn fold_case(root: &Path, path: &Path) -> PathBuf {
    let root_parts: Vec<_> = root.components().collect();
    let parts: Vec<_> = path.components().collect();
    if path.starts_with(root) || parts.len() < root_parts.len() || !root_parts.iter().zip(&parts).all(|(a, b)| same_ignoring_case(a.as_os_str(), b.as_os_str())) {
        return path.to_path_buf();
    }
    let mut out = root.to_path_buf();
    for part in &parts[root_parts.len()..] {
        let name = part.as_os_str();
        let entries: Vec<std::ffi::OsString> = std::fs::read_dir(&out).map(|dir| dir.flatten().map(|entry| entry.file_name()).collect()).unwrap_or_default();
        let spelled = entries.iter().find(|entry| entry.as_os_str() == name).or_else(|| entries.iter().find(|entry| same_ignoring_case(entry, name)));
        out.push(spelled.map(|entry| entry.as_os_str()).unwrap_or(name));
    }
    out
}
