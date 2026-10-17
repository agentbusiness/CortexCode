//! The fallback edit formats: exact string replacement (unique match required) and patches
//! (Codex `apply_patch` V4A format and unified diffs). Both are applied exactly; they never fuzz
//! beyond ignoring trailing whitespace on context lines (journaled, not told).

use crate::doc::Doc;

/// Result of a text edit: new text plus the exact line map and changed regions.
#[derive(Debug, Clone)]
pub struct TextChange {
    pub text: String,
    /// Old line index -> new line index (None when the line was touched).
    pub line_map: Vec<Option<usize>>,
    /// Changed regions in the new text, 0-based half-open line ranges.
    pub changed: Vec<(usize, usize)>,
    pub notes: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct TextEditError {
    pub kind: &'static str,
    pub message: String,
}

impl TextEditError {
    fn new(kind: &'static str, message: impl Into<String>) -> Self {
        Self { kind, message: message.into() }
    }
}

/// Byte offsets where each line starts (plus the text length as a sentinel).
fn line_starts(text: &str) -> Vec<usize> {
    let mut starts = vec![0];
    for (index, byte) in text.bytes().enumerate() {
        if byte == b'\n' && index + 1 < text.len() {
            starts.push(index + 1);
        }
    }
    starts
}

fn line_of(starts: &[usize], offset: usize) -> usize {
    match starts.binary_search(&offset) {
        Ok(index) => index,
        Err(index) => index.saturating_sub(1),
    }
}

fn line_count(text: &str) -> usize {
    Doc::parse(text).len()
}

/// Apply non-overlapping byte edits (sorted by start) and compute the exact line map.
fn apply_byte_edits(text: &str, edits: &[(usize, usize, String)]) -> TextChange {
    let starts = line_starts(text);
    let old_lines = line_count(text);
    let mut out = String::with_capacity(text.len());
    let mut cursor = 0;
    let mut touched = vec![false; old_lines.max(1)];
    let mut new_regions: Vec<(usize, usize)> = Vec::new();
    for (start, end, replacement) in edits {
        out.push_str(&text[cursor..*start]);
        let first_line = line_of(&starts, *start);
        let last_line = if end > start { line_of(&starts, end - 1) } else { first_line };
        for flag in touched.iter_mut().take(last_line + 1).skip(first_line) {
            *flag = true;
        }
        let new_first = out.matches('\n').count();
        out.push_str(replacement);
        let new_last = out.matches('\n').count();
        new_regions.push((new_first, new_last + 1));
        cursor = *end;
    }
    out.push_str(&text[cursor..]);
    // Map untouched lines: walk old lines in order, tracking the shift from edits before them.
    let mut line_map = vec![None; old_lines];
    let mut shift: isize = 0;
    let mut edit_index = 0;
    for (old_index, slot) in line_map.iter_mut().enumerate() {
        while edit_index < edits.len() {
            let (start, end, replacement) = &edits[edit_index];
            let last_line = if end > start { line_of(&starts, end - 1) } else { line_of(&starts, *start) };
            if last_line < old_index {
                let removed = text[*start..*end].matches('\n').count() as isize;
                let added = replacement.matches('\n').count() as isize;
                shift += added - removed;
                edit_index += 1;
            } else {
                break;
            }
        }
        if !touched.get(old_index).copied().unwrap_or(false) {
            let mapped = old_index as isize + shift;
            if mapped >= 0 {
                *slot = Some(mapped as usize);
            }
        }
    }
    let new_total = line_count(&out);
    let changed = new_regions.into_iter().map(|(a, b)| (a.min(new_total), b.min(new_total).max(a.min(new_total)))).collect();
    TextChange { text: out, line_map, changed, notes: Vec::new() }
}

fn compose(first: &[Option<usize>], second: &[Option<usize>]) -> Vec<Option<usize>> {
    first.iter().map(|mapped| mapped.and_then(|index| second.get(index).copied().flatten())).collect()
}

fn match_positions(text: &str, needle: &str) -> Vec<usize> {
    let mut out = Vec::new();
    let mut from = 0;
    while let Some(position) = text[from..].find(needle) {
        out.push(from + position);
        from += position + needle.len().max(1);
    }
    out
}

/// Text with the `N:hh|` prefixes of read's lines taken off, when every non-empty line carries one
/// whose tag verifies (copied from read output); None otherwise.
fn strip_prefixes(text: &str) -> Option<String> {
    let lines: Vec<String> = text.split('\n').map(str::to_owned).collect();
    crate::hashedit::strip_copied_prefixes(&lines).map(|lines| lines.join("\n"))
}

/// Exact replacements applied in order, all or nothing. Each `old` must match exactly once unless
/// `all` is set. A CRLF file accepts `\n` in old/new (converted, journaled). `N:hh|` prefixes
/// copied from read output into old or new come off (as with anchored edits' text). `path` names
/// the file in the refusal.
pub fn replace(path: &str, text: &str, replacements: &[(String, String, bool)]) -> Result<TextChange, TextEditError> {
    let crlf = Doc::parse(text).default_eol == "\r\n";
    let mut current = text.to_owned();
    let mut map: Vec<Option<usize>> = (0..line_count(text)).map(Some).collect();
    let mut notes = Vec::new();
    let mut changed_all: Vec<(usize, usize)> = Vec::new();
    for (index, (old, new, all)) in replacements.iter().enumerate() {
        let which = if replacements.len() > 1 { format!(" of replacement #{}", index + 1) } else { String::new() };
        let mut old = old.clone();
        let mut new = new.clone();
        for (field, value) in [("old", &mut old), ("new", &mut new)] {
            if let Some(stripped) = strip_prefixes(value) {
                *value = stripped;
                notes.push(format!("removed N:hh| prefixes copied into `{field}`"));
            }
        }
        let (old, new) = if crlf && old.contains('\n') && !old.contains("\r\n") && !current.contains(old.as_str()) {
            notes.push("converted \\n to the file's \\r\\n line endings".to_owned());
            (old.replace('\n', "\r\n"), new.replace("\r\n", "\n").replace('\n', "\r\n"))
        } else {
            (old.clone(), new.clone())
        };
        let positions = match_positions(&current, &old);
        if positions.is_empty() {
            let mut message = format!("Nothing was written: `old`{which} was not found in {path}.\n");
            let doc = Doc::parse(&current);
            for line in near_lines(&current, &old) {
                message.push_str(&doc.render(line, line));
            }
            message.push_str("Read the file again, then send the edit.");
            return Err(TextEditError::new("not_found", message));
        }
        if positions.len() > 1 && !all {
            let starts = line_starts(&current);
            let lines: Vec<String> = positions.iter().map(|position| (line_of(&starts, *position) + 1).to_string()).collect();
            return Err(TextEditError::new(
                "ambiguous_match",
                format!(
                    "Nothing was written: `old`{which} matches {} places (lines {}). Include more surrounding text, or set all.",
                    positions.len(),
                    lines.join(", ")
                ),
            ));
        }
        let edits: Vec<(usize, usize, String)> = positions.iter().map(|position| (*position, position + old.len(), new.clone())).collect();
        let step = apply_byte_edits(&current, &edits);
        map = compose(&map, &step.line_map);
        // Earlier changed regions move with the new edit.
        changed_all = changed_all
            .into_iter()
            .filter_map(|(a, b)| {
                let a2 = step.line_map.get(a).copied().flatten()?;
                Some((a2, a2 + (b - a)))
            })
            .collect();
        changed_all.extend(step.changed.iter().copied());
        current = step.text;
    }
    changed_all.sort();
    Ok(TextChange { text: current, line_map: map, changed: changed_all, notes })
}

/// The lines (1-based, at most 5) equal to `old`'s first non-blank line except for whitespace.
fn near_lines(text: &str, old: &str) -> Vec<usize> {
    let squash = |s: &str| s.split_whitespace().collect::<Vec<_>>().join(" ");
    let first = old.lines().find(|line| !line.trim().is_empty()).map(str::trim).unwrap_or("");
    if first.is_empty() {
        return Vec::new();
    }
    text.lines().enumerate().filter(|(_, line)| squash(line) == squash(first)).map(|(index, _)| index + 1).take(5).collect()
}

/// Point the model at a near miss (same text ignoring whitespace), mechanically.
fn near_miss(text: &str, old: &str) -> String {
    let squash = |s: &str| s.split_whitespace().collect::<Vec<_>>().join(" ");
    let first = old.lines().find(|line| !line.trim().is_empty()).map(str::trim).unwrap_or("");
    if first.is_empty() {
        return String::new();
    }
    let hits: Vec<usize> =
        text.lines().enumerate().filter(|(_, line)| squash(line) == squash(first)).map(|(index, _)| index + 1).take(5).collect();
    if hits.is_empty() {
        "; no line resembles its first line either (read the file again)".to_owned()
    } else {
        format!(
            "; a line equal to its first line except for whitespace is at line {} (whitespace and indentation must match exactly; read those lines again)",
            hits.iter().map(usize::to_string).collect::<Vec<_>>().join(", ")
        )
    }
}

// ---------------------------------------------------------------------------------------------
// Patches

#[derive(Debug, Clone, PartialEq)]
pub enum FileOp {
    Add { path: String, content: String },
    Delete { path: String },
    Update { path: String, move_to: Option<String>, hunks: Vec<Hunk> },
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Hunk {
    /// `@@ text` context header (V4A) used to narrow the search.
    pub header: Option<String>,
    pub old: Vec<String>,
    pub new: Vec<String>,
    /// 1-based old start line from a unified diff header.
    pub line_hint: Option<usize>,
    /// The hunk must match at the end of the file (`*** End of File`).
    pub at_eof: bool,
}

pub fn parse_patch(text: &str) -> Result<Vec<FileOp>, TextEditError> {
    let trimmed = text.trim_start();
    if trimmed.starts_with("*** Begin Patch") {
        parse_v4a(text)
    } else if trimmed.starts_with("--- ") || trimmed.starts_with("diff --git") || trimmed.starts_with("Index: ") {
        parse_unified(text)
    } else {
        Err(TextEditError::new("bad_patch", "the patch is neither `*** Begin Patch` (apply_patch) nor a unified diff (--- / +++ / @@)"))
    }
}

fn parse_v4a(text: &str) -> Result<Vec<FileOp>, TextEditError> {
    let lines: Vec<&str> = text.split('\n').map(|line| line.strip_suffix('\r').unwrap_or(line)).collect();
    let mut index = lines.iter().position(|line| line.trim() == "*** Begin Patch").map(|i| i + 1).unwrap_or(0);
    let mut ops = Vec::new();
    let mut ended = false;
    while index < lines.len() {
        let line = lines[index];
        if line.trim() == "*** End Patch" {
            ended = true;
            break;
        }
        if let Some(path) = line.strip_prefix("*** Add File: ") {
            index += 1;
            let mut content = String::new();
            while index < lines.len() && !lines[index].starts_with("*** ") {
                match lines[index].strip_prefix('+') {
                    Some(rest) => {
                        content.push_str(rest);
                        content.push('\n');
                    }
                    None if lines[index].is_empty() && index + 1 == lines.len() => {}
                    None => {
                        return Err(TextEditError::new(
                            "bad_patch",
                            format!("Add File {path}: every line must start with `+`, got `{}`", lines[index]),
                        ));
                    }
                }
                index += 1;
            }
            ops.push(FileOp::Add { path: path.trim().to_owned(), content });
            continue;
        }
        if let Some(path) = line.strip_prefix("*** Delete File: ") {
            ops.push(FileOp::Delete { path: path.trim().to_owned() });
            index += 1;
            continue;
        }
        if let Some(path) = line.strip_prefix("*** Update File: ") {
            index += 1;
            let mut move_to = None;
            if index < lines.len()
                && let Some(target) = lines[index].strip_prefix("*** Move to: ")
            {
                move_to = Some(target.trim().to_owned());
                index += 1;
            }
            let mut hunks = Vec::new();
            let mut current = Hunk::default();
            let mut has_lines = false;
            while index < lines.len() {
                let line = lines[index];
                if line.trim() == "*** End of File" {
                    current.at_eof = true;
                    index += 1;
                    continue;
                }
                if line.starts_with("*** ") {
                    break;
                }
                if let Some(header) = line.strip_prefix("@@") {
                    if has_lines {
                        hunks.push(std::mem::take(&mut current));
                        has_lines = false;
                    }
                    let header = header.trim();
                    current.header = (!header.is_empty()).then(|| header.to_owned());
                } else if let Some(rest) = line.strip_prefix('-') {
                    current.old.push(rest.to_owned());
                    has_lines = true;
                } else if let Some(rest) = line.strip_prefix('+') {
                    current.new.push(rest.to_owned());
                    has_lines = true;
                } else if let Some(rest) = line.strip_prefix(' ') {
                    current.old.push(rest.to_owned());
                    current.new.push(rest.to_owned());
                    has_lines = true;
                } else if line.is_empty() {
                    // An empty context line whose leading space was lost.
                    if index + 1 < lines.len() && !lines[index + 1].starts_with("*** ") {
                        current.old.push(String::new());
                        current.new.push(String::new());
                        has_lines = true;
                    }
                } else {
                    return Err(TextEditError::new(
                        "bad_patch",
                        format!("Update File {path}: unexpected line `{line}` (hunk lines start with ' ', '-' or '+')"),
                    ));
                }
                index += 1;
            }
            if has_lines {
                hunks.push(current);
            }
            if hunks.is_empty() && move_to.is_none() {
                return Err(TextEditError::new("bad_patch", format!("Update File {path} has no changes")));
            }
            ops.push(FileOp::Update { path: path.trim().to_owned(), move_to, hunks });
            continue;
        }
        if line.trim().is_empty() {
            index += 1;
            continue;
        }
        return Err(TextEditError::new("bad_patch", format!("unexpected line in patch: `{line}`")));
    }
    if !ended {
        return Err(TextEditError::new("truncated", "the patch has no `*** End Patch` line (cut off?); send the complete patch"));
    }
    if ops.is_empty() {
        return Err(TextEditError::new("bad_patch", "the patch contains no file operations"));
    }
    Ok(ops)
}

fn strip_diff_prefix(path: &str) -> String {
    let path = path.split('\t').next().unwrap_or(path).trim();
    if path == "/dev/null" {
        return path.to_owned();
    }
    path.strip_prefix("a/").or_else(|| path.strip_prefix("b/")).unwrap_or(path).to_owned()
}

fn parse_unified(text: &str) -> Result<Vec<FileOp>, TextEditError> {
    let lines: Vec<&str> = text.split('\n').map(|line| line.strip_suffix('\r').unwrap_or(line)).collect();
    let mut ops = Vec::new();
    let mut index = 0;
    while index < lines.len() {
        let Some(old_path) = lines[index].strip_prefix("--- ") else {
            index += 1;
            continue;
        };
        let Some(new_path) = lines.get(index + 1).and_then(|line| line.strip_prefix("+++ ")) else {
            return Err(TextEditError::new("bad_patch", "a `---` line must be followed by `+++`"));
        };
        let old_path = strip_diff_prefix(old_path);
        let new_path = strip_diff_prefix(new_path);
        index += 2;
        let mut hunks = Vec::new();
        while index < lines.len() && lines[index].starts_with("@@") {
            let header = lines[index];
            let (old_start, old_count, new_count) =
                parse_hunk_header(header).ok_or_else(|| TextEditError::new("bad_patch", format!("bad hunk header `{header}`")))?;
            index += 1;
            let mut hunk = Hunk { line_hint: Some(old_start.max(1)), ..Hunk::default() };
            let (mut seen_old, mut seen_new) = (0usize, 0usize);
            while index < lines.len() && (seen_old < old_count || seen_new < new_count) {
                let line = lines[index];
                if let Some(rest) = line.strip_prefix('-') {
                    hunk.old.push(rest.to_owned());
                    seen_old += 1;
                } else if let Some(rest) = line.strip_prefix('+') {
                    hunk.new.push(rest.to_owned());
                    seen_new += 1;
                } else if let Some(rest) = line.strip_prefix(' ') {
                    hunk.old.push(rest.to_owned());
                    hunk.new.push(rest.to_owned());
                    seen_old += 1;
                    seen_new += 1;
                } else if line.starts_with('\\') {
                    // "\ No newline at end of file"
                } else if line.is_empty() {
                    hunk.old.push(String::new());
                    hunk.new.push(String::new());
                    seen_old += 1;
                    seen_new += 1;
                } else {
                    return Err(TextEditError::new("bad_patch", format!("unexpected line in hunk: `{line}`")));
                }
                index += 1;
            }
            if seen_old < old_count || seen_new < new_count {
                return Err(TextEditError::new("truncated", format!("hunk `{header}` is shorter than its header says (cut off?)")));
            }
            while index < lines.len() && lines[index].starts_with('\\') {
                index += 1;
            }
            hunks.push(hunk);
        }
        if old_path == "/dev/null" {
            let content: String = hunks.iter().flat_map(|hunk| hunk.new.iter()).map(|line| format!("{line}\n")).collect();
            ops.push(FileOp::Add { path: new_path, content });
        } else if new_path == "/dev/null" {
            ops.push(FileOp::Delete { path: old_path });
        } else {
            let move_to = (new_path != old_path).then_some(new_path);
            ops.push(FileOp::Update { path: old_path, move_to, hunks });
        }
    }
    if ops.is_empty() {
        return Err(TextEditError::new("bad_patch", "no `---`/`+++` file headers found in the diff"));
    }
    Ok(ops)
}

fn parse_hunk_header(header: &str) -> Option<(usize, usize, usize)> {
    let body = header.strip_prefix("@@")?.split("@@").next()?.trim();
    let mut parts = body.split_whitespace();
    let old = parts.next()?.strip_prefix('-')?;
    let new = parts.next()?.strip_prefix('+')?;
    let parse = |spec: &str| -> Option<(usize, usize)> {
        match spec.split_once(',') {
            Some((start, count)) => Some((start.parse().ok()?, count.parse().ok()?)),
            None => Some((spec.parse().ok()?, 1)),
        }
    };
    let (old_start, old_count) = parse(old)?;
    let (_, new_count) = parse(new)?;
    Some((old_start, old_count, new_count))
}

/// Positions where `needle` lines occur in `lines` at or after `from`, comparing exactly or with
/// trailing whitespace ignored.
fn find_sequence(lines: &[String], needle: &[String], from: usize, relaxed: bool) -> Vec<usize> {
    if needle.is_empty() {
        return Vec::new();
    }
    let eq = |a: &str, b: &str| if relaxed { a.trim_end() == b.trim_end() } else { a == b };
    let mut out = Vec::new();
    if needle.len() > lines.len() {
        return out;
    }
    for start in from..=(lines.len() - needle.len()) {
        if needle.iter().enumerate().all(|(offset, line)| eq(&lines[start + offset], line)) {
            out.push(start);
        }
    }
    out
}

/// Apply the hunks of one file to its text, all or nothing.
pub fn apply_hunks(path: &str, text: &str, hunks: &[Hunk]) -> Result<TextChange, TextEditError> {
    let doc = Doc::parse(text);
    let lines = &doc.lines;
    let mut placements: Vec<(usize, usize, Vec<String>)> = Vec::new();
    let mut notes = Vec::new();
    let mut cursor = 0usize;
    for (number, hunk) in hunks.iter().enumerate() {
        let label = format!("{path} hunk #{}", number + 1);
        let mut from = cursor;
        if let Some(header) = &hunk.header {
            let hits: Vec<usize> = (cursor..lines.len()).filter(|index| lines[*index].trim() == header.trim()).collect();
            match hits.as_slice() {
                [] => {}
                [only] => from = *only,
                _ => {
                    return Err(TextEditError::new(
                        "ambiguous_match",
                        format!("{label}: the @@ context `{header}` occurs {} times", hits.len()),
                    ));
                }
            }
        }
        let start = if hunk.old.is_empty() {
            // Pure insertion: at the hinted line, at EOF, or after the header.
            if hunk.at_eof {
                lines.len()
            } else if let Some(hint) = hunk.line_hint {
                hint.min(lines.len())
            } else if hunk.header.is_some() {
                from + 1
            } else {
                return Err(TextEditError::new("bad_patch", format!("{label} adds lines without any context to place them")));
            }
        } else {
            let mut hits = find_sequence(lines, &hunk.old, from, false);
            if hits.is_empty() {
                hits = find_sequence(lines, &hunk.old, from, true);
                if !hits.is_empty() {
                    notes.push(format!("{label}: matched ignoring trailing whitespace"));
                }
            }
            if hunk.at_eof {
                hits.retain(|hit| hit + hunk.old.len() == lines.len());
            }
            match (hits.as_slice(), hunk.line_hint) {
                ([], _) => {
                    let first = hunk.old.iter().find(|line| !line.trim().is_empty()).cloned().unwrap_or_default();
                    return Err(TextEditError::new(
                        "not_found",
                        format!("{label}: its context/removed lines do not match the file{}", near_miss(&lines.join("\n"), &first)),
                    ));
                }
                ([only], _) => *only,
                (many, Some(hint)) if many.contains(&(hint - 1)) => hint - 1,
                (many, _) => {
                    return Err(TextEditError::new(
                        "ambiguous_match",
                        format!(
                            "{label}: its context matches {} places (lines {}); add more context lines",
                            many.len(),
                            many.iter().map(|hit| (hit + 1).to_string()).collect::<Vec<_>>().join(", ")
                        ),
                    ));
                }
            }
        };
        let end = start + hunk.old.len();
        if let Some((_, previous_end, _)) = placements.last()
            && start < *previous_end
        {
            return Err(TextEditError::new("bad_patch", format!("{label} overlaps the previous hunk")));
        }
        placements.push((start, end, hunk.new.clone()));
        cursor = end;
    }
    // Rebuild with exact terminators.
    let mut entries: Vec<(String, Option<&'static str>)> = Vec::new();
    let mut line_map = vec![None; lines.len()];
    let mut changed = Vec::new();
    let mut old = 0;
    for (start, end, new_lines) in &placements {
        while old < *start {
            line_map[old] = Some(entries.len());
            entries.push((lines[old].clone(), Some(doc.eols[old])));
            old += 1;
        }
        // Lines equal on both sides at the edges of a hunk are context, not changes.
        let first = entries.len();
        for line in new_lines {
            entries.push((line.clone(), None));
        }
        let old_slice = &lines[*start..*end];
        let mut prefix = 0;
        while prefix < old_slice.len() && prefix < new_lines.len() && old_slice[prefix] == new_lines[prefix] {
            line_map[start + prefix] = Some(first + prefix);
            entries[first + prefix].1 = Some(doc.eols[start + prefix]);
            prefix += 1;
        }
        let mut suffix = 0;
        while suffix < old_slice.len() - prefix
            && suffix < new_lines.len() - prefix
            && old_slice[old_slice.len() - 1 - suffix] == new_lines[new_lines.len() - 1 - suffix]
        {
            let old_index = end - 1 - suffix;
            let new_index = first + new_lines.len() - 1 - suffix;
            line_map[old_index] = Some(new_index);
            entries[new_index].1 = Some(doc.eols[old_index]);
            suffix += 1;
        }
        changed.push((first + prefix, first + new_lines.len() - suffix));
        old = *end;
    }
    while old < lines.len() {
        line_map[old] = Some(entries.len());
        entries.push((lines[old].clone(), Some(doc.eols[old])));
        old += 1;
    }
    let new_doc = doc.rebuild(entries);
    Ok(TextChange { text: new_doc.to_text(), line_map, changed, notes })
}
