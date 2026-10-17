//! Parser for git's unified patch output (`git diff -p` with explicit `a/` and `b/` prefixes).
//!
//! The parser is purely structural: it follows the patch grammar (file headers, `@@` hunk headers
//! with line counts, ` `/`+`/`-` body lines and `\` no-newline markers). It works on bytes, keeps
//! the exact bytes of every file header and hunk (so one hunk can be re-applied with `git apply`),
//! and decodes paths and text for the contract only at the edge.

use cc_protocol::contracts::vcs::{DiffLine, FileDiff, Hunk};

/// One hunk with its exact patch bytes (`@@` line through the last body line, newline-terminated).
#[derive(Debug, Clone)]
pub struct ParsedHunk {
    pub hunk: Hunk,
    pub raw: Vec<u8>,
}

/// One file of a patch.
#[derive(Debug, Clone)]
pub struct ParsedFile {
    pub path: String,
    pub old_path: Option<String>,
    /// added | modified | deleted | renamed | conflicted
    pub status: String,
    pub binary: bool,
    /// Exact header bytes (`diff --git` line up to, excluding, the first hunk).
    pub header: Vec<u8>,
    /// The `---` and `+++` names exactly as printed (possibly C-quoted), without trailing tab.
    pub minus_name: Option<Vec<u8>>,
    pub plus_name: Option<Vec<u8>>,
    pub hunks: Vec<ParsedHunk>,
}

impl ParsedFile {
    pub fn to_contract(&self) -> FileDiff {
        let mut added = 0;
        let mut removed = 0;
        for hunk in &self.hunks {
            for line in &hunk.hunk.lines {
                match line.kind.as_str() {
                    "+" => added += 1,
                    "-" => removed += 1,
                    _ => {}
                }
            }
        }
        FileDiff {
            path: self.path.clone(),
            old_path: self.old_path.clone(),
            status: self.status.clone(),
            binary: self.binary,
            hunks: self.hunks.iter().map(|h| h.hunk.clone()).collect(),
            added,
            removed,
        }
    }

    /// A patch containing only hunk `index` of this file, applicable with `git apply`.
    ///
    /// For renames and copies the header is rewritten to a plain modification of the new path, so
    /// applying (or reverse-applying) one hunk changes content only and never undoes the rename.
    pub fn hunk_patch(&self, index: usize) -> Option<Vec<u8>> {
        let hunk = self.hunks.get(index)?;
        let mut patch = Vec::new();
        if self.old_path.is_some() {
            let plus = self.plus_name.as_ref()?;
            let minus = rename_prefix(plus, b"b/", b"a/")?;
            patch.extend_from_slice(b"diff --git ");
            patch.extend_from_slice(&minus);
            patch.push(b' ');
            patch.extend_from_slice(plus);
            // git terminates `---`/`+++` names containing a space with a tab; mirror that.
            let tab: &[u8] = if plus.contains(&b' ') { b"\t" } else { b"" };
            patch.extend_from_slice(b"\n--- ");
            patch.extend_from_slice(&minus);
            patch.extend_from_slice(tab);
            patch.extend_from_slice(b"\n+++ ");
            patch.extend_from_slice(plus);
            patch.extend_from_slice(tab);
            patch.push(b'\n');
        } else {
            patch.extend_from_slice(&self.header);
        }
        patch.extend_from_slice(&hunk.raw);
        Some(patch)
    }
}

/// Replace the `from` prefix of a (possibly quoted) patch name with `to`.
fn rename_prefix(name: &[u8], from: &[u8], to: &[u8]) -> Option<Vec<u8>> {
    let quoted = name.first() == Some(&b'"');
    let body = if quoted { &name[1..] } else { name };
    let rest = body.strip_prefix(from)?;
    let mut out = Vec::with_capacity(name.len());
    if quoted {
        out.push(b'"');
    }
    out.extend_from_slice(to);
    out.extend_from_slice(rest);
    Some(out)
}

/// Undo git's C-style path quoting (`quote_c_style`): `"a\tb\303\251"` -> bytes. Unquoted input is
/// returned as is. Returns the decoded bytes and the number of input bytes consumed.
pub fn unquote(input: &[u8]) -> (Vec<u8>, usize) {
    if input.first() != Some(&b'"') {
        return (input.to_vec(), input.len());
    }
    let mut out = Vec::new();
    let mut i = 1;
    while i < input.len() {
        match input[i] {
            b'"' => return (out, i + 1),
            b'\\' if i + 1 < input.len() => {
                let next = input[i + 1];
                let simple = match next {
                    b'a' => Some(7),
                    b'b' => Some(8),
                    b't' => Some(b'\t'),
                    b'n' => Some(b'\n'),
                    b'v' => Some(11),
                    b'f' => Some(12),
                    b'r' => Some(b'\r'),
                    b'"' => Some(b'"'),
                    b'\\' => Some(b'\\'),
                    _ => None,
                };
                if let Some(byte) = simple {
                    out.push(byte);
                    i += 2;
                } else if next.is_ascii_digit() && i + 3 < input.len() {
                    let digits = &input[i + 1..i + 4];
                    let value = digits.iter().try_fold(0u32, |acc, d| {
                        (b'0'..=b'7').contains(d).then(|| acc * 8 + u32::from(d - b'0'))
                    });
                    match value {
                        Some(v) if v <= 255 => {
                            out.push(v as u8);
                            i += 4;
                        }
                        _ => {
                            out.push(next);
                            i += 2;
                        }
                    }
                } else {
                    out.push(next);
                    i += 2;
                }
            }
            byte => {
                out.push(byte);
                i += 1;
            }
        }
    }
    (out, input.len())
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// A `---`/`+++` name: drop the trailing tab git adds to names containing spaces.
fn header_name(rest: &[u8]) -> Vec<u8> {
    let trimmed = rest.strip_suffix(b"\t").unwrap_or(rest);
    trimmed.to_vec()
}

/// Decode a `---`/`+++` name to a path without its `a/`/`b/` prefix; `None` for `/dev/null`.
fn name_to_path(name: &[u8], prefix: &[u8]) -> Option<Vec<u8>> {
    if name == b"/dev/null" {
        return None;
    }
    let (decoded, _) = unquote(name);
    Some(decoded.strip_prefix(prefix).map(<[u8]>::to_vec).unwrap_or(decoded))
}

/// Paths from a `diff --git <a> <b>` line when no other header names them (binary or mode-only
/// changes). Without quoting the two names are equal (no rename, which would have its own header),
/// so the split point follows from the length: `a/P b/P`.
fn paths_from_git_line(rest: &[u8]) -> (Vec<u8>, Vec<u8>) {
    if rest.first() == Some(&b'"') {
        let (a, used) = unquote(rest);
        let tail = rest.get(used..).unwrap_or_default();
        let tail = tail.strip_prefix(b" ").unwrap_or(tail);
        let (b, _) = unquote(tail);
        return (strip(&a, b"a/"), strip(&b, b"b/"));
    }
    if rest.len() >= 5 && (rest.len() - 5).is_multiple_of(2) {
        let n = (rest.len() - 5) / 2;
        let a = &rest[..2 + n];
        let b = &rest[3 + n..];
        if a.strip_prefix(b"a/") == b.strip_prefix(b"b/") {
            return (strip(a, b"a/"), strip(b, b"b/"));
        }
    }
    // Mixed quoting (only the second name quoted) or an unexpected shape: split at the last " b/".
    let split = rest.windows(3).rposition(|w| w == b" b/" || w == b" \"b");
    match split {
        Some(at) => {
            let a = &rest[..at];
            let (b, _) = unquote(&rest[at + 1..]);
            (strip(a, b"a/"), strip(&b, b"b/"))
        }
        None => (rest.to_vec(), rest.to_vec()),
    }
}

fn strip(name: &[u8], prefix: &[u8]) -> Vec<u8> {
    name.strip_prefix(prefix).map(<[u8]>::to_vec).unwrap_or_else(|| name.to_vec())
}

/// Parse `@@ -a[,b] +c[,d] @@...` into (a, b, c, d).
fn parse_hunk_header(line: &[u8]) -> Option<(u32, u32, u32, u32)> {
    let rest = line.strip_prefix(b"@@ -")?;
    let end = rest.windows(3).position(|w| w == b" @@")?;
    let ranges = std::str::from_utf8(&rest[..end]).ok()?;
    let (old, new) = ranges.split_once(" +")?;
    let range = |spec: &str| -> Option<(u32, u32)> {
        match spec.split_once(',') {
            Some((start, count)) => Some((start.parse().ok()?, count.parse().ok()?)),
            None => Some((spec.parse().ok()?, 1)),
        }
    };
    let (a, b) = range(old)?;
    let (c, d) = range(new)?;
    Some((a, b, c, d))
}

/// Split into lines; each item is the line without its `\n` (a final line without `\n` is kept).
fn lines(data: &[u8]) -> Vec<&[u8]> {
    let mut out: Vec<&[u8]> = data.split(|b| *b == b'\n').collect();
    if out.last().is_some_and(|l| l.is_empty()) {
        out.pop();
    }
    out
}

fn push_line(buffer: &mut Vec<u8>, line: &[u8]) {
    buffer.extend_from_slice(line);
    buffer.push(b'\n');
}

/// Parse a whole patch.
pub fn parse_patch(data: &[u8]) -> Vec<ParsedFile> {
    let all = lines(data);
    let mut files = Vec::new();
    let mut i = 0;
    while i < all.len() {
        let line = all[i];
        if let Some(rest) = line.strip_prefix(b"diff --git ") {
            let (file, next) = parse_git_file(&all, i, rest);
            files.push(file);
            i = next;
        } else if let Some(rest) = line.strip_prefix(b"diff --cc ").or_else(|| line.strip_prefix(b"diff --combined ")) {
            // A combined diff of an unmerged path. Its body lines carry one prefix column per
            // parent, so no body line can start with "diff " at column 0: skip to the next file.
            let (path, _) = unquote(rest);
            let mut header = Vec::new();
            push_line(&mut header, line);
            i += 1;
            while i < all.len() && !starts_file(all[i]) {
                i += 1;
            }
            files.push(conflicted(text(&path), header));
        } else if let Some(rest) = line.strip_prefix(b"* Unmerged path ") {
            let mut header = Vec::new();
            push_line(&mut header, line);
            files.push(conflicted(text(rest), header));
            i += 1;
        } else {
            i += 1;
        }
    }
    files
}

fn conflicted(path: String, header: Vec<u8>) -> ParsedFile {
    ParsedFile {
        path,
        old_path: None,
        status: "conflicted".into(),
        binary: false,
        header,
        minus_name: None,
        plus_name: None,
        hunks: Vec::new(),
    }
}

fn starts_file(line: &[u8]) -> bool {
    line.starts_with(b"diff --git ")
        || line.starts_with(b"diff --cc ")
        || line.starts_with(b"diff --combined ")
        || line.starts_with(b"* Unmerged path ")
}

fn parse_git_file(all: &[&[u8]], start: usize, git_line_rest: &[u8]) -> (ParsedFile, usize) {
    let mut header = Vec::new();
    push_line(&mut header, all[start]);
    let mut i = start + 1;
    let mut new_file = false;
    let mut deleted = false;
    let mut binary = false;
    let mut rename: (Option<Vec<u8>>, Option<Vec<u8>>) = (None, None);
    let mut copy: (Option<Vec<u8>>, Option<Vec<u8>>) = (None, None);
    let mut minus_name = None;
    let mut plus_name = None;
    // Header lines.
    while i < all.len() {
        let line = all[i];
        if line.starts_with(b"@@ ") || starts_file(line) {
            break;
        }
        push_line(&mut header, line);
        if line.starts_with(b"new file mode ") {
            new_file = true;
        } else if line.starts_with(b"deleted file mode ") {
            deleted = true;
        } else if let Some(rest) = line.strip_prefix(b"rename from ") {
            rename.0 = Some(unquote(rest).0);
        } else if let Some(rest) = line.strip_prefix(b"rename to ") {
            rename.1 = Some(unquote(rest).0);
        } else if let Some(rest) = line.strip_prefix(b"copy from ") {
            copy.0 = Some(unquote(rest).0);
        } else if let Some(rest) = line.strip_prefix(b"copy to ") {
            copy.1 = Some(unquote(rest).0);
        } else if let Some(rest) = line.strip_prefix(b"--- ") {
            minus_name = Some(header_name(rest));
        } else if let Some(rest) = line.strip_prefix(b"+++ ") {
            plus_name = Some(header_name(rest));
        } else if line.starts_with(b"Binary files ") || line == b"GIT binary patch" {
            binary = true;
        }
        i += 1;
    }
    // Hunks.
    let mut hunks = Vec::new();
    while i < all.len() && all[i].starts_with(b"@@ ") {
        let Some((old_start, old_lines, new_start, new_lines)) = parse_hunk_header(all[i]) else {
            break;
        };
        let mut raw = Vec::new();
        push_line(&mut raw, all[i]);
        let header_text = text(all[i]);
        i += 1;
        let mut out_lines: Vec<DiffLine> = Vec::new();
        let (mut old_left, mut new_left) = (old_lines, new_lines);
        let (mut old_no, mut new_no) = (old_start, new_start);
        loop {
            if i >= all.len() {
                break;
            }
            let line = all[i];
            if line.first() == Some(&b'\\') {
                // "\ No newline at end of file" belongs to the line before it.
                if let Some(last) = out_lines.last_mut() {
                    last.no_eol = true;
                }
                push_line(&mut raw, line);
                i += 1;
                continue;
            }
            if old_left == 0 && new_left == 0 {
                break;
            }
            let (kind, body) = match line.first() {
                Some(b'+') => ("+", &line[1..]),
                Some(b'-') => ("-", &line[1..]),
                Some(b' ') => (" ", &line[1..]),
                // diff.suppressBlankEmpty prints an empty context line without its space.
                None => (" ", line),
                Some(_) => break,
            };
            let (old_line, new_line) = match kind {
                "+" => {
                    new_left = new_left.saturating_sub(1);
                    new_no += 1;
                    (None, Some(new_no - 1))
                }
                "-" => {
                    old_left = old_left.saturating_sub(1);
                    old_no += 1;
                    (Some(old_no - 1), None)
                }
                _ => {
                    old_left = old_left.saturating_sub(1);
                    new_left = new_left.saturating_sub(1);
                    old_no += 1;
                    new_no += 1;
                    (Some(old_no - 1), Some(new_no - 1))
                }
            };
            out_lines.push(DiffLine { kind: kind.into(), text: text(body), old_line, new_line, no_eol: false });
            push_line(&mut raw, line);
            i += 1;
        }
        hunks.push(ParsedHunk {
            hunk: Hunk { header: header_text, old_start, old_lines, new_start, new_lines, lines: out_lines },
            raw,
        });
    }
    // Skip anything unexpected up to the next file.
    while i < all.len() && !starts_file(all[i]) {
        i += 1;
    }

    let (path, old_path, status) = if let (Some(from), Some(to)) = (&rename.0, &rename.1) {
        (to.clone(), Some(from.clone()), "renamed")
    } else if let (Some(from), Some(to)) = (&copy.0, &copy.1) {
        (to.clone(), Some(from.clone()), "added")
    } else {
        let minus = minus_name.as_deref().and_then(|n| name_to_path(n, b"a/"));
        let plus = plus_name.as_deref().and_then(|n| name_to_path(n, b"b/"));
        let (a, b) = match (minus, plus) {
            (Some(a), Some(b)) => (a, b),
            (Some(a), None) => (a.clone(), a),
            (None, Some(b)) => (b.clone(), b),
            (None, None) => paths_from_git_line(git_line_rest),
        };
        let status = if new_file {
            "added"
        } else if deleted {
            "deleted"
        } else {
            "modified"
        };
        (if deleted { a } else { b }, None, status)
    };
    let file = ParsedFile {
        path: text(&path),
        old_path: old_path.map(|p| text(&p)),
        status: status.into(),
        binary,
        header,
        minus_name,
        plus_name,
        hunks,
    };
    (file, i)
}
