//! `tools.execute`: repair the model's raw call, then run read / edit / run for one session.

use crate::approval;
use crate::artifacts::{self, InlineLimits};
use crate::core::{Core, Runtime};
use crate::doc::{Doc, text_to_lines};
use crate::hashedit::{self, EditOp, LineRef, OpKind};
use crate::host::Host;
use crate::jobs::{StartRequest, Wake};
use crate::lang::{self, Lang};
use crate::pyenv::PyEnv;
use crate::read::{self, split_path_range};
use crate::repair;
use crate::sandbox;
use crate::schemas::EditFormat;
use crate::shell::{Captured, Identity, RunRequest};
use crate::textedit::{self, FileOp};
use crate::uris;
use crate::util;
use cc_protocol::ItemSink;
use cc_protocol::contracts::tools::{ExecContext, ExecuteParams, ToolOutcome, topics};
use serde_json::{Map, Value, json};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;

/// The result of one tool before it becomes a [`ToolOutcome`].
#[derive(Debug, Default)]
pub struct Out {
    pub ok: bool,
    pub output: String,
    pub display: Value,
    pub changed_files: Vec<String>,
    pub artifact: Option<String>,
    pub needs_approval: bool,
    pub error_kind: Option<String>,
    /// Diagnostic codes the result shows (language server diagnostics, a verifier's output): the
    /// skills declared for them are attached ([`crate::codeskills`]).
    pub code_hits: Vec<crate::codeskills::CodeHit>,
    /// Files an edit wrote, to check once the edit is done (see [`check_text`]).
    pub checks: Vec<Check>,
}

/// A file an edit wrote: what its syntax check found, for the result's check line.
#[derive(Debug)]
pub struct Check {
    pub absolute: PathBuf,
    pub display: String,
    /// The syntax errors the edit introduced; None when no grammar covers the file.
    pub syntax: Option<Vec<lang::SyntaxIssue>>,
    pub doc: Doc,
}

impl Out {
    fn ok(output: String) -> Out {
        Out { ok: true, output, display: Value::Null, ..Out::default() }
    }

    fn fail(kind: &str, output: String) -> Out {
        Out { ok: false, output, error_kind: Some(kind.to_owned()), display: Value::Null, ..Out::default() }
    }
}

pub struct Session<'a> {
    pub context: &'a ExecContext,
    pub workspace: PathBuf,
    /// Give commands a Python environment (the project's venv, or an automatic one).
    pub python_env: bool,
}

impl Session<'_> {
    fn display(&self, path: &Path) -> String {
        util::display_rel(path, &self.workspace)
    }
}

pub async fn execute(
    core: &Arc<Core>,
    host: &Arc<dyn Host>,
    params: ExecuteParams,
    items: ItemSink,
    cancel: CancellationToken,
) -> ToolOutcome {
    execute_in(core, host, params, items, cancel, true).await
}

/// [`execute`], choosing whether `run` gets a Python environment (the probe's scratch runs do not).
pub async fn execute_in(
    core: &Arc<Core>,
    host: &Arc<dyn Host>,
    params: ExecuteParams,
    items: ItemSink,
    cancel: CancellationToken,
    python_env: bool,
) -> ToolOutcome {
    let started = Instant::now();
    let ExecuteParams { context, call } = params;
    let session = Session { workspace: util::canonical_lenient(&util::expand_home(&context.workspace)), context: &context, python_env };
    let mut canonical = match repair::normalize(&call.name, &call.arguments) {
        Ok(canonical) => canonical,
        Err(error) => {
            let tool = error.tool.clone().unwrap_or_else(|| call.name.clone());
            let mut output = format!("Tool call not executed: {}.", error.message);
            // A cut-off call was right but long: listing the arguments again only invites a change.
            if error.kind != "truncated"
                && let Some(tool) = &error.tool
            {
                let _ = write!(output, "\n`{tool}` takes: {}.", repair::arguments_in(tool, edit_format(&context)));
            }
            output.push_str("\nSend the complete call again.");
            return ToolOutcome {
                ok: false,
                output,
                display: json!({ "repair_error": error }),
                tool,
                args: Value::String(call.arguments.clone()),
                intent: String::new(),
                changed_files: Vec::new(),
                artifact: None,
                needs_approval: false,
                duration_ms: started.elapsed().as_millis() as u64,
                error_kind: Some(format!("call_{}", error.kind)),
            };
        }
    };
    // A container project's files named by their container path (/testbed/x.py) are its files on
    // the Mac: read and edit map them (only paths that do not exist on the Mac; journaled only).
    if matches!(canonical.tool, "read" | "edit")
        && let Some(note) = map_container_paths(host, &session, &mut canonical.args).await
    {
        canonical.notes.push(note);
    }
    let intent = canonical.args.get("intent").and_then(Value::as_str).unwrap_or("").to_owned();
    let mut out = match canonical.tool {
        "read" => exec_read(core, host, &session, &canonical.args).await,
        "edit" => exec_edit(core, host, &session, &canonical.args).await,
        "terminal" => exec_terminal(host, &session, &canonical.args, &cancel).await,
        _ => exec_run(core, host, &session, &canonical.args, &items, cancel).await,
    };
    // An edit ends with one check of each file it changed: the syntax check's new errors and the
    // language server's view (type errors, not only syntax), as one list.
    let checks = std::mem::take(&mut out.checks);
    let several = checks.len() > 1;
    for check in checks {
        let (text, hits) = check_text(host.as_ref(), &session, check, several).await;
        if !text.is_empty() {
            let _ = write!(out.output, "{}{text}", if out.output.ends_with('\n') || out.output.is_empty() { "" } else { "\n" });
        }
        out.code_hits.extend(hits);
    }
    attach_code_skills(core, host.as_ref(), &session.context.session_id, &mut out).await;
    // What the repair changed in the call's meaning comes first (a rename says nothing).
    if !canonical.shown.is_empty() {
        let lines: String = canonical.shown.iter().map(|line| format!("[{line}]\n")).collect();
        out.output.insert_str(0, &lines);
    }
    if !canonical.notes.is_empty() {
        if let Value::Object(display) = &mut out.display {
            display.insert("repairs".to_owned(), json!(canonical.notes));
        } else if out.display.is_null() {
            out.display = json!({ "repairs": canonical.notes });
        }
    }
    ToolOutcome {
        ok: out.ok,
        output: out.output,
        display: out.display,
        tool: canonical.tool.to_owned(),
        args: Value::Object(canonical.args),
        intent,
        changed_files: out.changed_files,
        artifact: out.artifact,
        needs_approval: out.needs_approval,
        duration_ms: started.elapsed().as_millis() as u64,
        error_kind: out.error_kind,
    }
}

/// A diagnostic code a plugin declared a skill for: the skill comes with the result (its text the
/// first time in `session_id`, nothing afterwards). The model's reads, edits and runs and the host's
/// `tools.read` of the same session share that memory. The skill goes before the result, so the
/// failing output and its exit line stay last.
pub async fn attach_code_skills(core: &Core, host: &dyn Host, session_id: &str, out: &mut Out) {
    if out.code_hits.is_empty() {
        return;
    }
    let index = crate::codeskills::index(host).await;
    if let Some((text, uris)) = crate::codeskills::attach_with_uris(host, &index, &core.read_state, session_id, &out.code_hits).await {
        out.output.insert_str(0, &format!("{text}\n"));
        // Which skills the text carries (it names only the code): for the journal's readers.
        if !uris.is_empty() {
            if out.display.is_null() {
                out.display = json!({});
            }
            if let Value::Object(display) = &mut out.display {
                display.insert("skills".to_owned(), json!(uris));
            }
        }
    }
}

/// Offload model-facing text that exceeds the limit (full text kept as an artifact).
fn offload_text(core: &Core, text: String, limits: InlineLimits) -> (String, Option<String>) {
    if text.len() <= limits.inline {
        return (text, None);
    }
    match core.artifacts.store(text.as_bytes()) {
        Ok(id) => (artifacts::inline_view(text.as_bytes(), &id, limits), Some(id)),
        Err(_) => (text, None),
    }
}

// ---------------------------------------------------------------------------------------------
// read

pub async fn read_target(
    core: &Arc<Core>,
    host: &Arc<dyn Host>,
    session: &Session<'_>,
    target: &str,
    range: Option<&str>,
    record: bool,
    diagnostics: crate::langs::DiagnosticsRead,
) -> Out {
    let target = target.trim();
    if let Some(rest) = target.strip_prefix("artifact://") {
        let (id, suffix) = split_path_range(rest);
        let range = range.map(str::to_owned).or(suffix);
        return match core.artifacts.read(&id) {
            Ok(bytes) => match artifacts::render_range(&bytes, &id, range.as_deref(), core.config.read.outline_lines) {
                Ok(text) => {
                    let mut out = Out::ok(text);
                    out.display = json!({ "artifact": id, "bytes": bytes.len() });
                    out
                }
                Err(message) => Out::fail("bad_range", format!("artifact://{id}: {message}")),
            },
            Err(_) => Out::fail("not_found", format!("artifact://{id} does not exist; run the command again to see its output.")),
        };
    }
    if uris::is_uri(target) {
        if range.is_some() {
            return Out::fail("bad_range", format!("{target}: ranges apply to files and artifacts, not to this URI"));
        }
        // A skill whose text this session already has (chosen for the turn, or brought by a
        // diagnostic code): one line, not the text again (a re-read costs its whole length).
        if target.starts_with("skill://") && core.read_state.skill_attached(&session.context.session_id, target) {
            return Out::ok(format!("{target}: its text is already in this conversation (shown earlier in this session); use it."));
        }
        let scope = uris::Scope { project_id: &session.context.project_id, workspace: &session.workspace };
        // `graph://<project>/...`, `definition://<project>:...`: another project (crate::xproject).
        let mut code_hits = Vec::new();
        let result = match crate::xproject::read_uri(host.as_ref(), &session.workspace, &session.context.project_id, target, core.config.reference_wait).await {
            Some(result) => result,
            // The workspace's diagnostics: their codes can bring a skill along.
            None if uris::split_uri(target).is_some_and(|(scheme, _, _)| scheme == "diagnostics") => {
                let path = uris::split_uri(target).map(|(_, path, _)| uris::decode(path)).unwrap_or_default();
                match crate::langs::read_diagnostics(host.as_ref(), &session.workspace, &path, diagnostics).await {
                    Ok((text, hits)) => {
                        code_hits = hits;
                        Ok(text)
                    }
                    // No language plugin covers the file: nothing to wait for or retry.
                    Err(message) => Err(uris::UriError::new("not_covered", message)),
                }
            }
            None => uris::read(host.as_ref(), &scope, target).await,
        };
        return match result {
            Ok(text) => Out { code_hits, ..Out::ok(text) },
            Err(error) => Out::fail(error.kind, error.message),
        };
    }
    // `<project>:<path>[:range]`: a file of another project, by its absolute path.
    let qualified = match crate::xproject::read_target(host.as_ref(), &session.workspace, target).await {
        Some(Ok(path)) => Some(path),
        Some(Err(error)) => return Out::fail(error.kind, error.message),
        None => None,
    };
    let target = qualified.as_deref().unwrap_or(target);
    let mut path_text = target.to_owned();
    let mut range = range.map(str::to_owned);
    let mut absolute = util::resolve(&session.workspace, &path_text);
    if range.is_none() && !absolute.exists() {
        let (base, suffix) = split_path_range(&path_text);
        if suffix.is_some() {
            path_text = base;
            range = suffix;
            absolute = util::resolve(&session.workspace, &path_text);
        }
    }
    let display = session.display(&absolute);
    if crate::sandbox::read_denied(&session.workspace, &absolute) {
        return Out::fail("denied", format!("{display}: not readable from this workspace. Work from the files in the workspace."));
    }
    match read::read_file(&absolute, &display, range.as_deref(), &core.config.read) {
        Ok(view) => {
            if record && let Some(hash) = &view.hash {
                core.read_state.record_read(&session.context.session_id, &util::canonical_lenient(&absolute), hash, &view.lines);
            }
            // What the project knows about this file rides along with it (knowledge items bound
            // to the file or to any symbol in it; docs/PROJECT-KNOWLEDGE-PLAN.md P2), under the
            // header and before the code, so the code stays next to what the model does with it.
            let mut text = view.text;
            if qualified.is_none() {
                let scope = uris::Scope { project_id: &session.context.project_id, workspace: &session.workspace };
                let known: String = uris::knowledge_lines(host.as_ref(), &scope, &[], std::slice::from_ref(&display))
                    .await
                    .iter()
                    .map(|line| format!("knowledge: {line}\n"))
                    .collect();
                if !known.is_empty() {
                    let at = text.find('\n').map(|index| index + 1).unwrap_or(text.len());
                    text.insert_str(at, &known);
                }
            }
            let mut out = Out::ok(text);
            out.display = json!({ "path": display, "mode": view.mode, "total_lines": view.total_lines });
            out
        }
        Err(error) => {
            let mut message = error.message;
            if error.kind == "not_found" {
                match absolute.parent().filter(|parent| parent.is_dir()) {
                    // What is there instead: the parent's entries (not the ignored ones).
                    Some(parent) => {
                        let (mut names, _) = read::visible_entries(parent).unwrap_or_default();
                        let more = names.len().saturating_sub(MISSING_FILE_ENTRIES);
                        names.truncate(MISSING_FILE_ENTRIES);
                        let listing = if names.is_empty() { "nothing".to_owned() } else { names.join(", ") };
                        let _ = write!(message, " {} holds: {listing}", read::dir_label(&session.display(parent)));
                        if more > 0 {
                            let _ = write!(message, " and {more} more");
                        }
                        message.push('.');
                    }
                    None => message.push_str(
                        " Find it with graph://search/<words>, or run `git ls-files | grep <part of the name>`.",
                    ),
                }
            }
            Out::fail(error.kind, message)
        }
    }
}

/// At most this many entries of the parent directory come with a missing file.
const MISSING_FILE_ENTRIES: usize = 30;

async fn exec_read(core: &Arc<Core>, host: &Arc<dyn Host>, session: &Session<'_>, args: &Map<String, Value>) -> Out {
    let range = args.get("range").and_then(Value::as_str);
    let targets: Vec<String> = match (args.get("path"), args.get("paths")) {
        (Some(Value::String(path)), _) => vec![path.clone()],
        (_, Some(Value::Array(paths))) => paths.iter().filter_map(Value::as_str).map(str::to_owned).collect(),
        _ => return Out::fail("call_missing_argument", "read needs `path`".to_owned()),
    };
    let mut combined = Out::ok(String::new());
    let mut displays = Vec::new();
    for (index, target) in targets.iter().enumerate() {
        let out = read_target(core, host, session, target, range, true, Default::default()).await;
        if index > 0 {
            combined.output.push('\n');
        }
        combined.output.push_str(&out.output);
        combined.code_hits.extend(out.code_hits.iter().cloned());
        if !out.ok {
            combined.ok = false;
            if combined.error_kind.is_none() {
                combined.error_kind = out.error_kind.clone();
            }
        }
        displays.push(out.display);
    }
    let (text, artifact) = offload_text(core, std::mem::take(&mut combined.output), core.config.read_limits);
    combined.output = text;
    combined.artifact = artifact;
    combined.display = if displays.len() == 1 { displays.remove(0) } else { Value::Array(displays) };
    combined
}

// ---------------------------------------------------------------------------------------------
// edit

fn resolve_in_workspace(session: &Session<'_>, path: &str) -> Result<PathBuf, Box<Out>> {
    let absolute = util::resolve(&session.workspace, path);
    if !util::is_inside(&absolute, &session.workspace) {
        return Err(Box::new(Out::fail("outside_workspace", format!("{path} is outside the workspace; edit only files inside it."))));
    }
    if absolute.is_dir() {
        return Err(Box::new(Out::fail("is_directory", format!("{path} is a directory"))));
    }
    Ok(util::canonical_lenient(&absolute))
}

/// Current text of a file: Ok(None) when it does not exist.
fn current_text(absolute: &Path, display: &str) -> Result<Option<String>, Box<Out>> {
    match std::fs::read(absolute) {
        Ok(bytes) => {
            if read::is_binary(&bytes) {
                return Err(Box::new(Out::fail("not_text", format!("{display} is not a UTF-8 text file; it cannot be edited as text"))));
            }
            Ok(Some(String::from_utf8_lossy(&bytes).into_owned()))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(Box::new(Out::fail("io_error", format!("{display}: {error}")))),
    }
}

struct Commit {
    absolute: PathBuf,
    display: String,
    old: Option<String>,
    new: String,
    /// Some for line-level edits (anchor/replace/patch), None for whole-file writes.
    line_map: Option<Vec<Option<usize>>>,
    changed: Vec<(usize, usize)>,
    notes: Vec<String>,
}

/// Syntax errors introduced by the change (tree-sitter, structural): None when no grammar covers
/// the file; with their record for the display.
fn syntax_check(path: &Path, old: &str, new: &str) -> (Option<Vec<lang::SyntaxIssue>>, Value) {
    let Some(lang) = Lang::detect(path, new) else {
        return (None, Value::Null);
    };
    match lang::introduced_issues(lang, old, new) {
        Some(issues) => {
            let errors: Vec<Value> = issues
                .iter()
                .map(|issue| json!({"line": issue.line, "column": issue.column, "kind": issue.kind, "detail": issue.detail}))
                .collect();
            (Some(issues), json!({"language": lang.name(), "errors": errors}))
        }
        None => (None, Value::Null),
    }
}

/// The check line(s) of an edited file: "No errors." when the syntax check and the language server
/// are both clean; else one list, the syntax check's new errors first and the language server's
/// for the other lines, each with its anchored line. Empty when nothing checks the file.
pub async fn check_text(host: &dyn Host, session: &Session<'_>, check: Check, several: bool) -> (String, Vec<crate::codeskills::CodeHit>) {
    let server = crate::langs::after_edit(host, &session.workspace, &check.absolute, &check.display).await;
    if check.syntax.is_none() && server.is_none() {
        return (String::new(), Vec::new());
    }
    let syntax = check.syntax.unwrap_or_default();
    // (line, text, is an error)
    let mut items: Vec<(usize, String, bool)> = syntax.iter().map(|issue| (issue.line, issue.detail.clone(), true)).collect();
    let (mut partial, mut hits) = (false, Vec::new());
    if let Some(server) = server {
        partial = server.partial;
        hits = server.hits;
        for diagnostic in &server.items {
            let line = diagnostic.line as usize;
            if syntax.iter().any(|issue| issue.line == line) {
                continue;
            }
            let code = diagnostic.code.as_deref().map(|code| format!(" [{code}]")).unwrap_or_default();
            let message = crate::langs::message_text(&diagnostic.message);
            let error = diagnostic.severity == "error";
            items.push((line, if error { format!("{message}{code}") } else { format!("warning: {message}{code}") }, error));
        }
    }
    let partial = if partial { crate::langs::PARTIAL } else { "" };
    let label = if several { format!("{}: ", check.display) } else { String::new() };
    if items.is_empty() {
        return (format!("{label}No errors{partial}.\n"), hits);
    }
    let errors = items.iter().filter(|(_, _, error)| *error).count();
    let mut text = format!("{label}{}{partial}:\n", crate::langs::counts(errors, items.len() - errors));
    for (line, message, _) in items.iter().take(crate::langs::EDIT_DIAGNOSTICS_SHOWN) {
        let _ = writeln!(text, "line {line}: {message}");
        if (1..=check.doc.len()).contains(line) {
            text.push_str(&check.doc.render(*line, *line));
        }
    }
    if items.len() > crate::langs::EDIT_DIAGNOSTICS_SHOWN {
        let _ = writeln!(text, "... {} more (read diagnostics://{})", items.len() - crate::langs::EDIT_DIAGNOSTICS_SHOWN, check.display);
    }
    (text, hits)
}

/// The old lines an edit removed that do not reappear in its changed regions, as they were (at
/// most 15, by their old line number); empty when nothing was lost.
fn render_removed(old: &str, map: &[Option<usize>], doc: &Doc, changed: &[(usize, usize)]) -> String {
    let written: std::collections::HashSet<&str> =
        changed.iter().flat_map(|&(start, end)| doc.lines[start.min(doc.len())..end.min(doc.len())].iter().map(|line| line.trim())).collect();
    let gone: Vec<(usize, &str)> = old
        .lines()
        .enumerate()
        .filter(|(index, line)| map.get(*index).is_some_and(Option::is_none) && !line.trim().is_empty() && !written.contains(line.trim()))
        .collect();
    if gone.is_empty() {
        return String::new();
    }
    let mut out = String::from("Removed by this edit (no longer in the file):\n");
    for (index, line) in gone.iter().take(15) {
        let _ = writeln!(out, "{}  {}", index + 1, line);
    }
    if gone.len() > 15 {
        let _ = writeln!(out, "... and {} more", gone.len() - 15);
    }
    out
}

/// Each changed range (0-based, half-open) widened to the innermost definition holding it when
/// that definition has at most 80 lines; true when any was widened.
fn widen_to_definitions(path: &Path, text: &str, changed: &[(usize, usize)]) -> (Vec<(usize, usize)>, bool) {
    const MAX: usize = 80;
    let Some(items) = crate::lang::Lang::detect(path, text).and_then(|lang| crate::lang::outline(lang, text)) else { return (changed.to_vec(), false) };
    fn innermost(items: &[crate::lang::Item], first: usize, last: usize) -> Option<(usize, usize)> {
        items.iter().find(|item| item.start <= first && item.end >= last).map(|item| {
            innermost(&item.children, first, last).unwrap_or((item.start, item.end))
        })
    }
    let mut whole = false;
    let shown = changed
        .iter()
        .map(|&(start, end)| {
            let last = end.saturating_sub(1).max(start);
            match innermost(&items, start, last) {
                Some((first, end_row)) if end_row + 1 - first <= MAX => {
                    whole = true;
                    (first, end_row + 1)
                }
                _ => (start, end),
            }
        })
        .collect();
    (shown, whole)
}

fn render_regions(doc: &Doc, changed: &[(usize, usize)]) -> String {
    let total = doc.len();
    if total == 0 {
        return "(the file is now empty)\n".to_owned();
    }
    let mut regions: Vec<(usize, usize)> = changed
        .iter()
        .map(|(start, end)| {
            let first = start.saturating_sub(2) + 1;
            let last = (end.max(&(start + 1)) + 2).min(total);
            (first.min(total), last)
        })
        .collect();
    regions.sort();
    let mut merged: Vec<(usize, usize)> = Vec::new();
    for (start, end) in regions {
        match merged.last_mut() {
            Some(last) if start <= last.1 + 1 => last.1 = last.1.max(end),
            _ => merged.push((start, end)),
        }
    }
    let mut out = String::new();
    for (index, (start, end)) in merged.iter().enumerate() {
        if index > 0 {
            out.push_str("...\n");
        }
        out.push_str(&doc.render(*start, *end));
    }
    out
}

fn unified_diff(old: &str, new: &str, path: &str) -> String {
    similar::TextDiff::from_lines(old, new).unified_diff().context_radius(3).header(&format!("a/{path}"), &format!("b/{path}")).to_string()
}

fn commit(core: &Core, host: &Arc<dyn Host>, session: &Session<'_>, commit: Commit) -> Out {
    let Commit { absolute, display, old, new, line_map, changed, notes } = commit;
    let old_text = old.clone().unwrap_or_default();
    if old.as_deref() == Some(new.as_str()) {
        let mut out = Out::ok(format!("No change: {display} already has this content.\n"));
        out.display = json!({ "path": display, "unchanged": true });
        return out;
    }
    // An edit of an existing file that adds syntax errors is not written (SWE-agent's guard: a broken
    // edit cascades, the next edits pile onto it). The model gets the errors and what it proposed;
    // the file and its anchors stay as they were.
    if let (Some(_), Some(_)) = (&old, &line_map) {
        let (introduced, _) = syntax_check(&absolute, &old_text, &new);
        if let Some(issues) = introduced.filter(|issues| !issues.is_empty()) {
            let proposed = Doc::parse(&new);
            let (shown, _) = widen_to_definitions(&absolute, &new, &changed);
            let mut text = format!("Nothing was written: this edit would add a syntax error to {display}.\n");
            for issue in issues.iter().take(5) {
                let _ = writeln!(text, "- line {}: {}", issue.line, issue.detail);
            }
            text.push_str("What the edit would make it (not in the file):\n");
            for &(start, end) in &shown {
                for line in start.saturating_sub(1)..(end + 1).min(proposed.len()) {
                    let _ = writeln!(text, "{}  {}", line + 1, proposed.lines[line]);
                }
            }
            text.push_str("Send the edit again with the error fixed; the anchors from your last read still hold.\n");
            return Out::fail("syntax_error", text);
        }
    }
    if let Err(error) = util::atomic_write(&absolute, new.as_bytes()) {
        return Out::fail("io_error", format!("{display}: {error}"));
    }
    let new_hash = util::content_hash(new.as_bytes());
    let doc = Doc::parse(&new);
    match (&old, &line_map) {
        (Some(old), Some(map)) => {
            let old_hash = util::content_hash(old.as_bytes());
            core.read_state.record_edit(&session.context.session_id, &absolute, &old_hash, &new_hash, Some(map));
        }
        _ => core.read_state.record_write(&session.context.session_id, &absolute, &new_hash, &doc.lines),
    }
    let verb = match &old {
        None => "Created",
        Some(_) if line_map.is_none() => "Wrote",
        Some(_) => "Edited",
    };
    let mut output = String::new();
    if line_map.is_some() {
        let anchors = if edit_format(session.context) == EditFormat::Hashline { ", with new anchors" } else { "" };
        // A change inside a function shows that whole function as it now is: the model compares
        // its intent with the real function, never with fragments it pieced together.
        let (shown, whole) = widen_to_definitions(&absolute, &new, &changed);
        let what = if whole { "Each changed function, whole, as it is now" } else { "Changed lines and 2 around them" };
        let _ = writeln!(output, "{verb} {display}. {what}{anchors}:");
        output.push_str(&render_regions(&doc, &shown));
        // Lines the edit took out whose text is nowhere in what it wrote: a replace over the wrong
        // range shows here, not only when a run fails later (seen: a branch replaced away).
        if let (Some(old), Some(map)) = (&old, &line_map) {
            output.push_str(&render_removed(old, map, &doc, &changed));
        }
    } else if doc.len() <= 200 {
        let _ = writeln!(output, "{verb} {display}:");
        output.push_str(&doc.render(1, doc.len()));
    } else {
        // The model just wrote it: its shape, not how to read it.
        let _ = writeln!(output, "{verb} {display}.");
        output.push_str(&read::render_outline(&absolute, &display, &new, &doc, false));
    }
    let (syntax_issues, syntax) = syntax_check(&absolute, &old_text, &new);
    host.publish(
        topics::FILES_CHANGED,
        json!({ "workspace": session.context.workspace, "paths": [display.clone()], "session_id": session.context.session_id }),
    );
    let mut out = Out::ok(output);
    out.changed_files = vec![display.clone()];
    out.display = json!({
        "path": display.clone(),
        "created": old.is_none(),
        "diff": unified_diff(&old_text, &new, &display),
        "syntax": syntax,
        "notes": notes,
    });
    out.checks = vec![Check { absolute, display, syntax: syntax_issues, doc }];
    out
}

fn line_ref(value: &Value) -> Option<LineRef> {
    match value {
        Value::Number(number) if number.as_u64() == Some(0) => Some(LineRef::Start),
        Value::String(text) => LineRef::parse(text),
        _ => None,
    }
}

fn parse_ops(edits: &[Value]) -> Result<(Vec<EditOp>, Vec<String>), String> {
    let mut ops = Vec::with_capacity(edits.len());
    let mut notes = Vec::new();
    for (index, edit) in edits.iter().enumerate() {
        let kind =
            edit.get("op").and_then(Value::as_str).and_then(OpKind::parse).ok_or_else(|| format!("edit #{}: unknown op", index + 1))?;
        // `line` is only produced by the str_replace_editor `insert` mapping (a line number of the
        // file as the session last read it).
        let at = match (edit.get("at"), edit.get("line").and_then(Value::as_u64)) {
            (Some(at), _) => line_ref(at).ok_or_else(|| format!("edit #{}: `at` is not an anchor like \"12:ab\"", index + 1))?,
            (None, Some(0)) => LineRef::Start,
            (None, Some(line)) => LineRef::Bare(line as usize),
            (None, None) => return Err(format!("edit #{}: missing `at`", index + 1)),
        };
        let to = match edit.get("to") {
            Some(value) => Some(line_ref(value).ok_or_else(|| format!("edit #{}: `to` is not an anchor like \"14:cd\"", index + 1))?),
            None => None,
        };
        let mut lines = match (kind, edit.get("text").and_then(Value::as_str)) {
            (OpKind::Delete, _) => Vec::new(),
            (_, Some(text)) => text_to_lines(text),
            (_, None) => return Err(format!("edit #{}: `{}` needs `text`", index + 1, kind.name())),
        };
        // Copied N:hh| prefixes come off silently (journaled in the display).
        if let Some(stripped) = hashedit::strip_copied_prefixes(&lines) {
            lines = stripped;
            let note = "removed N:hh| prefixes copied into the new text".to_owned();
            if !notes.contains(&note) {
                notes.push(note);
            }
        }
        ops.push(EditOp { kind, at, to, lines });
    }
    Ok((ops, notes))
}

async fn exec_edit(core: &Arc<Core>, host: &Arc<dyn Host>, session: &Session<'_>, args: &Map<String, Value>) -> Out {
    if let Some(patch) = args.get("patch").and_then(Value::as_str) {
        return apply_patch(core, host, session, patch).await;
    }
    let Some(path) = args.get("path").and_then(Value::as_str) else {
        return Out::fail("call_missing_argument", "edit needs `path`".to_owned());
    };
    let absolute = match resolve_in_workspace(session, path) {
        Ok(path) => path,
        Err(out) => return *out,
    };
    let display = session.display(&absolute);
    let lock = core.file_lock(&absolute);
    let _guard = lock.lock().await;
    let current = match current_text(&absolute, &display) {
        Ok(current) => current,
        Err(out) => return *out,
    };
    let session_id = &session.context.session_id;

    // Both `content` and `edits` sent (the two forms describe the same change): the whole content
    // where a whole write is allowed (a new file, or a stub not changed yet), else the edits.
    let rewrite_refused = |existing: &Option<String>| {
        existing.as_ref().is_some_and(|text| !text.trim().is_empty())
            && edit_format(session.context) != EditFormat::Whole
            && core.read_state.has_changed(session_id, &absolute)
    };
    let content = args.get("content").and_then(Value::as_str).filter(|_| !args.contains_key("edits") || !rewrite_refused(&current));
    if let Some(content) = content {
        // `content` writes a file whole once: a new file, or a stub the session has not changed yet.
        // A file the session already changed is changed by anchored edits, never written whole
        // again (a rewrite throws away what worked and hides what changed). The task step says so
        // up front; the refusal carries the file with its anchors for the next call.
        if let Some(existing) = &current
            && !existing.trim().is_empty()
            && edit_format(session.context) != EditFormat::Whole
            && core.read_state.has_changed(session_id, &absolute)
        {
            let view = read_target(core, host, session, &display, None, true, Default::default()).await;
            let (text, artifact) = offload_text(core, view.output, core.config.read_limits);
            let how = match edit_format(session.context) {
                EditFormat::Replace => "with old/new replacements",
                _ => "with anchored edits, every change to it in one call",
            };
            let mut text = text;
            if !text.ends_with('\n') {
                text.push('\n');
            }
            let mut out = Out::fail(
                "whole_rewrite",
                format!("Nothing was written: you already changed {display}, so it is not written whole again. Its current content:\n{text}Change the parts that need changing {how}."),
            );
            out.display = view.display;
            out.artifact = artifact;
            return out;
        }
        if let Some(existing) = &current {
            let hash = util::content_hash(existing.as_bytes());
            if !core.read_state.has_seen(session_id, &absolute, &hash) {
                let reason = if core.read_state.has_read_any(session_id, &absolute) {
                    "it changed since you read it"
                } else {
                    "you have not read it"
                };
                // The refusal carries the read: the file's current content (which this session has
                // now seen), so the next call can send the whole new content or anchored edits
                // without a separate read step.
                let view = read_target(core, host, session, &display, None, true, Default::default()).await;
                let (text, artifact) = offload_text(core, view.output, core.config.read_limits);
                let other = match edit_format(session.context) {
                    EditFormat::Hashline => ", or change it with anchored edits",
                    EditFormat::Replace => ", or change it with old/new",
                    EditFormat::Whole => "",
                };
                let mut text = text;
                if !text.ends_with('\n') {
                    text.push('\n');
                }
                let mut out = Out::fail(
                    "not_read",
                    format!("Not overwritten: {display} exists and {reason}. Its current content:\n{text}Send the whole new content again{other}."),
                );
                out.display = view.display;
                out.artifact = artifact;
                return out;
            }
        }
        return commit(
            core,
            host,
            session,
            Commit { absolute, display, old: current, new: content.to_owned(), line_map: None, changed: Vec::new(), notes: Vec::new() },
        );
    }

    if let Some(edits) = args.get("edits").and_then(Value::as_array) {
        let (ops, mut notes) = match parse_ops(edits) {
            Ok(parsed) => parsed,
            Err(message) => return Out::fail("bad_edit", message),
        };
        let text = match &current {
            Some(text) => text.clone(),
            None => {
                if ops.iter().all(|op| op.kind == OpKind::InsertAfter && op.at == LineRef::Start) {
                    String::new()
                } else {
                    return Out::fail("not_found", format!("{display} does not exist; create it with `content`."));
                }
            }
        };
        let doc = Doc::parse(&text);
        let hash = util::content_hash(text.as_bytes());
        let view = core.read_state.view(session_id, &absolute, &hash);
        return match hashedit::apply(&doc, view.as_ref(), &ops) {
            Ok(applied) => {
                // Anchors mapped through the session's own edits are journaled, not told.
                if !applied.relocations.is_empty() {
                    notes
                        .push(format!("anchors from your earlier read, mapped through your own edits: {}", applied.relocations.join(", ")));
                }
                let new = applied.doc.to_text();
                let old = current.map(|_| text);
                commit(
                    core,
                    host,
                    session,
                    Commit { absolute, display, old, new, line_map: Some(applied.line_map), changed: applied.changed, notes },
                )
            }
            Err(rejection) => {
                let mut out = Out::fail(rejection.error_kind, rejection.message);
                out.display = json!({ "path": display, "problems": format!("{:?}", rejection.problems) });
                out
            }
        };
    }

    let replacements: Vec<(String, String, bool)> = if let Some(items) = args.get("replacements").and_then(Value::as_array) {
        items
            .iter()
            .map(|item| {
                (
                    item.get("old").and_then(Value::as_str).unwrap_or("").to_owned(),
                    item.get("new").and_then(Value::as_str).unwrap_or("").to_owned(),
                    item.get("all").and_then(Value::as_bool).unwrap_or(false),
                )
            })
            .collect()
    } else if let (Some(old), Some(new)) = (args.get("old").and_then(Value::as_str), args.get("new").and_then(Value::as_str)) {
        vec![(old.to_owned(), new.to_owned(), args.get("all").and_then(Value::as_bool).unwrap_or(false))]
    } else {
        return Out::fail("call_missing_argument", "edit needs edits, old/new, replacements, content or patch".to_owned());
    };
    let Some(text) = current else {
        return Out::fail("not_found", format!("{display} does not exist; create it with `content`."));
    };
    match textedit::replace(&display, &text, &replacements) {
        Ok(change) => commit(
            core,
            host,
            session,
            Commit {
                absolute,
                display,
                old: Some(text),
                new: change.text,
                line_map: Some(change.line_map),
                changed: change.changed,
                notes: change.notes,
            },
        ),
        Err(error) => Out::fail(error.kind, error.message),
    }
}

async fn apply_patch(core: &Arc<Core>, host: &Arc<dyn Host>, session: &Session<'_>, patch: &str) -> Out {
    let ops = match textedit::parse_patch(patch) {
        Ok(ops) => ops,
        Err(error) => return Out::fail(error.kind, format!("{}. Nothing was written.", error.message)),
    };
    // Plan every file first; write only if all of them apply.
    enum Planned {
        Write {
            absolute: PathBuf,
            display: String,
            old: Option<String>,
            new: String,
            line_map: Option<Vec<Option<usize>>>,
            changed: Vec<(usize, usize)>,
            notes: Vec<String>,
        },
        Remove {
            absolute: PathBuf,
            display: String,
        },
    }
    // Lock every file the patch touches (in a fixed order), then plan, then write: a patch is
    // checked against the files as they are while it holds the locks.
    let mut targets = Vec::new();
    for op in &ops {
        let path = match op {
            FileOp::Add { path, .. } | FileOp::Delete { path } | FileOp::Update { path, .. } => path,
        };
        targets.push(match resolve_in_workspace(session, path) {
            Ok(path) => path,
            Err(out) => return *out,
        });
        if let FileOp::Update { move_to: Some(target), .. } = op {
            match resolve_in_workspace(session, target) {
                Ok(path) => targets.push(path),
                Err(out) => return *out,
            }
        }
    }
    let mut lock_order = targets.clone();
    lock_order.sort();
    lock_order.dedup();
    let locks: Vec<_> = lock_order.iter().map(|path| core.file_lock(path)).collect();
    let mut guards = Vec::with_capacity(locks.len());
    for lock in &locks {
        guards.push(lock.lock().await);
    }
    let mut plans = Vec::new();
    for op in &ops {
        let path = match op {
            FileOp::Add { path, .. } | FileOp::Delete { path } | FileOp::Update { path, .. } => path,
        };
        let absolute = match resolve_in_workspace(session, path) {
            Ok(path) => path,
            Err(out) => return *out,
        };
        let display = session.display(&absolute);
        let current = match current_text(&absolute, &display) {
            Ok(current) => current,
            Err(out) => return *out,
        };
        match op {
            FileOp::Add { content, .. } => {
                if current.is_some() {
                    return Out::fail(
                        "exists",
                        format!("patch adds {display}, but it already exists (use an Update File section). Nothing was written."),
                    );
                }
                plans.push(Planned::Write {
                    absolute,
                    display,
                    old: None,
                    new: content.clone(),
                    line_map: None,
                    changed: Vec::new(),
                    notes: Vec::new(),
                });
            }
            FileOp::Delete { .. } => {
                if current.is_none() {
                    return Out::fail("not_found", format!("patch deletes {display}, which does not exist. Nothing was written."));
                }
                plans.push(Planned::Remove { absolute, display });
            }
            FileOp::Update { move_to, hunks, .. } => {
                let Some(text) = current else {
                    return Out::fail("not_found", format!("patch updates {display}, which does not exist. Nothing was written."));
                };
                let change = if hunks.is_empty() {
                    textedit::TextChange {
                        text: text.clone(),
                        line_map: (0..Doc::parse(&text).len()).map(Some).collect(),
                        changed: Vec::new(),
                        notes: Vec::new(),
                    }
                } else {
                    match textedit::apply_hunks(&display, &text, hunks) {
                        Ok(change) => change,
                        Err(error) => return Out::fail(error.kind, format!("{}. Nothing was written.", error.message)),
                    }
                };
                match move_to {
                    Some(target) => {
                        let target_abs = match resolve_in_workspace(session, target) {
                            Ok(path) => path,
                            Err(out) => return *out,
                        };
                        let target_display = session.display(&target_abs);
                        plans.push(Planned::Write {
                            absolute: target_abs,
                            display: target_display,
                            old: None,
                            new: change.text,
                            line_map: None,
                            changed: Vec::new(),
                            notes: change.notes,
                        });
                        plans.push(Planned::Remove { absolute, display });
                    }
                    None => plans.push(Planned::Write {
                        absolute,
                        display,
                        old: Some(text),
                        new: change.text,
                        line_map: Some(change.line_map),
                        changed: change.changed,
                        notes: change.notes,
                    }),
                }
            }
        }
    }
    let mut combined = Out::ok(String::new());
    let mut displays = Vec::new();
    for plan in plans {
        match plan {
            Planned::Write { absolute, display, old, new, line_map, changed, notes } => {
                let out = commit(core, host, session, Commit { absolute, display, old, new, line_map, changed, notes });
                combined.output.push_str(&out.output);
                combined.changed_files.extend(out.changed_files);
                combined.checks.extend(out.checks);
                if !out.ok {
                    combined.ok = false;
                    combined.error_kind = out.error_kind;
                }
                displays.push(out.display);
            }
            Planned::Remove { absolute, display } => match std::fs::remove_file(&absolute) {
                Ok(()) => {
                    let _ = writeln!(combined.output, "Deleted {display}.");
                    combined.changed_files.push(display.clone());
                    host.publish(topics::FILES_CHANGED, json!({ "workspace": session.context.workspace, "paths": [display.clone()], "session_id": session.context.session_id }));
                    displays.push(json!({ "path": display, "deleted": true }));
                }
                Err(error) => {
                    combined.ok = false;
                    combined.error_kind = Some("io_error".to_owned());
                    let _ = writeln!(combined.output, "Could not delete {display}: {error}");
                }
            },
        }
    }
    drop(guards);
    combined.display = json!({ "files": displays });
    combined
}

// ---------------------------------------------------------------------------------------------
// run

pub fn identity(core: &Core, session: &Session<'_>, runtime: Option<&Runtime>) -> Identity {
    Identity {
        session_id: session.context.session_id.clone(),
        project_id: session.context.project_id.clone(),
        workspace: session.workspace.clone(),
        home: core.home.clone(),
        sandbox: runtime.map(|runtime| runtime.sandbox.clone()),
        venv: runtime.and_then(|runtime| runtime.venv.as_ref()).map(|venv| venv.path().to_path_buf()),
    }
}

/// Model-facing text and artifact for captured command output.
pub fn present_output(core: &Core, captured: Captured, limits: InlineLimits) -> (String, Option<String>, u64) {
    match captured {
        Captured::Mem(bytes) => {
            let total = bytes.len() as u64;
            if bytes.len() <= limits.inline {
                return (String::from_utf8_lossy(&bytes).into_owned(), None, total);
            }
            match core.artifacts.store(&bytes) {
                Ok(id) => (artifacts::inline_view(&bytes, &id, limits), Some(id), total),
                Err(_) => (String::from_utf8_lossy(&bytes).into_owned(), None, total),
            }
        }
        Captured::File { path, head, tail, total, newlines } => {
            let id = core.artifacts.adopt(&path).unwrap_or_else(|_| path.to_string_lossy().into_owned());
            let head_text = String::from_utf8_lossy(&head[..head.len().min(limits.head)]).into_owned();
            let tail_start = tail.len().saturating_sub(limits.tail);
            let tail_text = String::from_utf8_lossy(&tail[tail_start..]).into_owned();
            let head_cut = head_text.rfind('\n').map(|index| index + 1).unwrap_or(head_text.len());
            let tail_cut = tail_text.find('\n').map(|index| index + 1).unwrap_or(0);
            let (head, tail) = (&head_text[..head_cut], &tail_text[tail_cut..]);
            let head_lines = head.lines().count();
            let omitted = (newlines as usize).saturating_sub(head_lines + tail.lines().count());
            let text = format!("{head}{}{tail}", artifacts::omitted_marker(&id, head_lines, omitted));
            (text, Some(id), total)
        }
    }
}

/// The paths named on the lines of captured output that report a refused write
/// (see [`sandbox::eperm_paths`]).
fn refused_paths(captured: &Captured) -> Vec<PathBuf> {
    match captured {
        Captured::Mem(bytes) => sandbox::eperm_paths(bytes),
        Captured::File { head, tail, .. } => {
            let mut paths = sandbox::eperm_paths(head);
            paths.extend(sandbox::eperm_paths(tail));
            paths
        }
    }
}

/// The directories commands may write, in words, for model-facing explanations.
const WRITABLE: &str = "the workspace, temp dirs, package caches, your Python venv";

/// The end of a run's result: its exit (the time only when it took over 10 s).
fn exit_line(code: i32, duration_ms: u64) -> String {
    if duration_ms > 10_000 { format!("[exit {code}, {}s]", duration_ms / 1000) } else { format!("[exit {code}]") }
}

/// A command that ran out of time.
fn timeout_line(timeout_ms: u64) -> String {
    format!(
        "[timed out after {} s; the command and everything it started were killed. If it waits for input or never ends, fix that; run a server or watcher with background: true.]",
        timeout_ms.div_ceil(1000)
    )
}

/// How a background job was started, in one message (the same for the Mac and a container).
fn started_line(job_id: &str, artifact: &str, wake: &Wake) -> String {
    let mut when = vec!["it exits".to_owned()];
    if let Some(pattern) = &wake.pattern {
        when.push(format!("a line of its output matches /{pattern}/"));
    }
    if let Some(silence) = wake.silence_s {
        when.push(format!("it prints nothing for {silence} s"));
    }
    let when = match when.len() {
        1 => when.remove(0),
        _ => {
            let last = when.pop().unwrap_or_default();
            format!("{}, or when {last}", when.join(", when "))
        }
    };
    format!("Started background job {job_id}. You are woken when {when}. Its output so far: read artifact://{artifact}. Stop it: `cx job kill {job_id}`.\n")
}

/// Paths of `write_outside` (absolute, or relative to the command's directory).
fn requested_writes(args: &Map<String, Value>, cwd: &Path) -> Vec<PathBuf> {
    let values: Vec<&str> = match args.get("write_outside") {
        Some(Value::String(path)) => vec![path.as_str()],
        Some(Value::Array(paths)) => paths.iter().filter_map(Value::as_str).collect(),
        _ => Vec::new(),
    };
    values
        .into_iter()
        .map(str::trim)
        .filter(|path| !path.is_empty())
        .map(|path| util::canonical_lenient(&util::resolve(cwd, path)))
        .collect()
}

// ---------------------------------------------------------------------------------------------
// terminal

/// Default and longest wait for a terminal's screen.
const TERMINAL_WAIT_MS: u64 = 10_000;
const TERMINAL_WAIT_MAX_MS: u64 = 600_000;
/// A screen unchanged for this long is taken as the program waiting.
const TERMINAL_SETTLE_MS: u64 = 600;
const TERMINAL_POLL_MS: u64 = 100;

async fn exec_terminal(host: &Arc<dyn Host>, session: &Session<'_>, args: &Map<String, Value>, cancel: &CancellationToken) -> Out {
    let action = args.get("action").and_then(Value::as_str).unwrap_or("");
    let id = args.get("id").and_then(Value::as_str).unwrap_or("").to_owned();
    let wait_ms = args.get("wait_ms").and_then(Value::as_u64).unwrap_or(TERMINAL_WAIT_MS).min(TERMINAL_WAIT_MAX_MS);
    let until = match args.get("until").and_then(Value::as_str).map(regex::Regex::new).transpose() {
        Ok(until) => until,
        Err(error) => return Out::fail("bad_argument", format!("`until` is not a valid regex: {error}")),
    };
    let unavailable = |_: cc_protocol::RpcError| Out::fail("unavailable", "the terminal is not available; use run.".to_owned());
    let term_id = match action {
        "open" => {
            let workspace = session.workspace.display().to_string();
            let program = args.get("program").and_then(Value::as_str).filter(|p| !p.trim().is_empty());
            // A project that runs in a container gets its terminals there.
            let container = match host.call("container.status", json!({"root": workspace})).await {
                Ok(Value::Object(info)) if info.get("state").and_then(Value::as_str) == Some("running") => {
                    match host.call("container.shell_command", json!({"root": workspace})).await {
                        Ok(value) => value.get("argv").and_then(Value::as_array).map(|argv| argv.iter().filter_map(Value::as_str).map(str::to_owned).collect::<Vec<_>>()),
                        Err(error) => return Out::fail("unavailable", format!("the project's container gives no shell: {}", error.message)),
                    }
                }
                _ => None,
            };
            let (shell, shell_args): (Option<String>, Option<Vec<String>>) = match (container, program) {
                (Some(mut argv), program) if !argv.is_empty() => {
                    let shell = argv.remove(0);
                    if let Some(program) = program {
                        argv.extend(["-c".to_owned(), program.to_owned()]);
                    }
                    (Some(shell), Some(argv))
                }
                (_, Some(program)) => (Some("/bin/bash".to_owned()), Some(vec!["-l".to_owned(), "-c".to_owned(), program.to_owned()])),
                (_, None) => (None, None),
            };
            let opened = host.call("terminal.open", json!({"cwd": workspace, "cols": 120, "rows": 32, "shell": shell, "args": shell_args})).await;
            match opened {
                Ok(info) => info.get("term_id").and_then(Value::as_str).unwrap_or_default().to_owned(),
                Err(error) => return unavailable(error),
            }
        }
        "send" => {
            let keys = args.get("keys").and_then(Value::as_str).unwrap_or("");
            if let Err(error) = host.call("terminal.write", json!({"term_id": id, "data": crate::term::keys_to_bytes(keys)})).await {
                return unavailable(error);
            }
            id
        }
        "read" => id,
        "close" => {
            return match host.call("terminal.close", json!({"term_id": id})).await {
                Ok(_) => Out::ok(format!("terminal {id} closed")),
                Err(error) => unavailable(error),
            };
        }
        other => return Out::fail("bad_argument", format!("unknown terminal action `{other}`")),
    };
    // `read` returns the screen as it is; open and send wait for the program to settle.
    let wait = crate::term::Wait {
        until,
        settle: Duration::from_millis(TERMINAL_SETTLE_MS),
        limit: Duration::from_millis(if action == "read" { 0 } else { wait_ms }),
    };
    let started = Instant::now();
    let mut watcher = crate::term::Watcher::new(wait, started);
    loop {
        let snapshot = match host.call("terminal.snapshot", json!({"term_id": term_id})).await {
            Ok(snapshot) => snapshot,
            Err(error) => return unavailable(error),
        };
        let text = snapshot.get("text").and_then(Value::as_str).unwrap_or("");
        let row = snapshot.get("cursor_row").and_then(Value::as_u64).unwrap_or(0) as u16;
        let col = snapshot.get("cursor_col").and_then(Value::as_u64).unwrap_or(0) as u16;
        let alive = snapshot.get("alive").and_then(Value::as_bool).unwrap_or(false);
        let screen = crate::term::render(text, row, col);
        if let Some(settled) = watcher.observe(&screen, alive, Instant::now()) {
            // Only a screen still changing when the wait ran out says so (a read never waits).
            let note = match settled {
                _ if !alive => " (the program exited)",
                crate::term::Settled::Limit if action != "read" => " (still changing; read again to see more)",
                _ => "",
            };
            let mut out = Out::ok(format!("terminal {term_id}{note}:\n{screen}"));
            out.display = json!({"terminal": term_id, "alive": alive, "screen": screen});
            return out;
        }
        tokio::select! {
            _ = cancel.cancelled() => return Out::fail("cancelled", format!("stopped waiting for terminal {term_id}")),
            _ = tokio::time::sleep(Duration::from_millis(TERMINAL_POLL_MS)) => {}
        }
    }
}

/// Rewrite `path`/`paths` that name the container's workdir (`/testbed/src/a.py`) to the same
/// files in the workspace. Paths that exist on the Mac, URIs and relative paths are left alone.
async fn map_container_paths(host: &Arc<dyn Host>, session: &Session<'_>, args: &mut Map<String, Value>) -> Option<String> {
    let mapped_one = |path: &str, workdir: &str| -> Option<String> {
        if !path.starts_with('/') || Path::new(path).exists() || uris::is_uri(path) {
            return None;
        }
        let workdir = workdir.trim_end_matches('/');
        let rest = path.strip_prefix(workdir)?;
        if !(rest.is_empty() || rest.starts_with('/')) {
            return None;
        }
        Some(session.workspace.join(rest.trim_start_matches('/')).display().to_string())
    };
    let names_container_path = |value: &Value| value.as_str().is_some_and(|p| p.starts_with('/') && !Path::new(p).exists());
    let candidates = args.get("path").is_some_and(names_container_path)
        || args.get("paths").and_then(Value::as_array).is_some_and(|paths| paths.iter().any(names_container_path));
    if !candidates {
        return None;
    }
    let container = project_container(host, session).await?;
    let mut changed = Vec::new();
    if let Some(Value::String(path)) = args.get("path").cloned()
        && let Some(mapped) = mapped_one(&path, &container.workdir)
    {
        changed.push(format!("{path} -> {}", session.display(Path::new(&mapped))));
        args.insert("path".to_owned(), Value::String(mapped));
    }
    if let Some(Value::Array(paths)) = args.get("paths").cloned() {
        let paths: Vec<Value> = paths
            .into_iter()
            .map(|value| match value.as_str().and_then(|p| mapped_one(p, &container.workdir).map(|m| (p.to_owned(), m))) {
                Some((from, mapped)) => {
                    changed.push(format!("{from} -> {}", session.display(Path::new(&mapped))));
                    Value::String(mapped)
                }
                None => value,
            })
            .collect();
        args.insert("paths".to_owned(), Value::Array(paths));
    }
    (!changed.is_empty()).then(|| format!("container path {}", changed.join(", ")))
}

/// A project whose commands run in a container (the container plugin's `container.status`).
struct ProjectContainer {
    id: String,
    workdir: String,
}

/// The session's project container when one is running; None when the project runs on the Mac
/// (or no container plugin is installed).
async fn project_container(host: &Arc<dyn Host>, session: &Session<'_>) -> Option<ProjectContainer> {
    let info = host.call("container.status", json!({"root": session.workspace.display().to_string()})).await.ok()?;
    let field = |name: &str| info.get(name).and_then(Value::as_str).map(str::to_owned);
    (field("state").as_deref() == Some("running")).then(|| ProjectContainer {
        id: field("id").unwrap_or_default(),
        workdir: field("workdir").unwrap_or_else(|| "/workspace".to_owned()),
    })
}

/// The script a command runs as inside a container: the session's working directory and exported
/// variables persist between commands in `/tmp/.cortexcode-<session>.{cwd,env}` inside the
/// container (like the persistent shell on the Mac). `cwd` runs this one command elsewhere.
pub fn container_script(session_id: &str, workdir: &str, command: &str, cwd: Option<&str>, persist: bool) -> String {
    let state: String = session_id.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' }).collect();
    let state = format!("/tmp/.cortexcode-{state}");
    let workdir_q = util::shell_quote(workdir);
    let mut script = format!(
        "__cx_state={state}\n[ -f \"$__cx_state.env\" ] && . \"$__cx_state.env\" >/dev/null 2>&1\ncd \"$(cat \"$__cx_state.cwd\" 2>/dev/null || echo {workdir_q})\" 2>/dev/null || cd {workdir_q}\n"
    );
    if let Some(cwd) = cwd {
        script.push_str(&format!("cd {} || exit 1\n", util::shell_quote(cwd)));
    }
    script.push_str(&format!("{{ {command}\n}} </dev/null\n__cx_rc=$?\n"));
    if persist {
        script.push_str("export -p > \"$__cx_state.env\" 2>/dev/null\n");
        if cwd.is_none() {
            script.push_str("pwd > \"$__cx_state.cwd\" 2>/dev/null\n");
        }
    }
    script.push_str("exit $__cx_rc\n");
    script
}

/// A host path under the workspace as the container sees it (relative paths stay relative).
fn container_path(session: &Session<'_>, workdir: &str, path: &str) -> String {
    let path_buf = Path::new(path);
    match path_buf.strip_prefix(&session.workspace) {
        Ok(rel) if path_buf.is_absolute() => format!("{}/{}", workdir.trim_end_matches('/'), rel.display()).trim_end_matches('/').to_owned(),
        _ => path.to_owned(),
    }
}

async fn run_in_container(
    core: &Arc<Core>,
    host: &Arc<dyn Host>,
    session: &Session<'_>,
    container: &ProjectContainer,
    command: &str,
    args: &Map<String, Value>,
    cancel: CancellationToken,
) -> Out {
    let cwd = args.get("cwd").and_then(Value::as_str).map(|cwd| container_path(session, &container.workdir, cwd));
    let session_id = &session.context.session_id;
    if args.get("background").and_then(Value::as_bool).unwrap_or(false) {
        // A background job is a local job whose command is the docker exec (no write sandbox: the
        // container is the isolation, and the Docker CLI must reach its socket).
        let script = container_script(session_id, &container.workdir, command, cwd.as_deref(), false);
        // The container plugin's docker (an absolute path: Docker Desktop is not always on PATH).
        let docker_bin = match host.call("container.shell_command", json!({"root": session.workspace.display().to_string()})).await {
            Ok(value) => value.get("argv").and_then(|argv| argv.get(0)).and_then(Value::as_str).unwrap_or("docker").to_owned(),
            Err(error) => return Out::fail("container_error", format!("the project's container gives no shell: {}", error.message)),
        };
        let docker = format!("{} exec -i {} bash -lc {}", util::shell_quote(&docker_bin), util::shell_quote(&container.id), util::shell_quote(&script));
        let wake = args.get("wake").and_then(|wake| serde_json::from_value::<Wake>(wake.clone()).ok()).unwrap_or_default();
        return match core.jobs.start(StartRequest {
            session_id,
            project_id: &session.context.project_id,
            command: &docker,
            cwd: &session.workspace,
            env_file: None,
            identity_env: Vec::new(),
            wake: wake.clone(),
            sandbox: None,
            venv: None,
            toolchain: crate::langs::toolchain(host.as_ref()).await,
        }) {
            Ok(meta) => {
                let mut out = Out::ok(started_line(&meta.job_id, &meta.artifact, &wake));
                out.display = json!({ "job_id": meta.job_id, "pid": meta.pid, "artifact": meta.artifact, "background": true, "container": container.id });
                out
            }
            Err(error) => Out::fail("job_error", format!("could not start the job: {error}")),
        };
    }
    let timeout_ms = args.get("timeout_ms").and_then(Value::as_u64).unwrap_or(core.config.default_timeout_ms);
    let script = container_script(session_id, &container.workdir, command, cwd.as_deref(), true);
    let call = host.call("container.exec", json!({"root": session.workspace.display().to_string(), "command": script, "timeout_ms": timeout_ms}));
    let result = tokio::select! {
        result = call => result,
        _ = cancel.cancelled() => return Out::fail("cancelled", "[cancelled; the command in the container was stopped]\n".to_owned()),
    };
    let result = match result {
        Ok(result) => result,
        Err(error) => return Out::fail("container_error", format!("could not run the command in the project's container: {}", error.message)),
    };
    let output = result.get("output").and_then(Value::as_str).unwrap_or("").as_bytes().to_vec();
    let code_hits = verifier_hits(host, command, &output[..]).await;
    let exit_code = result.get("exit_code").and_then(Value::as_i64).map(|code| code as i32);
    let duration_ms = result.get("duration_ms").and_then(Value::as_u64).unwrap_or(0);
    let timed_out = result.get("timed_out").and_then(Value::as_bool).unwrap_or(false);
    let (mut text, artifact, bytes) = present_output(core, Captured::Mem(output), core.config.run_limits);
    if !text.is_empty() && !text.ends_with('\n') {
        text.push('\n');
    }
    let (ok, kind) = match (timed_out, exit_code) {
        (true, _) => {
            let _ = writeln!(text, "{}", timeout_line(timeout_ms));
            (false, Some("timeout"))
        }
        (false, Some(code)) => {
            let _ = writeln!(text, "{}", exit_line(code, duration_ms));
            (code == 0, (code != 0).then_some("nonzero_exit"))
        }
        (false, None) => {
            let _ = writeln!(text, "[finished without an exit status]");
            (false, Some("nonzero_exit"))
        }
    };
    let mut out = Out { ok, output: text, artifact: artifact.clone(), error_kind: kind.map(str::to_owned), code_hits, ..Out::default() };
    out.display = json!({ "exit_code": exit_code, "duration_ms": duration_ms, "timed_out": timed_out, "artifact": artifact, "bytes": bytes, "container": container.id });
    out
}

async fn exec_run(core: &Arc<Core>, host: &Arc<dyn Host>, session: &Session<'_>, args: &Map<String, Value>, items: &ItemSink, cancel: CancellationToken) -> Out {
    let Some(command) = args.get("command").and_then(Value::as_str) else {
        return Out::fail("call_missing_argument", "run needs `command`".to_owned());
    };
    // A project that runs in a container runs its commands there.
    if let Some(container) = project_container(host, session).await {
        return run_in_container(core, host, session, &container, command, args, cancel).await;
    }
    let shell_cwd = core.shells.saved_cwd(&session.context.session_id).unwrap_or_else(|| session.workspace.clone());
    let cwd = args.get("cwd").and_then(Value::as_str).map(|cwd| util::resolve(&shell_cwd, cwd));
    if let Some(cwd) = &cwd
        && !cwd.is_dir()
    {
        return Out::fail("not_found", format!("cwd {} is not a directory", cwd.display()));
    }
    let effective_cwd = cwd.clone().unwrap_or(shell_cwd);
    let runtime = match core.runtime(&session.context.session_id, &session.workspace, session.python_env).await {
        Ok(runtime) => runtime,
        Err(message) => return Out::fail("sandbox_unavailable", message),
    };
    let venv_path = runtime.venv.as_ref().map(|venv| venv.path().to_path_buf());
    let writable = |path: &Path| core.sandbox.writable(path, &session.workspace, venv_path.as_deref(), &runtime.grants);
    // Writes outside the sandbox: asked for by the model (`write_outside`) or, in ask mode, implied
    // by a deletion or overwrite outside the workspace that the user is asked about anyway.
    let requested = requested_writes(args, &effective_cwd);
    if requested.iter().any(|path| core.sandbox.protected(path)) {
        return Out::fail("protected_path", "Not run: it would change CortexCode's own state.\n".to_owned());
    }
    let mut write_classes: Vec<String> = requested.iter().filter(|path| !writable(path)).map(|path| sandbox::write_class(path)).collect();
    if session.context.mode == "ask" {
        let approved = core.approved(&session.context.session_id);
        let all = approval::assess(command, &approval::Context { cwd: &effective_cwd, workspace: &session.workspace });
        for concern in &all {
            if let Some(path) = &concern.path
                && !writable(path)
            {
                write_classes.push(sandbox::write_class(path));
            }
        }
        let concerns: Vec<approval::Concern> = all.into_iter().filter(|concern| !approved.contains(concern.class)).collect();
        write_classes.sort();
        write_classes.dedup();
        if !concerns.is_empty() || !write_classes.is_empty() {
            let mut classes: Vec<String> = concerns.iter().map(|concern| concern.class.to_owned()).collect();
            classes.dedup();
            classes.extend(write_classes.iter().cloned());
            let mut reasons: Vec<String> =
                concerns.iter().map(|concern| format!("{} ({})", describe_class(concern.class), concern.detail)).collect();
            reasons.extend(write_classes.iter().map(|class| describe_class(class)));
            let mut output = String::from("Not run: this command needs the user's approval because it ");
            output.push_str(&reasons.join("; "));
            output.push_str(". The user has been asked; continue with other work or wait for their answer.\n");
            let mut out = Out::fail("needs_approval", output);
            out.needs_approval = true;
            out.display = json!({ "approval": { "classes": classes, "concerns": concerns, "command": command } });
            return out;
        }
    } else if !write_classes.is_empty() {
        write_classes.sort();
        write_classes.dedup();
        let paths: Vec<&str> = write_classes.iter().filter_map(|class| class.strip_prefix(sandbox::WRITE_CLASS_PREFIX)).collect();
        let output = format!(
            "Not run: it writes to {}, outside the folders commands may write to ({WRITABLE}). Do it inside the workspace; if that is impossible, say in your reply what it needs.\n",
            paths.join(", ")
        );
        let mut out = Out::fail("approval_required", output);
        out.display = json!({ "approval": { "classes": write_classes, "command": command, "mode": "auto" } });
        return out;
    }
    let identity = identity(core, session, Some(&runtime));
    // The language plugins' installed toolchains (Go, a JDK, shellcheck ...) on the command's PATH.
    let toolchain = crate::langs::toolchain(host.as_ref()).await;
    let background = args.get("background").and_then(Value::as_bool).unwrap_or(false);
    if background {
        let wake = args.get("wake").and_then(|wake| serde_json::from_value::<Wake>(wake.clone()).ok()).unwrap_or_default();
        let env_file = core.shells.saved_env(&session.context.session_id);
        let identity_env = vec![
            ("CORTEX_SESSION_ID".to_owned(), identity.session_id.clone()),
            ("CORTEX_PROJECT_ID".to_owned(), identity.project_id.clone()),
            ("CORTEX_HOME".to_owned(), core.home.to_string_lossy().into_owned()),
            ("CORTEX_WORKSPACE".to_owned(), session.workspace.to_string_lossy().into_owned()),
        ];
        return match core.jobs.start(StartRequest {
            session_id: &identity.session_id,
            project_id: &identity.project_id,
            command,
            cwd: &effective_cwd,
            env_file: env_file.as_deref(),
            identity_env,
            wake: wake.clone(),
            sandbox: Some(&runtime.sandbox),
            venv: venv_path.as_deref(),
            toolchain: toolchain.clone(),
        }) {
            Ok(meta) => {
                let mut out = Out::ok(started_line(&meta.job_id, &meta.artifact, &wake));
                out.display = json!({ "job_id": meta.job_id, "pid": meta.pid, "artifact": meta.artifact, "background": true });
                out
            }
            Err(error) => Out::fail("job_error", format!("could not start the job: {error}")),
        };
    }
    let timeout_ms = args.get("timeout_ms").and_then(Value::as_u64).unwrap_or(core.config.default_timeout_ms);
    let timeout = Duration::from_millis(timeout_ms);
    let sink = items.clone();
    let result = core
        .shells
        .run(&identity, RunRequest { command, cwd, env: Vec::new(), toolchain, timeout }, cancel, move |chunk: &[u8]| {
            if !chunk.is_empty() {
                sink.send(json!({ "kind": "output", "text": String::from_utf8_lossy(chunk) }));
            }
        })
        .await;
    let run = match result {
        Ok(run) => run,
        Err(error) => return Out::fail("shell_error", format!("could not run the command: {error}")),
    };
    // A refused write: the OS error names a path the sandbox does not let commands write.
    let eperm = refused_paths(&run.output).iter().any(|path| !writable(path));
    let code_hits = match &run.output {
        Captured::Mem(bytes) => verifier_hits(host, command, &bytes[..]).await,
        Captured::File { path, .. } => match std::fs::File::open(path) {
            Ok(file) => verifier_hits(host, command, std::io::BufReader::new(file)).await,
            Err(_) => Vec::new(),
        },
    };
    let (mut text, artifact, bytes) = present_output(core, run.output, core.config.run_limits);
    if !text.is_empty() && !text.ends_with('\n') {
        text.push('\n');
    }
    let seconds = run.duration_ms as f64 / 1000.0;
    // Notes on how it ran come before its exit line, which stays last.
    if eperm {
        let _ = writeln!(text, "[\"Operation not permitted\": commands may write only in the workspace, temp dirs, package caches and your venv.]");
    }
    if let Some(error) = &runtime.venv_error
        && core.read_state.first_note(&session.context.session_id, "no-venv")
    {
        let _ = writeln!(
            text,
            "[no Python venv for this workspace ({error}); python and pip are the system ones, and installs into them are refused]"
        );
    }
    let (ok, kind) = if run.timed_out {
        let _ = writeln!(text, "{}", timeout_line(timeout_ms));
        (false, Some("timeout"))
    } else if run.cancelled {
        let _ = writeln!(text, "[cancelled after {seconds:.1}s; the command and its processes were killed]");
        (false, Some("cancelled"))
    } else if run.shell_exited {
        let _ = writeln!(text, "[the shell exited; the next command starts in a new shell]");
        (run.exit_code == Some(0), (run.exit_code != Some(0)).then_some("nonzero_exit"))
    } else {
        match run.exit_code {
            Some(code) => {
                let _ = writeln!(text, "{}", exit_line(code, run.duration_ms));
                (code == 0, (code != 0).then_some("nonzero_exit"))
            }
            None => {
                let _ = writeln!(text, "[finished without an exit status]");
                (false, Some("nonzero_exit"))
            }
        }
    };
    let mut out = Out { ok, output: text, artifact: artifact.clone(), error_kind: kind.map(str::to_owned), code_hits, ..Out::default() };
    out.display = json!({
        "exit_code": run.exit_code,
        "duration_ms": run.duration_ms,
        "timed_out": run.timed_out,
        "cancelled": run.cancelled,
        "artifact": artifact,
        "bytes": bytes,
        "sandbox": {
            "venv": venv_path,
            "venv_kind": runtime.venv.as_ref().map(|venv| if matches!(venv, PyEnv::Project(_)) { "project" } else { "auto" }),
            "eperm": eperm,
        },
    });
    out
}

/// The declared diagnostic codes in a command's output when the command runs one of the language
/// plugins' verifiers (none otherwise: other commands' output is not scanned).
async fn verifier_hits(host: &Arc<dyn Host>, command: &str, output: impl std::io::BufRead) -> Vec<crate::codeskills::CodeHit> {
    let plugins = crate::langs::plugins(host.as_ref()).await;
    let owners = crate::codeskills::verifier_owners(&plugins, command);
    if owners.is_empty() {
        return Vec::new();
    }
    let index = crate::codeskills::index(host.as_ref()).await;
    crate::codeskills::output_hits(&index, &owners, output)
}

fn describe_class(class: &str) -> String {
    if let Some(path) = class.strip_prefix(sandbox::WRITE_CLASS_PREFIX) {
        return format!("writes to {path}, outside the write sandbox");
    }
    match class {
        "git_push" => "pushes to a git remote",
        "publish" => "publishes or changes something on a remote service",
        "network" => "talks to another machine",
        "delete_outside" => "deletes files outside the workspace",
        "overwrite_outside" => "overwrites a file outside the workspace",
        _ => "runs something only known at run time",
    }
    .to_owned()
}

/// Edit format for a context (default hashline).
pub fn edit_format(context: &ExecContext) -> EditFormat {
    EditFormat::parse(context.edit_format.as_deref())
}
