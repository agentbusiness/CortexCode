//! Hashline-anchored edits: a batch of line operations named by `N:hh` anchors, validated against the
//! current file and applied atomically (all or nothing).
//!
//! Anchor resolution is mechanical:
//! - an anchor matches when the current line `N` has tag `hh`;
//! - when the session's last view of the file is known and the file has only been changed by this
//!   session's own edits since, an anchor taken from that view is mapped through the exact line map
//!   of those edits (a line that the session did not touch keeps its identity);
//! - if both interpretations exist and disagree, the anchor is ambiguous and the batch is rejected.
//!
//! An anchor that no longer names its line (stale, ambiguous, or a bare number after a change) skips
//! only its own operation: the others are applied, and the result names each skipped one with the
//! lines around it as they now are (seen: six good edits lost to one stale anchor). A malformed
//! batch (a bad range, overlapping operations) or one with nothing left to apply is rejected whole.

use crate::doc::{Doc, text_to_lines};
use cc_protocol::hashline::{self, Anchor};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpKind {
    Replace,
    InsertAfter,
    InsertBefore,
    Delete,
}

impl OpKind {
    pub fn parse(name: &str) -> Option<OpKind> {
        match name {
            "replace" => Some(OpKind::Replace),
            "insert_after" => Some(OpKind::InsertAfter),
            "insert_before" => Some(OpKind::InsertBefore),
            "delete" => Some(OpKind::Delete),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            OpKind::Replace => "replace",
            OpKind::InsertAfter => "insert_after",
            OpKind::InsertBefore => "insert_before",
            OpKind::Delete => "delete",
        }
    }
}

/// How an operation names a line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineRef {
    /// `N:hh`
    Tagged(Anchor),
    /// `0`: before the first line (only for `insert_after`).
    Start,
    /// A bare 1-based line number from a line-number dialect (str_replace_editor `insert_line`).
    /// Valid only while the session's view of the file is exactly current.
    Bare(usize),
}

impl LineRef {
    pub fn parse(text: &str) -> Option<LineRef> {
        let trimmed = text.trim();
        let head = trimmed.split(['|', ':']).next().unwrap_or("").trim();
        if head == "0" {
            return Some(LineRef::Start);
        }
        hashline::parse_anchor(trimmed).map(LineRef::Tagged)
    }
}

impl std::fmt::Display for LineRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LineRef::Tagged(anchor) => write!(f, "{anchor}"),
            LineRef::Start => write!(f, "0"),
            LineRef::Bare(line) => write!(f, "line {line}"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditOp {
    pub kind: OpKind,
    pub at: LineRef,
    pub to: Option<LineRef>,
    /// New lines (for replace and inserts).
    pub lines: Vec<String>,
}

/// What the session last saw of the file, and how its lines map into the current file through the
/// session's own edits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct View {
    pub base_lines: Vec<String>,
    /// `map[i]` = current index of base line `i`, or `None` if the session replaced/deleted it.
    pub map: Vec<Option<usize>>,
}

impl View {
    pub fn identity(lines: &[String]) -> View {
        View { base_lines: lines.to_vec(), map: (0..lines.len()).map(Some).collect() }
    }

    pub fn is_identity(&self, current_len: usize) -> bool {
        self.map.len() == current_len && self.map.iter().enumerate().all(|(index, mapped)| *mapped == Some(index))
    }

    /// Compose with the line map of a further edit (old current index -> new index).
    pub fn compose(&self, step: &[Option<usize>]) -> View {
        View {
            base_lines: self.base_lines.clone(),
            map: self.map.iter().map(|mapped| mapped.and_then(|index| step.get(index).copied().flatten())).collect(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Problem {
    /// The anchor's tag does not match the current file (or the line no longer exists).
    Stale { op: usize, anchor: String, line: usize },
    /// The anchor matches the current line and, through the session's earlier view, another line.
    Ambiguous { op: usize, anchor: String, direct: usize, relocated: usize },
    /// A bare line number was used but the file changed since the session's view.
    BareWithoutView { op: usize, line: usize },
    /// `to` before `at`, `to` on an insert, `0` on something other than insert_after, ...
    BadRange { op: usize, detail: String },
    /// Two operations touch the same line.
    Overlap { first: usize, second: usize, line: usize },
}

#[derive(Debug, Clone)]
pub struct Rejection {
    pub problems: Vec<Problem>,
    /// Human/model-facing explanation with fresh hashlines around each problem.
    pub message: String,
    pub error_kind: &'static str,
}

#[derive(Debug, Clone)]
pub struct Applied {
    pub doc: Doc,
    /// Changed regions in the new file as 0-based half-open ranges (empty for pure deletions).
    pub changed: Vec<(usize, usize)>,
    /// Old index -> new index for every line of the file before the edit.
    pub line_map: Vec<Option<usize>>,
    /// Anchors that were relocated through the session's view, e.g. "50:ab -> 53".
    pub relocations: Vec<String>,
    pub added: usize,
    pub removed: usize,
    /// The operations not applied (their anchors no longer named their lines), said with the lines
    /// around each in the new file and fresh anchors; empty when every operation was applied.
    pub skipped: String,
}

enum Resolved {
    Line(usize),
    Start,
}

fn resolve(doc: &Doc, view: Option<&View>, reference: LineRef, op: usize, relocations: &mut Vec<String>) -> Result<Resolved, Problem> {
    match reference {
        LineRef::Start => Ok(Resolved::Start),
        LineRef::Bare(line) => {
            let Some(view) = view.filter(|view| view.is_identity(doc.len())) else {
                return Err(Problem::BareWithoutView { op, line });
            };
            if line == 0 {
                return Ok(Resolved::Start);
            }
            match view.map.get(line - 1).copied().flatten() {
                Some(index) => Ok(Resolved::Line(index)),
                None => Err(Problem::BadRange { op, detail: format!("line {line} is past the end of the file ({} lines)", doc.len()) }),
            }
        }
        LineRef::Tagged(anchor) => {
            let index = anchor.line - 1;
            let direct = index < doc.len() && doc.tag(index).as_bytes() == anchor.tag;
            let relocated = view.and_then(|view| {
                let base = view.base_lines.get(index)?;
                if hashline::tag(base).as_bytes() != anchor.tag {
                    return None;
                }
                view.map.get(index).copied().flatten()
            });
            match (direct, relocated) {
                (true, None) => Ok(Resolved::Line(index)),
                (true, Some(other)) if other == index => Ok(Resolved::Line(index)),
                // Both readings name a line with the same text (a duplicated line): the line the
                // anchor's number names now is the one shown last.
                (true, Some(other)) if doc.lines.get(other) == doc.lines.get(index) => Ok(Resolved::Line(index)),
                (true, Some(other)) => Err(Problem::Ambiguous { op, anchor: anchor.to_string(), direct: index + 1, relocated: other + 1 }),
                (false, Some(other)) => {
                    relocations.push(format!("{anchor} -> line {}", other + 1));
                    Ok(Resolved::Line(other))
                }
                // The file changed outside this session's view (another agent's edit, a command):
                // a line whose tag is the anchor's and the only such line of the file is the line
                // the anchor named, moved; the edit goes there with a note instead of failing (seen:
                // eleven refused edits in one run, each costing a read for fresh anchors).
                (false, None) => {
                    let mut tagged = (0..doc.len()).filter(|index| doc.tag(*index).as_bytes() == anchor.tag);
                    match (tagged.next(), tagged.next()) {
                        (Some(only), None) => {
                            relocations.push(format!("{anchor} -> line {} (the file changed; found by its tag)", only + 1));
                            Ok(Resolved::Line(only))
                        }
                        _ => Err(Problem::Stale { op, anchor: anchor.to_string(), line: anchor.line }),
                    }
                }
            }
        }
    }
}

/// One validated operation in current-file coordinates.
struct Planned {
    order: usize,
    kind: OpKind,
    /// For replace/delete: inclusive line span. For inserts: the anchor line (None for start).
    span: Option<(usize, usize)>,
    /// Insertion gap (index of the line the new lines go before; `len` = end of file).
    gap: usize,
    lines: Vec<String>,
}

/// Validate and apply a batch. `view` must only be passed when the file is unchanged since the
/// session's last read or own edit (the caller checks the content hash).
pub fn apply(doc: &Doc, view: Option<&View>, ops: &[EditOp]) -> Result<Applied, Rejection> {
    let mut problems = Vec::new();
    let mut relocations = Vec::new();
    let mut planned = Vec::new();
    if ops.is_empty() {
        problems.push(Problem::BadRange { op: 0, detail: "the batch contains no operations".to_owned() });
    }
    for (order, op) in ops.iter().enumerate() {
        let at = match resolve(doc, view, op.at, order, &mut relocations) {
            Ok(at) => at,
            Err(problem) => {
                problems.push(problem);
                continue;
            }
        };
        let to = match op.to {
            None => None,
            Some(reference) => match resolve(doc, view, reference, order, &mut relocations) {
                Ok(Resolved::Line(index)) => Some(index),
                Ok(Resolved::Start) => {
                    problems.push(Problem::BadRange { op: order, detail: "`to` cannot be 0".to_owned() });
                    continue;
                }
                Err(problem) => {
                    problems.push(problem);
                    continue;
                }
            },
        };
        match op.kind {
            OpKind::Replace | OpKind::Delete => {
                let Resolved::Line(start) = at else {
                    problems.push(Problem::BadRange {
                        op: order,
                        detail: format!("{} needs a line anchor, not 0 (0 only works with insert_after)", op.kind.name()),
                    });
                    continue;
                };
                let end = to.unwrap_or(start);
                if end < start {
                    problems.push(Problem::BadRange {
                        op: order,
                        detail: format!("range end (line {}) is before its start (line {})", end + 1, start + 1),
                    });
                    continue;
                }
                let lines = if op.kind == OpKind::Replace { op.lines.clone() } else { Vec::new() };
                planned.push(Planned { order, kind: op.kind, span: Some((start, end)), gap: start, lines });
            }
            OpKind::InsertAfter | OpKind::InsertBefore => {
                if to.is_some() {
                    problems
                        .push(Problem::BadRange { op: order, detail: format!("{} takes one anchor (`at`), not a range", op.kind.name()) });
                    continue;
                }
                match (op.kind, at) {
                    (OpKind::InsertBefore, Resolved::Start) => {
                        problems.push(Problem::BadRange {
                            op: order,
                            detail: "insert_before needs a line anchor; use insert_after 0 for the start of the file".to_owned(),
                        });
                    }
                    (_, Resolved::Start) => {
                        planned.push(Planned { order, kind: op.kind, span: None, gap: 0, lines: op.lines.clone() });
                    }
                    (kind, Resolved::Line(index)) => {
                        let gap = if kind == OpKind::InsertAfter { index + 1 } else { index };
                        planned.push(Planned { order, kind, span: Some((index, index)), gap, lines: op.lines.clone() });
                    }
                }
            }
        }
    }

    // Overlap: two operations touching a common line, except two inserts sharing an anchor line.
    for i in 0..planned.len() {
        for j in (i + 1)..planned.len() {
            let (a, b) = (&planned[i], &planned[j]);
            let (Some((a_start, a_end)), Some((b_start, b_end))) = (a.span, b.span) else { continue };
            let both_inserts = matches!(a.kind, OpKind::InsertAfter | OpKind::InsertBefore)
                && matches!(b.kind, OpKind::InsertAfter | OpKind::InsertBefore);
            if both_inserts {
                continue;
            }
            if a_start <= b_end && b_start <= a_end {
                problems.push(Problem::Overlap { first: a.order, second: b.order, line: a_start.max(b_start) + 1 });
            }
        }
    }

    // Only anchor problems (a line that moved or changed) skip their own operation; anything else,
    // or nothing left to apply, rejects the batch.
    let anchor_only = problems.iter().all(|problem| matches!(problem, Problem::Stale { .. } | Problem::Ambiguous { .. } | Problem::BareWithoutView { .. }));
    if !problems.is_empty() && (!anchor_only || planned.is_empty()) {
        return Err(reject(doc, problems, ops.len() > 1));
    }

    // Apply in one pass over the old lines.
    let len = doc.len();
    let mut inserts_at: Vec<Vec<usize>> = vec![Vec::new(); len + 1];
    let mut span_at: Vec<Option<usize>> = vec![None; len];
    for (index, plan) in planned.iter().enumerate() {
        match plan.kind {
            OpKind::InsertAfter | OpKind::InsertBefore => inserts_at[plan.gap].push(index),
            OpKind::Replace | OpKind::Delete => {
                if let Some((start, _)) = plan.span {
                    span_at[start] = Some(index);
                }
            }
        }
    }
    for list in &mut inserts_at {
        list.sort_by_key(|index| planned[*index].order);
    }
    let mut entries: Vec<(String, Option<&'static str>)> = Vec::with_capacity(len);
    let mut line_map = vec![None; len];
    let mut changed = Vec::new();
    let mut added = 0;
    let mut removed = 0;
    let mut old = 0;
    loop {
        if old <= len {
            for index in &inserts_at[old] {
                let start = entries.len();
                for line in &planned[*index].lines {
                    entries.push((line.clone(), None));
                }
                added += planned[*index].lines.len();
                changed.push((start, entries.len()));
            }
        }
        if old >= len {
            break;
        }
        if let Some(index) = span_at[old] {
            let plan = &planned[index];
            let (start, end) = plan.span.unwrap_or((old, old));
            let first = entries.len();
            for line in &plan.lines {
                entries.push((line.clone(), None));
            }
            added += plan.lines.len();
            removed += end - start + 1;
            changed.push((first, entries.len()));
            old = end + 1;
            continue;
        }
        line_map[old] = Some(entries.len());
        entries.push((doc.lines[old].clone(), Some(doc.eols[old])));
        old += 1;
    }
    let new_doc = doc.rebuild(entries);
    let skipped = if problems.is_empty() { String::new() } else { skipped_text(&new_doc, &line_map, &problems, ops.len() > 1) };
    Ok(Applied { doc: new_doc, changed, line_map, relocations, added, removed, skipped })
}

/// The operations a partly applied batch skipped, each with the lines around its anchor as they now
/// are (after the applied ones), and the step: send those again.
fn skipped_text(new_doc: &Doc, line_map: &[Option<usize>], problems: &[Problem], several: bool) -> String {
    let label = |op: usize| if several { format!("edit #{}: ", op + 1) } else { String::new() };
    // A line of the old file, where it is now (or the nearest kept line before it).
    let now = |line: usize| -> usize {
        let old = line.saturating_sub(1).min(line_map.len().saturating_sub(1));
        (0..=old).rev().find_map(|index| line_map.get(index).copied().flatten()).map(|index| index + 1).unwrap_or(1)
    };
    let mut lines = Vec::new();
    let mut regions: Vec<(usize, usize)> = Vec::new();
    for problem in problems {
        let (op, why, line) = match problem {
            Problem::Stale { op, anchor, line } => (*op, format!("anchor {anchor} is stale"), *line),
            Problem::Ambiguous { op, anchor, direct, .. } => (*op, format!("anchor {anchor} is ambiguous"), *direct),
            Problem::BareWithoutView { op, line } => (*op, format!("line number {line} cannot be used (the file changed)"), *line),
            _ => continue,
        };
        lines.push(format!("{}{why}", label(op)));
        let at = now(line);
        regions.push((at.saturating_sub(3).max(1), (at + 3).min(new_doc.len())));
    }
    let mut text = format!("Not applied (the other edits were written):\n{}", lines.iter().map(|line| format!("- {line}.\n")).collect::<String>());
    regions.sort();
    let mut merged: Vec<(usize, usize)> = Vec::new();
    for (start, end) in regions.into_iter().filter(|(start, end)| start <= end) {
        match merged.last_mut() {
            Some(last) if start <= last.1 + 1 => last.1 = last.1.max(end),
            _ => merged.push((start, end)),
        }
    }
    if !merged.is_empty() {
        text.push_str("The lines there now:\n");
    }
    for (index, (start, end)) in merged.iter().enumerate() {
        if index > 0 {
            text.push_str("...\n");
        }
        text.push_str(&new_doc.render(*start, *end));
    }
    text.push_str("Send those edits again with these anchors.\n");
    text
}

/// What was wrong, the current lines around it (with fresh anchors), and the step: send the edit
/// again. "edit #N" names the operation only when the call had several.
fn reject(doc: &Doc, problems: Vec<Problem>, several: bool) -> Rejection {
    let label = |op: usize| if several { format!("edit #{}: ", op + 1) } else { String::new() };
    let mut lines: Vec<String> = Vec::new();
    let mut regions: Vec<(usize, usize)> = Vec::new();
    let mut error_kind = "bad_edit";
    for problem in &problems {
        match problem {
            Problem::Stale { op, anchor, line } => {
                error_kind = "stale_anchor";
                if *line > doc.len() {
                    lines.push(format!("{}anchor {anchor} is stale (the file has {} lines)", label(*op), doc.len()));
                    regions.push((doc.len().saturating_sub(3).max(1), doc.len()));
                } else {
                    lines.push(format!("{}anchor {anchor} is stale (line {line} is now {line}:{})", label(*op), doc.tag(line - 1)));
                    regions.push((line.saturating_sub(3).max(1), line + 3));
                }
            }
            Problem::Ambiguous { op, anchor, direct, relocated } => {
                if error_kind != "stale_anchor" {
                    error_kind = "ambiguous_anchor";
                }
                lines.push(format!(
                    "{}anchor {anchor} is ambiguous (it is line {direct} now and, through your earlier edits, line {relocated})",
                    label(*op)
                ));
                regions.push((direct.saturating_sub(2).max(1), direct + 2));
                regions.push((relocated.saturating_sub(2).max(1), relocated + 2));
            }
            Problem::BareWithoutView { op, line } => {
                error_kind = "stale_anchor";
                lines.push(format!("{}line number {line} cannot be used (the file changed since you read it)", label(*op)));
                regions.push((line.saturating_sub(3).max(1), line + 3));
            }
            Problem::BadRange { op, detail } => {
                lines.push(format!("{}{detail}", label(*op)));
            }
            Problem::Overlap { first, second, line } => {
                if error_kind == "bad_edit" {
                    error_kind = "overlapping_edits";
                }
                lines.push(format!("edits #{} and #{} both touch line {line}; merge them into one", first + 1, second + 1));
                regions.push((line.saturating_sub(2).max(1), line + 2));
            }
        }
    }
    let mut message = match lines.as_slice() {
        [one] => format!("Nothing was written: {one}.\n"),
        many => format!("Nothing was written:\n{}", many.iter().map(|line| format!("- {line}.\n")).collect::<String>()),
    };
    regions.retain(|(start, end)| *start <= *end && *start <= doc.len());
    // A stale anchor in a small file: the whole file as it is now, so every next anchor comes from
    // one current view, not from lines the model remembers (seen: three stale edits in a row).
    const WHOLE_UP_TO: usize = 200;
    if error_kind == "stale_anchor" && doc.len() <= WHOLE_UP_TO && !regions.is_empty() {
        regions = vec![(1, doc.len())];
        message.push_str("The whole file as it is now:\n");
    }
    regions.sort();
    let mut merged: Vec<(usize, usize)> = Vec::new();
    for (start, end) in regions {
        match merged.last_mut() {
            Some(last) if start <= last.1 + 1 => last.1 = last.1.max(end),
            _ => merged.push((start, end)),
        }
    }
    for (index, (start, end)) in merged.iter().enumerate() {
        if index > 0 {
            message.push_str("...\n");
        }
        message.push_str(&doc.render(*start, *end));
    }
    message.push_str(if merged.is_empty() { "Send the edit again, corrected." } else { "Send the edit again with these anchors." });
    Rejection { problems, message, error_kind }
}

/// Strip copied hashline prefixes (`12:ab|`) from replacement lines, but only when every non-empty
/// line carries one and each prefix's tag verifies against the rest of its line (a mechanical
/// proof that the prefix came from our own rendering, not from the file's content).
pub fn strip_copied_prefixes(lines: &[String]) -> Option<Vec<String>> {
    let mut out = Vec::with_capacity(lines.len());
    let mut stripped_any = false;
    for line in lines {
        let Some((head, rest)) = line.split_once('|') else {
            if line.is_empty() {
                out.push(String::new());
                continue;
            }
            return None;
        };
        let anchor = hashline::parse_anchor(head)?;
        if head.trim() != head || hashline::tag(rest).as_bytes() != anchor.tag {
            return None;
        }
        stripped_any = true;
        out.push(rest.to_owned());
    }
    stripped_any.then_some(out)
}

/// Lines of new text given either as a string or as an array of strings.
pub fn lines_from_text(text: &str) -> Vec<String> {
    text_to_lines(text)
}
