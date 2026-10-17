//! `read` for files and directories: hashline-rendered text, line ranges, structural outlines for
//! large files, directory listings and binary detection.

use crate::doc::Doc;
use crate::lang::{self, Lang};
use crate::util;
use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::path::Path;

#[derive(Debug, Clone)]
pub struct ReadConfig {
    /// Files longer than this (lines) are shown as an outline when read without a range.
    pub outline_lines: usize,
    /// Files bigger than this (bytes) are shown as an outline when read without a range.
    pub outline_bytes: usize,
}

impl Default for ReadConfig {
    fn default() -> Self {
        Self { outline_lines: 600, outline_bytes: 64 * 1024 }
    }
}

/// One segment of a range: 1-based inclusive start, end (None = open-ended).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Segment {
    pub start: usize,
    pub end: Option<usize>,
}

/// Parse `50`, `50-200`, `50+30`, `50-` and comma-separated lists (`5-16,40-52`).
pub fn parse_range(text: &str) -> Result<Vec<Segment>, String> {
    let text = text.trim().trim_start_matches(':');
    if text.is_empty() {
        return Err("empty range".to_owned());
    }
    let mut segments = Vec::new();
    for part in text.split(',') {
        let part = part.trim();
        let number = |s: &str| -> Result<usize, String> {
            s.trim().parse::<usize>().map_err(|_| format!("`{part}` is not a line range (use 50, 50-80, 50+30 or 5-16,40-52)"))
        };
        let segment = if let Some((start, count)) = part.split_once('+') {
            let start = number(start)?;
            let count = number(count)?;
            if count == 0 {
                return Err(format!("`{part}`: a count of 0 lines selects nothing"));
            }
            Segment { start, end: Some(start + count - 1) }
        } else if let Some((start, end)) = part.split_once('-') {
            let start = number(start)?;
            if end.trim().is_empty() { Segment { start, end: None } } else { Segment { start, end: Some(number(end)?) } }
        } else {
            Segment { start: number(part)?, end: None }
        };
        if segment.start == 0 {
            return Err(format!("`{part}`: lines are numbered from 1"));
        }
        if let Some(end) = segment.end
            && end < segment.start
        {
            return Err(format!("`{part}`: the range ends before it starts"));
        }
        segments.push(segment);
    }
    Ok(segments)
}

/// Split `path:range` when the suffix is valid range syntax (structural: digits, `-`, `+`, `,`).
pub fn split_path_range(target: &str) -> (String, Option<String>) {
    if let Some((path, suffix)) = target.rsplit_once(':')
        && !path.is_empty()
        && !suffix.is_empty()
        && suffix.bytes().all(|b| b.is_ascii_digit() || b"-+, ".contains(&b))
        && suffix.bytes().next().is_some_and(|b| b.is_ascii_digit())
        && parse_range(suffix).is_ok()
    {
        return (path.to_owned(), Some(suffix.to_owned()));
    }
    (target.to_owned(), None)
}

#[derive(Debug, Clone)]
pub struct FileView {
    pub text: String,
    pub total_lines: usize,
    /// Content hash of the version read (for read state); None for directories/binary.
    pub hash: Option<String>,
    pub lines: Vec<String>,
    /// `file` | `range` | `outline` | `dir` | `binary` | `empty`
    pub mode: &'static str,
}

#[derive(Debug, Clone)]
pub struct ReadError {
    pub kind: &'static str,
    pub message: String,
}

impl ReadError {
    fn new(kind: &'static str, message: impl Into<String>) -> Self {
        Self { kind, message: message.into() }
    }
}

/// Recognize binary content by magic numbers (structural) for a helpful description.
fn describe_binary(bytes: &[u8]) -> &'static str {
    let starts = |magic: &[u8]| bytes.starts_with(magic);
    if starts(b"\x89PNG") {
        "PNG image"
    } else if starts(b"\xff\xd8\xff") {
        "JPEG image"
    } else if starts(b"GIF8") {
        "GIF image"
    } else if starts(b"%PDF") {
        "PDF document"
    } else if starts(b"PK\x03\x04") {
        "ZIP archive (or Office/JAR file)"
    } else if starts(b"\x1f\x8b") {
        "gzip archive"
    } else if starts(b"\x7fELF") {
        "ELF executable"
    } else if starts(b"\xcf\xfa\xed\xfe") || starts(b"\xca\xfe\xba\xbe") {
        "Mach-O binary"
    } else if starts(b"SQLite format 3\0") {
        "SQLite database"
    } else if starts(b"\0asm") {
        "WebAssembly module"
    } else if starts(b"RIFF") {
        "RIFF media file"
    } else {
        "binary data"
    }
}

pub fn is_binary(bytes: &[u8]) -> bool {
    bytes.iter().take(8192).any(|b| *b == 0) || std::str::from_utf8(bytes).is_err()
}

pub fn read_file(path: &Path, display: &str, range: Option<&str>, config: &ReadConfig) -> Result<FileView, ReadError> {
    let metadata = std::fs::metadata(path).map_err(|error| match error.kind() {
        std::io::ErrorKind::NotFound => ReadError::new("not_found", format!("{display}: no such file.")),
        _ => ReadError::new("io_error", format!("{display}: {error}")),
    })?;
    if metadata.is_dir() {
        if range.is_some() {
            return Err(ReadError::new("bad_range", format!("{display} is a directory; ranges apply to files")));
        }
        return list_dir(path, display);
    }
    let bytes = std::fs::read(path).map_err(|error| ReadError::new("io_error", format!("{display}: {error}")))?;
    if is_binary(&bytes) {
        let what = describe_binary(&bytes);
        return Ok(FileView {
            text: format!("{display}: binary file ({what}). Inspect it with run (e.g. `file {display}`).\n"),
            total_lines: 0,
            hash: None,
            lines: Vec::new(),
            mode: "binary",
        });
    }
    let text = String::from_utf8_lossy(&bytes).into_owned();
    let hash = util::content_hash(&bytes);
    let doc = Doc::parse(&text);
    let total = doc.len();
    if total == 0 {
        return Ok(FileView {
            text: format!("{display}: empty file. To fill it, send edit `content`.\n"),
            total_lines: 0,
            hash: Some(hash),
            lines: Vec::new(),
            mode: "empty",
        });
    }
    let lines = doc.lines.clone();
    let view =
        |text: String, mode: &'static str| FileView { text, total_lines: total, hash: Some(hash.clone()), lines: lines.clone(), mode };

    if let Some(range) = range {
        let segments = parse_range(range).map_err(|message| ReadError::new("bad_range", format!("{display}: {message}")))?;
        let mut out = String::new();
        let mut shown_any = false;
        for (index, segment) in segments.iter().enumerate() {
            if segment.start > total {
                let _ = writeln!(out, "[{display} has {total} lines; line {} is past the end]", segment.start);
                continue;
            }
            let (end, continued) = match segment.end {
                Some(end) => (end.min(total), None),
                None => {
                    let end = (segment.start + config.outline_lines - 1).min(total);
                    (end, (end < total).then_some(end + 1))
                }
            };
            if index == 0 {
                let _ = writeln!(out, "{display}:{}-{end} (of {total} lines):", segment.start);
            } else {
                let _ = writeln!(out, "...\n{display}:{}-{end}:", segment.start);
            }
            out.push_str(&doc.render(segment.start, end));
            shown_any = true;
            if let Some(next) = continued {
                let _ = writeln!(out, "[more: read {display}:{next}]");
            } else if end == total {
                // Says the view is complete to the end: no reason to doubt it or read it again.
                let _ = writeln!(out, "[end of file]");
            }
        }
        if !shown_any {
            return Err(ReadError::new("bad_range", out.trim_end().to_owned()));
        }
        return Ok(view(out, "range"));
    }

    if total <= config.outline_lines && bytes.len() <= config.outline_bytes {
        let mut out = format!("{display}:\n");
        out.push_str(&doc.render(1, total));
        // The whole file, as it is on disk now: the current, complete view.
        let _ = writeln!(out, "[end of file: the whole file as it is now]");
        return Ok(view(out, "file"));
    }

    Ok(view(render_outline(path, display, &text, &doc, true), "outline"))
}

/// Outline view of a large file: definitions with spans, bodies elided. With `hint`, the header ends
/// with how to see the left-out lines (a range example: the body of the first definition shown).
pub fn render_outline(path: &Path, display: &str, text: &str, doc: &Doc, hint: bool) -> String {
    let total = doc.len();
    let mut shown = BTreeSet::new();
    let mut how = "top-level lines";
    if let Some(lang) = Lang::detect(path, text)
        && let Some(items) = lang::outline(lang, text)
    {
        lang::shown_rows(&items, &doc.lines, &mut shown);
        how = "definitions";
    }
    if shown.is_empty() {
        shown = lang::indentation_rows(&doc.lines);
    }
    // Walk the shown rows; a gap is marked only when it holds something other than blank lines.
    let hidden = |from: usize, to: usize| from < to && !doc.lines[from..to].iter().all(|line| line.trim().is_empty());
    let mut body = String::new();
    // The first shown line whose body is left out: (its row, the next shown row).
    let mut example: Option<(usize, usize)> = None;
    let rows: Vec<usize> = shown.into_iter().filter(|row| *row < total).collect();
    let mut next = 0usize;
    // The first row of the run of shown rows the current one belongs to (a definition's header).
    let mut run_start = 0usize;
    for (index, row) in rows.iter().enumerate() {
        if hidden(next, *row) {
            let _ = writeln!(body, "   ⋮ {}-{}", next + 1, row);
        }
        if index == 0 || *row != next {
            run_start = *row;
        }
        body.push_str(&doc.render(row + 1, row + 1));
        next = row + 1;
        let following = rows.get(index + 1).copied().unwrap_or(total);
        if example.is_none() && hidden(next, following) {
            example = Some((run_start, following));
        }
    }
    if hidden(next, total) {
        let _ = writeln!(body, "   ⋮ {}-{}", next + 1, total);
    }
    let mut out = format!("{display}: {total} lines; its {how} are shown, ⋮ N-M marks lines left out.");
    if hint {
        let example = match example {
            Some((row, following)) => format!("{display}:{}-{following}", row + 1),
            None => format!("{display}:1-{}", total.min(100)),
        };
        let _ = write!(out, " Read a range to see them, e.g. {example}.");
    }
    out.push('\n');
    out.push_str(&body);
    out
}

/// The entries of a directory a listing shows: `.git` and what the ignore rules (.gitignore etc.)
/// exclude are left out; directories end in `/`. Sorted: directories first, then by name.
/// The second list holds the ignored entries.
pub fn visible_entries(path: &Path) -> std::io::Result<(Vec<String>, Vec<String>)> {
    let mut all: Vec<(String, bool)> = Vec::new();
    for entry in std::fs::read_dir(path)?.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name == ".git" {
            continue;
        }
        let is_dir = std::fs::metadata(entry.path()).or_else(|_| entry.metadata()).map(|meta| meta.is_dir()).unwrap_or(false);
        all.push((name, is_dir));
    }
    let mut visible = std::collections::HashSet::new();
    let walker = ignore::WalkBuilder::new(path).max_depth(Some(1)).hidden(false).parents(true).build();
    for entry in walker.flatten() {
        if entry.depth() == 1 {
            visible.insert(entry.file_name().to_string_lossy().into_owned());
        }
    }
    all.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.to_lowercase().cmp(&b.0.to_lowercase())));
    let (mut shown, mut ignored) = (Vec::new(), Vec::new());
    for (name, is_dir) in all {
        let entry = if is_dir { format!("{name}/") } else { name.clone() };
        if visible.contains(&name) { shown.push(entry) } else { ignored.push(entry) }
    }
    Ok((shown, ignored))
}

fn list_dir(path: &Path, display: &str) -> Result<FileView, ReadError> {
    let (shown, ignored) = visible_entries(path).map_err(|error| ReadError::new("io_error", format!("{display}: {error}")))?;
    let mut out = format!("{}:\n", dir_label(display));
    for name in &shown {
        let _ = writeln!(out, "{name}");
    }
    if !ignored.is_empty() {
        let _ = writeln!(out, "ignored by .gitignore: {}", ignored.join(", "));
    }
    if shown.is_empty() && ignored.is_empty() {
        out.push_str("(empty directory)\n");
    }
    Ok(FileView { text: out, total_lines: shown.len() + ignored.len(), hash: None, lines: Vec::new(), mode: "dir" })
}

/// A directory as results name it: `src/`.
pub fn dir_label(display: &str) -> String {
    if display.ends_with('/') { display.to_owned() } else { format!("{display}/") }
}
