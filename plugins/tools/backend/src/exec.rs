//! `tools.execute`: repair the model's raw call, then run read / edit / run for one session.

use crate::approval;
use crate::artifacts::{self, InlineLimits};
use crate::core::{Core, Runtime};
use crate::doc::{Doc, text_to_lines};
use crate::hashedit::{self, EditOp, LineRef, OpKind};
use crate::host::Host;
use crate::jobs::{StartRequest, StopMode, Wake};
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
    // A read of `run:<command>` (no such file) means the command: it runs now, one step instead
    // of a refusal and a second call (seen: `read run:find . -name '*Test*.java'`).
    if canonical.tool == "read"
        && let Some(Value::String(path)) = canonical.args.get("path")
        && let Some(command) = path.strip_prefix("run:").map(str::trim).filter(|command| !command.is_empty())
        && !util::resolve(&session.workspace, path).exists()
    {
        let command = command.to_owned();
        canonical.args.remove("path");
        canonical.args.insert("command".to_owned(), Value::String(command));
        canonical.tool = "run";
        canonical.notes.push("a read of `run:<command>` ran as run".to_owned());
    }
    let intent = canonical.args.get("intent").and_then(Value::as_str).unwrap_or("").to_owned();
    // The step log: an edit records itself with the text in hand; a command's effects are found
    // when it ends (the watcher leaves them alone meanwhile).
    let steps = (canonical.tool != "read").then(|| core.steps.open(&session.workspace));
    if let Some(steps) = &steps {
        steps.begin();
    }
    let mut out = match canonical.tool {
        "read" => exec_read(core, host, &session, &canonical.args).await,
        "edit" => exec_edit(core, host, &session, &canonical.args).await,
        "terminal" => exec_terminal(host, &session, &canonical.args, &cancel).await,
        _ => exec_run(core, host, &session, &canonical.args, &items, cancel).await,
    };
    if let Some(steps) = steps {
        if canonical.tool == "edit" {
            steps.end_quiet();
        } else {
            let detail = match canonical.tool {
                "terminal" => format!("terminal {}", canonical.args.get("action").and_then(Value::as_str).unwrap_or("")),
                _ => canonical.args.get("command").and_then(Value::as_str).unwrap_or("").chars().take(200).collect(),
            };
            let who = crate::undo::Attribution::session(&session.context.session_id, "run", &detail);
            let _ = tokio::task::spawn_blocking(move || steps.end(&who)).await;
        }
    }
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
    next_step(core, &session, canonical.tool, &mut out);
    read_hint(&session, canonical.tool, &canonical.args, &mut out);
    same_refusal(core, &session, canonical.tool, &canonical.args, &mut out);
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

/// A call refused again the same way as the session's last refusal: said last, since the message
/// above already names what to change (seen: one refused edit sent three times unchanged). A run
/// that only exits non-zero is no refusal.
fn same_refusal(core: &Core, session: &Session<'_>, tool: &str, args: &serde_json::Map<String, Value>, out: &mut Out) {
    let Ok(mut refused) = core.refused.lock() else { return };
    let session_id = &session.context.session_id;
    let kind = out.error_kind.clone().filter(|kind| !out.ok && !matches!(kind.as_str(), "nonzero_exit" | "timeout" | "cancelled"));
    let Some(kind) = kind else {
        refused.remove(session_id);
        return;
    };
    let mut call = args.clone();
    call.remove("intent");
    let call = format!("{tool} {}", Value::Object(call));
    if refused.get(session_id).is_some_and(|(previous, previous_kind)| *previous == call && *previous_kind == kind) {
        let gap = if out.output.ends_with('\n') { "" } else { "\n" };
        let _ = writeln!(out.output, "{gap}You sent this same call before and it was refused the same way: change what the message above names before you send it again.");
    }
    refused.insert(session_id.clone(), (call, kind));
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
    // `file#Name` (or `file#Type::method`): one definition of the file as it is now, found by the
    // language's parser, read as its line range (anchors, tracked as a read). A big file is read
    // one definition at a time instead of whole.
    let by_name;
    let target = match target.split_once('#') {
        Some((file, name)) if !target.contains("://") && !file.is_empty() && !name.trim().is_empty() => {
            match definition_range(session, file, name.trim()) {
                Ok(lines) => {
                    by_name = format!("{file}:{lines}");
                    by_name.as_str()
                }
                Err(message) => return Out::fail("not_found", message),
            }
        }
        _ => target,
    };
    // `$TMPDIR/...` is the scratch folder `tmp/` (the same folder its shells see).
    let mut path_text = util::expand_scratch(target, &util::scratch_dir(&session.workspace));
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
    if core.sandbox.control_read_denied(&absolute) {
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

fn resolve_in_workspace(core: &Core, session: &Session<'_>, path: &str) -> Result<PathBuf, Box<Out>> {
    // The workspace, its scratch folder `tmp/` included (throwaway checks, also as `$TMPDIR/...`).
    let scratch = util::scratch_dir(&session.workspace);
    let absolute = util::resolve(&session.workspace, &util::expand_scratch(path, &scratch));
    if !util::is_inside(&absolute, &session.workspace) {
        return Err(Box::new(Out::fail(
            "outside_workspace",
            format!("{path} is outside the workspace; edit files inside it, a throwaway one in `tmp/<name>`."),
        )));
    }
    if util::is_inside(&absolute, &scratch) {
        util::ensure_scratch(&session.workspace);
    }
    if core.sandbox.protected(&absolute) || core.sandbox.git_control_write_denied(&session.workspace, &absolute) {
        return Err(Box::new(Out::fail("protected_path", "App control files and Git worktree pointers are managed by the app; edit project files instead.".to_owned())));
    }
    if absolute.is_dir() {
        return Err(Box::new(Out::fail("is_directory", format!("{path} is a directory"))));
    }
    Ok(util::canonical_lenient(&absolute))
}

/// Current text of a file: Ok(None) when it does not exist.
fn current_text(absolute: &Path, display: &str) -> Result<Option<String>, Box<Out>> {
    match crate::util::read_host_file(absolute) {
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
/// Syntax errors named where the edit is (seen: "line 1: cannot parse `// this module...`" and
/// "line 341: missing `}`" for a brace the edit left out near line 109): a region the parser could
/// not read that holds the edit, or a missing token past every changed line, is named at the edit's
/// last line. Errors at the edit, and named spots (a bracket that closes nothing), stay as they are.
/// `regions` are the edit's lines in the new file (1-based, first..=last); for an edit that only
/// takes lines out, `removed` is true and each region is the line the removed ones stood before.
fn at_the_edit(issues: Vec<lang::SyntaxIssue>, regions: &[(usize, usize)], removed: bool) -> Vec<lang::SyntaxIssue> {
    if regions.is_empty() {
        return issues;
    }
    let edit = |first: usize, last: usize| if removed { format!("the lines your edit took out, before line {first}") } else { format!("your edit (lines {first}-{last})") };
    let near = |line: usize| regions.iter().any(|&(first, last)| line + SYNTAX_CONTEXT >= first && line <= last + SYNTAX_CONTEXT);
    issues
        .into_iter()
        .map(|issue| {
            if near(issue.line) {
                return issue;
            }
            let moved = match issue.kind {
                "missing" => regions.iter().rev().find(|&&(_, last)| last < issue.line).map(|&(_, last)| {
                    (last, format!("{} (the parser only notices it at line {}): a block your edit opens is not closed, or the edit took out a closing line", issue.detail, issue.line))
                }),
                _ if issue.detail.starts_with("cannot parse") => regions.iter().find(|&&(first, last)| first >= issue.line && last <= issue.end_line).map(|&(first, last)| {
                    (last, format!("the code from line {} to {} cannot be read, and {} is inside it: check where it opens and closes blocks", issue.line, issue.end_line, edit(first, last)))
                }),
                _ => None,
            };
            match moved {
                Some((line, detail)) => lang::SyntaxIssue { line, column: 1, end_line: line, kind: issue.kind, detail },
                None => issue,
            }
        })
        .collect()
}

/// Where an edit is in the new file, 1-based first..=last: its changed regions, or, for an edit that
/// only takes lines out, the line each run of removed lines stood before (`true`).
fn edit_spots(changed: &[(usize, usize)], line_map: &[Option<usize>], new_len: usize) -> (Vec<(usize, usize)>, bool) {
    let regions: Vec<(usize, usize)> = changed.iter().filter(|(start, end)| end > start).map(|&(start, end)| (start + 1, end)).collect();
    if !regions.is_empty() {
        return (regions, false);
    }
    let mut spots: Vec<(usize, usize)> = Vec::new();
    let mut index = 0;
    while index < line_map.len() {
        if line_map[index].is_some() {
            index += 1;
            continue;
        }
        while index < line_map.len() && line_map[index].is_none() {
            index += 1;
        }
        // The first kept line after the run, in the new file; the last line when the run ends the file.
        let before = line_map[index..].iter().flatten().next().map_or(new_len.max(1), |&new| new + 1);
        if spots.last() != Some(&(before, before)) {
            spots.push((before, before));
        }
    }
    (spots, true)
}

/// For an edit that takes out more lone closing lines (`}`, `)`, `]`) than it writes: what each of
/// the last of them closes, by the line that opens it in the file, so the missing closing line is
/// looked for where it belongs (seen: a test file's last `}` deleted eight times while it closed a
/// function that had swallowed the tests after it, its own `}` missing higher up).
fn closing_notes(path: &Path, old: &str, line_map: &[Option<usize>], new: &str, changed: &[(usize, usize)]) -> Vec<String> {
    let Some(lang) = Lang::detect(path, old) else { return Vec::new() };
    let lone = |line: &str| {
        let text = line.trim();
        text.contains(['}', ')', ']']) && text.chars().all(|c| matches!(c, '}' | ')' | ']' | ';' | ','))
    };
    let old_lines: Vec<&str> = old.split('\n').collect();
    let new_lines: Vec<&str> = new.split('\n').collect();
    let removed: Vec<usize> = line_map.iter().enumerate().filter(|(index, new)| new.is_none() && old_lines.get(*index).is_some_and(|line| lone(line))).map(|(index, _)| index + 1).collect();
    let written = changed.iter().flat_map(|&(start, end)| start..end).filter(|&index| new_lines.get(index).is_some_and(|line| lone(line))).count();
    if removed.len() <= written {
        return Vec::new();
    }
    let extra = (removed.len() - written).min(2);
    removed[removed.len() - extra..]
        .iter()
        .filter_map(|&line| {
            let open = lang::opening_line(lang, old, line)?;
            if open + 1 >= line {
                return None;
            }
            let header: String = old_lines[open - 1].trim().chars().take(80).collect();
            let closer = old_lines[line - 1].trim();
            Some(format!(
                "Line {line}'s `{closer}` closes `{header}` from line {open}, so lines {}-{} are inside it. If code there should not be inside it, the closing line that is missing belongs higher up, where that code starts.",
                open + 1,
                line - 1
            ))
        })
        .collect()
}

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

/// A run that printed a project file with `cat`/`head`/`tail`/`sed -n` among other commands (a
/// plain one is answered as a read already) ends with where file text comes from: it runs, with a
/// note, never refused (seen: `ls -la && cat bowling.js`, then edits without anchors to copy).
fn read_hint(session: &Session<'_>, tool: &str, args: &Map<String, Value>, out: &mut Out) {
    if tool != "run" {
        return;
    }
    let Some(command) = args.get("command").and_then(Value::as_str) else { return };
    if approval::terminal_read(command).is_some() {
        return;
    }
    let scratch = util::scratch_dir(&session.workspace);
    let printed = approval::printed_files(command).into_iter().any(|file| {
        let absolute = util::resolve(&session.workspace, &file);
        absolute.is_file() && util::is_inside(&absolute, &session.workspace) && !util::is_inside(&absolute, &scratch)
    });
    if printed {
        out.output.push_str("Read project files with `read`: its lines carry the anchors `edit` needs.\n");
    }
}

/// An edit result ends with the step to take next: run the check (seen: seven edits in a row from
/// re-reading the file shown after each one, never run). After two edits of project files without a
/// run it says so. A run starts the count again.
fn next_step(core: &Core, session: &Session<'_>, tool: &str, out: &mut Out) {
    let session_id = &session.context.session_id;
    let Ok(mut counts) = core.edits_since_run.lock() else { return };
    if tool == "run" {
        counts.remove(session_id);
        return;
    }
    if tool != "edit" || !out.ok || out.changed_files.is_empty() || !out.output.trim_end().ends_with("No errors.") {
        return;
    }
    let scratch = util::scratch_dir(&session.workspace);
    let in_scratch = out.changed_files.iter().all(|file| util::is_inside(&util::resolve(&session.workspace, file), &scratch));
    if in_scratch {
        out.output.push_str("Next: run it.\n");
        return;
    }
    let count = counts.entry(session_id.clone()).or_insert(0);
    *count += 1;
    if *count >= 2 {
        let _ = writeln!(out.output, "You changed the code {count} times without running it: run the check now.");
    } else {
        out.output.push_str("Next: run the check.\n");
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
    // Errors are what must be fixed: they are shown first when the list is cut, and last in the
    // text, right before the step to take (seen: a list ending in style warnings, then a whole
    // rewrite of a 400-line file instead of four one-line fixes).
    let mut shown: Vec<&(usize, String, bool)> = items.iter().filter(|(_, _, error)| *error).chain(items.iter().filter(|(_, _, error)| !*error)).take(crate::langs::EDIT_DIAGNOSTICS_SHOWN).collect();
    shown.sort_by_key(|(line, _, error)| (*error, *line));
    for (line, message, _) in shown {
        let _ = writeln!(text, "line {line}: {message}");
        if (1..=check.doc.len()).contains(line) {
            text.push_str(&check.doc.render(*line, *line));
        }
    }
    if items.len() > crate::langs::EDIT_DIAGNOSTICS_SHOWN {
        let _ = writeln!(text, "... {} more (read diagnostics://{})", items.len() - crate::langs::EDIT_DIAGNOSTICS_SHOWN, check.display);
    }
    if errors > 0 {
        let what = if errors == 1 { "the error".to_owned() } else { format!("the {errors} errors") };
        let _ = writeln!(text, "Next: fix {what} above, each at its line{}.", if errors < items.len() { "; the warnings can wait" } else { "" });
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
    let Some(items) = crate::lang::Lang::detect(path, text).and_then(|lang| crate::lang::outline(lang, text)) else {
        return (changed.to_vec(), false);
    };
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

/// Lines shown before and after each syntax error in a refused edit.
const SYNTAX_CONTEXT: usize = 3;

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

/// The line range (`a-b`, 1-based) of the one definition of `file` that `name` names: its last
/// part (`take` in `Cursor::take`) is a word of the definition's header, and every earlier part is
/// a word of an enclosing definition's header. A refusal lists the candidates when the name is
/// unknown or names several.
fn definition_range(session: &Session<'_>, file: &str, name: &str) -> Result<String, String> {
    let absolute = util::resolve(&session.workspace, &util::expand_scratch(file, &util::scratch_dir(&session.workspace)));
    let text = crate::util::read_host_text(&absolute).map_err(|_| format!("{file}: no such file."))?;
    let lines: Vec<&str> = text.lines().collect();
    let items = crate::lang::Lang::detect(&absolute, &text).and_then(|lang| crate::lang::outline(lang, &text)).ok_or_else(|| format!("{file}: its definitions cannot be listed; read a line range instead, e.g. {file}:1-100."))?;
    let parts: Vec<&str> = name.split([':', '.']).filter(|part| !part.is_empty()).collect();
    let header = |item: &crate::lang::Item| -> String { lines.get(item.start..=item.header_end.min(lines.len().saturating_sub(1))).map(|rows| rows.join(" ")).unwrap_or_default() };
    let has_word = |text: &str, word: &str| {
        text.match_indices(word).any(|(at, _)| {
            let before = text[..at].chars().next_back();
            let after = text[at + word.len()..].chars().next();
            !before.is_some_and(|c| c.is_alphanumeric() || c == '_') && !after.is_some_and(|c| c.is_alphanumeric() || c == '_')
        })
    };
    fn walk<'a>(items: &'a [crate::lang::Item], parents: &mut Vec<&'a crate::lang::Item>, out: &mut Vec<(Vec<&'a crate::lang::Item>, &'a crate::lang::Item)>) {
        for item in items {
            out.push((parents.clone(), item));
            parents.push(item);
            walk(&item.children, parents, out);
            parents.pop();
        }
    }
    let mut all = Vec::new();
    walk(&items, &mut Vec::new(), &mut all);
    let (last, earlier) = parts.split_last().ok_or_else(|| format!("name a definition after `#`, e.g. {file}#Name"))?;
    let found: Vec<&crate::lang::Item> = all
        .iter()
        .filter(|(parents, item)| has_word(&header(item), last) && earlier.iter().all(|part| parents.iter().any(|parent| has_word(&header(parent), part))))
        .map(|(_, item)| *item)
        .collect();
    let shown = |item: &crate::lang::Item| format!("line {} `{}`", item.start + 1, lines.get(item.start).map(|line| line.trim().chars().take(60).collect::<String>()).unwrap_or_default());
    match found.as_slice() {
        [item] => Ok(format!("{}-{}", item.start + 1, item.end + 1)),
        [] => {
            let listed: Vec<String> = all.iter().take(30).map(|(_, item)| shown(item)).collect();
            Err(format!("{file} has no definition named `{name}`. Its definitions:\n{}", listed.join("\n")))
        }
        many => {
            let listed: Vec<String> = many.iter().take(10).map(|item| shown(item)).collect();
            Err(format!("`{name}` names {} definitions in {file}; name the one you mean with its type, e.g. {file}#Type::{last}:\n{}", many.len(), listed.join("\n")))
        }
    }
}

/// Files of at most this many lines are shown whole after an edit: the same size a read shows whole.
const WHOLE_AFTER_EDIT: usize = 300;

/// Top-level definitions one edit may replace in an existing file.
const MAX_DEFINITIONS_PER_EDIT: usize = 2;

/// An edit that writes at most this many lines is small enough to lose: the definitions limit does
/// not apply (seen: four small type definitions changed together refused six times in one task).
const SMALL_EDIT_LINES: usize = 60;

/// Two replaces of one call where one's range holds the other's and both end on the same line: the
/// outer one is taken to stop where the inner one starts, said in the result (seen: lines 63-95 and
/// 90-95 sent together twice, refused as overlapping each time). Only anchors that name their line
/// in the file as it is now are moved.
fn shorten_nested(doc: &Doc, ops: &mut [hashedit::EditOp]) -> Vec<String> {
    let direct = |reference: &hashedit::LineRef| match reference {
        hashedit::LineRef::Tagged(anchor) if anchor.line >= 1 && anchor.line <= doc.len() && doc.tag(anchor.line - 1).as_bytes() == anchor.tag => Some(anchor.line),
        _ => None,
    };
    let mut said = Vec::new();
    for outer in 0..ops.len() {
        for inner in 0..ops.len() {
            if outer == inner || ops[outer].kind != hashedit::OpKind::Replace || ops[inner].kind != hashedit::OpKind::Replace {
                continue;
            }
            let (Some(outer_at), Some(inner_at)) = (direct(&ops[outer].at), direct(&ops[inner].at)) else { continue };
            let outer_to = ops[outer].to.as_ref().and_then(direct).unwrap_or(outer_at);
            let inner_to = ops[inner].to.as_ref().and_then(direct).unwrap_or(inner_at);
            if outer_at < inner_at && outer_to == inner_to {
                let end = inner_at - 1;
                let tag = doc.tag(end - 1);
                let bytes = tag.as_bytes();
                if bytes.len() != 2 {
                    continue;
                }
                ops[outer].to = Some(hashedit::LineRef::Tagged(cc_protocol::hashline::Anchor { line: end, tag: [bytes[0], bytes[1]] }));
                said.push(format!("Edit #{} now ends at line {end}, where edit #{} starts: their ranges overlapped.", outer + 1, inner + 1));
            }
        }
    }
    said
}

/// The lines an edit writes in all.
fn written_lines(ops: &[hashedit::EditOp]) -> usize {
    ops.iter().map(|op| op.lines.len()).sum()
}

/// The corrections of a refused edit whose range ends in the wrong place (seen: seven refusals of
/// this kind in one run, each costing a step and often fixed the wrong way). Tried in order; the
/// first that passes every check (the definitions limit, the syntax check) is written, with a note
/// saying what moved; None otherwise, and the refusal stands.
/// 1. The text already ends with the lines right after the range (a closing line, or several
///    lines): the range grows over them. A replace without `to` is a range of one line.
/// 2. The range's last line is a lone closing line (`}`, `end`...) the text does not end with: that
///    line stays.
/// 3. The text ends with a lone closing line and the line right after the range starts with the
///    same one (`} else {`): the text's last line goes.
#[allow(clippy::too_many_arguments)]
fn retry_with_range_fix(
    core: &Core,
    host: &Arc<dyn Host>,
    session: &Session<'_>,
    doc: &Doc,
    view: Option<&hashedit::View>,
    ops: &[hashedit::EditOp],
    in_scratch: bool,
    (absolute, display, old, notes): (PathBuf, String, Option<String>, Vec<String>),
) -> Option<Out> {
    let closing = |text: &str| !text.is_empty() && (text.chars().all(|c| matches!(c, '}' | ')' | ']' | ';' | ',')) || text == "end");
    let line_of = |reference: &hashedit::LineRef| match reference {
        hashedit::LineRef::Tagged(anchor) => Some(anchor.line),
        hashedit::LineRef::Bare(line) => Some(*line),
        hashedit::LineRef::Start => None,
    };
    // The file's line `line` (1-based), and its anchor.
    let file = |line: usize| if line >= 1 { doc.lines.get(line - 1).map(String::as_str) } else { None };
    let anchor = |line: usize| -> Option<hashedit::LineRef> {
        file(line)?;
        let tag = doc.tag(line - 1);
        let bytes = tag.as_bytes();
        (bytes.len() == 2).then(|| hashedit::LineRef::Tagged(cc_protocol::hashline::Anchor { line, tag: [bytes[0], bytes[1]] }))
    };
    // Two lines are the same line when they differ only in trailing spaces, or are the same lone
    // closing line at another indent.
    let same = |a: &str, b: &str| a.trim_end() == b.trim_end() || (closing(a.trim()) && a.trim() == b.trim());
    for fix in 1..=3 {
        let mut fixed: Vec<hashedit::EditOp> = ops.to_vec();
        let mut said: Vec<String> = Vec::new();
        for (index, op) in fixed.iter_mut().enumerate() {
            if op.kind != hashedit::OpKind::Replace {
                continue;
            }
            let Some(start) = line_of(&op.at) else { continue };
            let end = op.to.as_ref().and_then(line_of).unwrap_or(start);
            let mut text: Vec<String> = op.lines.clone();
            while text.last().is_some_and(|line| line.trim().is_empty()) {
                text.pop();
            }
            let Some(last) = text.last().map(|line| line.trim().to_owned()) else { continue };
            match fix {
                1 => {
                    let Some(k) = (1..text.len()).rev().find(|&k| {
                        (1..=k).all(|i| file(end + i).is_some_and(|line| same(&text[text.len() - k + i - 1], line)))
                    }) else {
                        continue;
                    };
                    let Some(to) = anchor(end + k) else { continue };
                    op.to = Some(to);
                    let lines = if k == 1 { "line".to_owned() } else { format!("{k} lines") };
                    said.push(format!("edit #{}: `to` is now line {}, because your text already ended with the {lines} after the range", index + 1, end + k));
                }
                2 => {
                    let Some(closer) = file(end) else { continue };
                    if end <= start || !closing(closer.trim()) || text.last().is_some_and(|line| line.trim_end() == closer.trim_end()) {
                        continue;
                    }
                    let Some(to) = anchor(end - 1) else { continue };
                    op.to = Some(to);
                    said.push(format!(
                        "edit #{}: line {end} (`{}`) stays, because it closes the code around your range and your text does not end with it",
                        index + 1,
                        closer.trim()
                    ));
                }
                _ => {
                    let Some(after) = file(end + 1).map(str::trim) else { continue };
                    if !closing(&last) || after.len() <= last.len() || !after.starts_with(last.as_str()) {
                        continue;
                    }
                    let kept = text.len() - 1;
                    op.lines.truncate(kept);
                    said.push(format!(
                        "edit #{}: its last line `{last}` is left out, because line {} right after the range (`{after}`) already closes that block",
                        index + 1,
                        end + 1
                    ));
                }
            }
        }
        if said.is_empty() {
            continue;
        }
        let Ok(applied) = hashedit::apply(doc, view, &fixed) else { continue };
        let written: Vec<(usize, usize)> = fixed
            .iter()
            .filter(|op| op.lines.len() > 2)
            .filter_map(|op| {
                let at = line_of(&op.at)?;
                Some((at, op.to.as_ref().and_then(line_of).unwrap_or(at)))
            })
            .collect();
        let text = old.clone().unwrap_or_default();
        if !in_scratch
            && written_lines(&fixed) > SMALL_EDIT_LINES
            && let Some(replaced) = replaced_definitions(&absolute, &text, &applied.line_map, &written)
            && replaced.len() > MAX_DEFINITIONS_PER_EDIT
        {
            continue;
        }
        let new = applied.doc.to_text();
        let commit_this = Commit {
            absolute: absolute.clone(),
            display: display.clone(),
            old: old.clone(),
            new,
            line_map: Some(applied.line_map),
            changed: applied.changed,
            notes: notes.clone(),
        };
        let mut out = commit(core, host, session, commit_this);
        if !out.ok {
            continue;
        }
        out.output.insert_str(0, &format!("Applied with the range corrected: {}.\n", said.join("; ")));
        return Some(out);
    }
    None
}

/// Lines of a definition's body that hold code: after its header, neither blank nor only closing
/// brackets. A stub (`pass`, `todo!()`, one `throw`) or a one-line function has at most one.
fn body_lines(lines: &[&str], item: &crate::lang::Item) -> usize {
    let end = item.end.min(lines.len().saturating_sub(1));
    (item.header_end + 1..=end)
        .filter_map(|index| lines.get(index))
        .map(|line| line.trim())
        .filter(|line| !line.is_empty() && !line.chars().all(|c| matches!(c, '}' | ')' | ']' | ';' | ',')))
        .count()
}

/// A file whose definitions are all stubs or one-liners (a skeleton to fill): written whole, it
/// loses no working code.
fn is_skeleton(path: &Path, text: &str) -> bool {
    let Some(items) = crate::lang::Lang::detect(path, text).and_then(|lang| crate::lang::outline(lang, text)) else {
        return false;
    };
    fn all_stubs(items: &[crate::lang::Item], lines: &[&str]) -> bool {
        items.iter().all(|item| if item.children.is_empty() { body_lines(lines, item) <= 1 } else { all_stubs(&item.children, lines) })
    }
    let lines: Vec<&str> = text.lines().collect();
    !items.is_empty() && all_stubs(&items, &lines)
}

/// The top-level definitions of `old` that an edit replaces: a function with whatever it nests, or
/// a class (impl, struct) with its methods, each counted once; a namespace or module is looked
/// through. One counts when a function in it lost at least half of its lines (`line_map` maps each
/// old line to its new one, None when replaced or deleted). A small fix inside a function does not
/// count, nor does a stub or one-line function (filling a skeleton loses no working code), nor one
/// that was only deleted (`written`: the line ranges that got new text). Their header lines,
/// shortened, as names. None when the language has no outline.
fn replaced_definitions(path: &Path, old: &str, line_map: &[Option<usize>], written: &[(usize, usize)]) -> Option<Vec<String>> {
    let items = crate::lang::Lang::detect(path, old).and_then(|lang| crate::lang::outline(lang, old))?;
    let lines: Vec<&str> = old.lines().collect();
    let replaced = |item: &crate::lang::Item| {
        let span = item.start..=item.end.min(line_map.len().saturating_sub(1));
        let total = span.clone().count();
        let gone = span.filter(|&index| line_map.get(index).is_some_and(Option::is_none)).count();
        // `written` holds 1-based ranges that got new text; `item` rows are 0-based.
        let overwritten = written.iter().any(|&(at, to)| at <= item.end + 1 && to > item.start);
        total >= 2 && gone * 2 >= total && body_lines(&lines, item) > 1 && overwritten
    };
    fn any_replaced(item: &crate::lang::Item, replaced: &dyn Fn(&crate::lang::Item) -> bool) -> bool {
        if item.children.is_empty() { replaced(item) } else { item.children.iter().any(|child| any_replaced(child, replaced)) }
    }
    fn top_level<'a>(items: &'a [crate::lang::Item], out: &mut Vec<&'a crate::lang::Item>) {
        for item in items {
            let namespace = matches!(
                item.kind.as_str(),
                "namespace_definition" | "namespace_declaration" | "mod_item" | "foreign_mod_item" | "internal_module" | "module"
            );
            if namespace && !item.children.is_empty() { top_level(&item.children, out) } else { out.push(item) }
        }
    }
    let mut tops = Vec::new();
    top_level(&items, &mut tops);
    // Methods defined outside their class (`int Buffer<T>::read() {` in C++) are that class's, as
    // methods written inside it are: one definition per class (seen: a fix to three methods of one
    // class refused as three).
    let mut owners: Vec<String> = Vec::new();
    let mut names = Vec::new();
    for item in tops.into_iter().filter(|item| any_replaced(item, &replaced)) {
        let header = lines.get(item.start).map(|line| line.trim()).unwrap_or_default();
        match method_owner(header) {
            Some(owner) if owners.contains(&owner) => {}
            Some(owner) => {
                names.push(format!("{owner}::…"));
                owners.push(owner);
            }
            None => names.push(header.chars().take(50).collect()),
        }
    }
    Some(names)
}

/// The class a definition outside it belongs to: in `int Buffer<T>::read() const {`, `Buffer`
/// (the name before the last `::` of the defined name, without template arguments). None for a
/// plain function.
fn method_owner(header: &str) -> Option<String> {
    let before_call = &header[..header.find('(')?];
    let name = before_call.split_whitespace().last()?.trim_start_matches(['*', '&']);
    let qualifier = &name[..name.rfind("::")?];
    let owner = qualifier.rsplit("::").next().unwrap_or(qualifier);
    let owner = owner.split('<').next().unwrap_or(owner).trim();
    (!owner.is_empty()).then(|| owner.to_owned())
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
    if let (Some(_), Some(map)) = (&old, &line_map) {
        let (introduced, _) = syntax_check(&absolute, &old_text, &new);
        if let Some(issues) = introduced.filter(|issues| !issues.is_empty()) {
            let proposed = Doc::parse(&new);
            let (spots, removed) = edit_spots(&changed, map, proposed.len());
            let issues = at_the_edit(issues, &spots, removed);
            let (mut shown, _) = widen_to_definitions(&absolute, &new, &changed);
            // An edit that only took lines out: the place they were, in view.
            if removed {
                shown.extend(spots.iter().map(|&(line, _)| (line.saturating_sub(1), line)));
            }
            let mut text = format!("Nothing was written: this edit would add a syntax error to {display}.\n");
            for issue in issues.iter().take(5) {
                let _ = writeln!(text, "- line {}: {}", issue.line, issue.detail);
            }
            for note in closing_notes(&absolute, &old_text, map, &new, &changed) {
                let _ = writeln!(text, "{note}");
            }
            text.push_str("What the edit would make it (not in the file):\n");
            // The changed region, and every error line with the lines around it: an error just past
            // the changed region (a leftover brace) must be in view, or the same edit comes back.
            let mut lines: std::collections::BTreeSet<usize> = std::collections::BTreeSet::new();
            for &(start, end) in &shown {
                lines.extend(start.saturating_sub(1)..(end + 1).min(proposed.len()));
            }
            for issue in issues.iter().take(5) {
                let at = issue.line.saturating_sub(1);
                lines.extend(at.saturating_sub(SYNTAX_CONTEXT)..(at + SYNTAX_CONTEXT + 1).min(proposed.len()));
            }
            let mut previous: Option<usize> = None;
            for line in lines {
                if previous.is_some_and(|previous| line > previous + 1) {
                    text.push_str("...\n");
                }
                let number = (line + 1).to_string();
                let _ = writeln!(text, "{number}  {}", proposed.lines[line]);
                // A caret under each error's column: the spot, not only the line.
                for issue in issues.iter().take(5).filter(|issue| issue.line == line + 1) {
                    let lead: String = proposed.lines[line].chars().take(issue.column.saturating_sub(1)).map(|c| if c == '\t' { '\t' } else { ' ' }).collect();
                    let _ = writeln!(text, "{}  {lead}^ {}", " ".repeat(number.len()), issue.detail);
                }
                previous = Some(line);
            }
            text.push_str("Send the edit again with the error fixed; the anchors from your last read still hold.\n");
            return Out::fail("syntax_error", text);
        }
    }
    // The file as it was before the turn's first change of it (None: the turn creates it): the
    // baseline check runs the tests on these. A one-off try in `tmp/` is no project code.
    if !util::is_inside(&absolute, &util::scratch_dir(&session.workspace))
        && let Ok(mut originals) = core.originals.lock()
    {
        originals.entry(session.context.session_id.clone()).or_default().entry(absolute.clone()).or_insert_with(|| old.clone());
    }
    if let Err(error) = util::atomic_write_checked(&absolute, new.as_bytes(), |path| util::is_inside(path, &session.workspace) && !core.sandbox.protected(path) && !core.sandbox.git_control_write_denied(&session.workspace, path)) {
        return Out::fail("io_error", format!("{display}: {error}"));
    }
    core.tree.reserve(&absolute, &session.workspace, &session.context.session_id);
    {
        let mode = std::fs::metadata(&absolute).ok().map(|meta| { use std::os::unix::fs::PermissionsExt; meta.permissions().mode() & 0o777 }).unwrap_or(0o644);
        core.steps.open(&session.workspace).record_edit(
            &crate::undo::Attribution::session(&session.context.session_id, "edit", &display),
            &display,
            old.as_deref().map(str::as_bytes),
            Some(new.as_bytes()),
            mode,
        );
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
        // The prefix is named once here: after a whole-file echo the model doubted whether the
        // `N:hh|` prefixes were in the file and sent edits that repeated them.
        let anchors = if edit_format(session.context) == EditFormat::Hashline { ", with new anchors (the `N:hh|` prefix is not part of a line)" } else { "" };
        // A change inside a function shows that whole function as it now is: the model compares
        // its intent with the real function, never with fragments it pieced together.
        let (shown, whole) = widen_to_definitions(&absolute, &new, &changed);
        let what = if whole { "Each changed function, whole, as it is now" } else { "Changed lines and 2 around them" };
        // A read of this file earlier in the conversation now shows an older version (reads are
        // never rewritten afterwards: that keeps the conversation in the model's cache). Said once
        // per edit after a read, so the model does not doubt the file on disk (seen: caches
        // cleared and the module's source inspected to see "which version runs").
        if core.read_state.take_read_since_edit(&session.context.session_id, &absolute) {
            let _ = writeln!(output, "Your earlier reads of {display} above are older than this edit; the file on disk is as shown here.");
        }
        if doc.len() <= WHOLE_AFTER_EDIT {
            // A small file is shown whole after every edit: the model has the complete current
            // file and no reason to read it again (seen: re-reads after every edit when only the
            // changed functions were shown). Older views collapse to one line at the next settle.
            let _ = writeln!(output, "{verb} {display}. The whole file as it is now{anchors}:");
            output.push_str(&doc.render(1, doc.len()));
            let _ = writeln!(output, "[end of file]");
        } else {
            let _ = writeln!(output, "{verb} {display}. {what}{anchors} ({} lines; the rest is unchanged):", doc.len());
            output.push_str(&render_regions(&doc, &shown));
        }
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
    // A whole write over the user's file of stubs (the one existing file `content` may write) leaves
    // working code: the rule that now holds goes with the result, where the model looks back at the
    // file (seen: the whole write repeated twice on the filled file, refused each time).
    let own_draft = core.created.lock().is_ok_and(|created| created.get(&session.context.session_id).is_some_and(|files| files.contains(&absolute)));
    if line_map.is_none()
        && old.as_deref().is_some_and(|text| !text.trim().is_empty())
        && !own_draft
        && edit_format(session.context) != EditFormat::Whole
        && !util::is_inside(&absolute, &util::scratch_dir(&session.workspace))
    {
        let _ = writeln!(output, "{display} now holds working code: change it from here with anchored edits, one or two definitions per call.");
    }
    let (syntax_issues, syntax) = syntax_check(&absolute, &old_text, &new);
    // The model's work: a turn's cleanup keeps what `edit` wrote; a file it created this turn may
    // be written whole again.
    if let Ok(mut edited) = core.edited.lock() {
        edited.entry(session.context.session_id.clone()).or_default().insert(absolute.clone());
    }
    if old.is_none()
        && let Ok(mut created) = core.created.lock()
    {
        created.entry(session.context.session_id.clone()).or_default().insert(absolute.clone());
    }
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
        // A bare line number (see the repair of anchors): valid only while the file is as last read.
        Value::Number(number) => number.as_u64().map(|line| LineRef::Bare(line as usize)),
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
    let absolute = match resolve_in_workspace(core, session, path) {
        Ok(path) => path,
        Err(out) => return *out,
    };
    let display = session.display(&absolute);
    if let Some(out) = reserved_elsewhere(core, host, session, std::slice::from_ref(&absolute)).await {
        return out;
    }
    let lock = core.file_lock(&absolute);
    let _guard = lock.lock().await;
    let current = match current_text(&absolute, &display) {
        Ok(current) => current,
        Err(out) => return *out,
    };
    let session_id = &session.context.session_id;

    // Both `content` and `edits` sent (the two forms describe the same change): the whole content
    // where a whole write is allowed (a new or empty file), else the edits.
    // A throwaway file in the session's scratch folder is the model's own: written whole freely
    // (nothing of the user's to protect there).
    let in_scratch = util::is_inside(&absolute, &util::scratch_dir(&session.workspace));
    // A file this turn created is the model's own draft: written whole again freely (seen: a test
    // file it had just written refused twice when it rewrote it).
    let own_draft = core.created.lock().is_ok_and(|created| created.get(session_id).is_some_and(|files| files.contains(&absolute)));
    let rewrite_refused = |existing: &Option<String>| {
        !in_scratch
            && !own_draft
            && existing.as_ref().is_some_and(|text| !text.trim().is_empty() && !is_skeleton(&absolute, text))
            && edit_format(session.context) != EditFormat::Whole
    };
    let content = args.get("content").and_then(Value::as_str).filter(|_| !args.contains_key("edits") || !rewrite_refused(&current));
    if let Some(content) = content {
        // `content` writes a new (or empty) file. An existing file is changed only where it needs
        // changing, never written whole: a whole write replaces code that works and hides what
        // changed (seen: "clean rewrites" that dropped working functions). A full rewrite the user
        // asked for is one edit over the file's lines. The refusal carries the file with its
        // anchors for the next call.
        if let Some(existing) = &current
            && !in_scratch
            && !own_draft
            && !existing.trim().is_empty()
            && !is_skeleton(&absolute, existing)
            && edit_format(session.context) != EditFormat::Whole
        {
            let view = read_target(core, host, session, &display, None, true, Default::default()).await;
            let (text, artifact) = offload_text(core, view.output, core.config.read_limits);
            let how = match edit_format(session.context) {
                EditFormat::Replace => "with old/new replacements",
                _ => "with anchored edits, one or two top-level definitions per call",
            };
            let mut text = text;
            if !text.ends_with('\n') {
                text.push('\n');
            }
            let mut out = Out::fail(
                "whole_rewrite",
                format!(
                    "Nothing was written: {display} exists (a file is written whole again only when the user asks). Its current content:\n{text}Change it {how}, only the lines the task needs."
                ),
            );
            out.display = view.display;
            out.artifact = artifact;
            return out;
        }
        if let Some(existing) = &current
            && !in_scratch
        {
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
        let (mut ops, mut notes) = match parse_ops(edits) {
            Ok(parsed) => parsed,
            Err(message) => return Out::fail("bad_edit", message),
        };
        // A delete whose `to` is the line right before its `at` names one line (seen: `at` 10 with
        // `to` 9, refused and sent again as `at` 10 alone): that line is deleted, said in the result.
        let mut reversed: Vec<String> = Vec::new();
        for op in ops.iter_mut() {
            let lines = match (&op.at, &op.to) {
                (hashedit::LineRef::Tagged(at), Some(hashedit::LineRef::Tagged(to))) if op.kind == hashedit::OpKind::Delete && to.line + 1 == at.line => {
                    Some((at.line, to.line))
                }
                _ => None,
            };
            if let Some((at, to)) = lines {
                op.to = None;
                reversed.push(format!("Deleted line {at} only: `to` (line {to}) was before `at`."));
            }
        }
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
        reversed.extend(shorten_nested(&doc, &mut ops));
        return match hashedit::apply(&doc, view.as_ref(), &ops) {
            Ok(applied) => {
                // One edit replaces at most two definitions of an existing file: a whole rewrite
                // sent as many line ranges breaks on one range off by a line, and the whole
                // generation is lost (seen: four 3k-token implementations refused in a row).
                // Only code written over code counts: a delete (or a replace with a line or two)
                // generates nothing to lose (seen: dead code after a refactor refused four times).
                let written: Vec<(usize, usize)> = ops
                    .iter()
                    .filter(|op| op.lines.len() > 2)
                    .filter_map(|op| {
                        let line = |reference: &hashedit::LineRef| match reference {
                            hashedit::LineRef::Tagged(anchor) => Some(anchor.line),
                            hashedit::LineRef::Bare(line) => Some(*line),
                            hashedit::LineRef::Start => None,
                        };
                        let at = line(&op.at)?;
                        Some((at, op.to.as_ref().and_then(line).unwrap_or(at)))
                    })
                    .collect();
                if !in_scratch
                    && written_lines(&ops) > SMALL_EDIT_LINES
                    && let Some(replaced) = replaced_definitions(&absolute, &text, &applied.line_map, &written)
                    && replaced.len() > MAX_DEFINITIONS_PER_EDIT
                {
                    let names: Vec<String> = replaced.iter().take(6).map(|name| format!("`{name}`")).collect();
                    return Out::fail(
                        "too_big",
                        format!(
                            "Nothing was written: this edit replaces {} top-level definitions of {display} at once ({}{}). Change an existing file one or two top-level definitions per edit (a class with its methods is one); the anchors of the lines you do not change stay valid between edits.",
                            replaced.len(),
                            names.join(", "),
                            if replaced.len() > 6 { ", ..." } else { "" }
                        ),
                    );
                }
                // Anchors mapped through the session's own edits are journaled, not told.
                if !applied.relocations.is_empty() {
                    notes
                        .push(format!("anchors from your earlier read, mapped through your own edits: {}", applied.relocations.join(", ")));
                }
                let new = applied.doc.to_text();
                let retry = (absolute.clone(), display.clone(), current.as_ref().map(|_| text.clone()), notes.clone());
                let old = current.map(|_| text);
                let mut out = commit(
                    core,
                    host,
                    session,
                    Commit { absolute, display, old, new, line_map: Some(applied.line_map), changed: applied.changed, notes },
                );
                // A range that ends in the wrong place (short of lines its text already holds, past
                // a closing line its text leaves out, or on a block the next line closes anyway):
                // the edit is written with the range corrected when that is what makes it parse,
                // said in a note (seen: the exact anchor named three times and the same range sent
                // again each time). Any other refusal stays as it is.
                if !out.ok
                    && out.error_kind.as_deref() == Some("syntax_error")
                    && let Some(fixed) = retry_with_range_fix(core, host, session, &doc, view.as_ref(), &ops, in_scratch, retry)
                {
                    return fixed;
                }
                if out.ok && !reversed.is_empty() {
                    out.output.insert_str(0, &format!("{}\n", reversed.join("\n")));
                }
                // Operations whose anchors no longer named their line: the rest was written.
                if !applied.skipped.is_empty() {
                    out.output.push_str(&applied.skipped);
                }
                // A refused edit whose cause is a one-line replace carrying many lines (no `to`):
                // the lines after the anchor stayed, which is where the extra braces come from
                // (seen: four full implementations refused before the model gave the range).
                if !out.ok {
                    let old_line = |line: usize| doc.lines.get(line.wrapping_sub(1)).map(|text| text.trim().to_owned());
                    // The anchor to copy (`121:ab`): a bare line number gets the same call resent.
                    let anchor = |line: usize| if (1..=doc.lines.len()).contains(&line) { format!("{line}:{}", doc.tag(line - 1)) } else { line.to_string() };
                    let closing = |text: &str| !text.is_empty() && text.chars().all(|c| matches!(c, '}' | ')' | ']' | ';' | ',')) || text == "end";
                    for (index, op) in ops.iter().enumerate() {
                        let first = op.lines.iter().map(|line| line.trim()).find(|line| !line.is_empty()).unwrap_or("");
                        let last = op.lines.iter().rev().map(|line| line.trim()).find(|line| !line.is_empty()).unwrap_or("");
                        if op.kind == hashedit::OpKind::Replace && op.to.is_none() && op.lines.len() > 1 {
                            let _ = writeln!(out.output, "Edit #{} replaced only its `at` line with {} lines (no `to`): the lines after it stayed. Give `to`, the last line to replace.", index + 1, op.lines.len());
                        }
                        // Where to look: a range that ends one line before a closing line its text
                        // also ends with, or starts one line after a line its text also starts with
                        // (seen: three refusals in one task, each a range ending one line early).
                        if op.kind == hashedit::OpKind::Replace
                            && let Some(hashedit::LineRef::Tagged(to)) = &op.to
                            && let Some(after) = old_line(to.line + 1)
                            && closing(&after)
                            && after == last
                        {
                            let _ = writeln!(out.output, "Look at edit #{}: its text ends with `{last}`, and line {} right after its `to` is `{after}` too: the range ends one line early. Give `to` as `{}`.", index + 1, to.line + 1, anchor(to.line + 1));
                        }
                        if op.kind == hashedit::OpKind::Replace
                            && let hashedit::LineRef::Tagged(at) = &op.at
                            && at.line > 1
                            && let Some(before) = old_line(at.line - 1)
                            && before.len() > 3
                            && before == first
                        {
                            let _ = writeln!(out.output, "Look at edit #{}: its text starts with `{first}`, and line {} right before its `at` is the same: the range starts one line late. Give `at` as `{}`.", index + 1, at.line - 1, anchor(at.line - 1));
                        }
                    }
                }
                out
            }
            Err(rejection) => {
                // The rejection shows the current lines with their anchors: those are the session's
                // view now, so the same anchors sent again name exactly what was shown (seen: one
                // ambiguous anchor sent back twice, refused the same way each time).
                if matches!(rejection.error_kind, "ambiguous_anchor" | "stale_anchor") {
                    core.read_state.record_read(session_id, &absolute, &hash, &doc.lines);
                }
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
        targets.push(match resolve_in_workspace(core, session, path) {
            Ok(path) => path,
            Err(out) => return *out,
        });
        if let FileOp::Update { move_to: Some(target), .. } = op {
            match resolve_in_workspace(core, session, target) {
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
        let absolute = match resolve_in_workspace(core, session, path) {
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
                        let target_abs = match resolve_in_workspace(core, session, target) {
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
    let planned_paths: Vec<PathBuf> = plans
        .iter()
        .map(|plan| match plan {
            Planned::Write { absolute, .. } | Planned::Remove { absolute, .. } => absolute.clone(),
        })
        .collect();
    if let Some(out) = reserved_elsewhere(core, host, session, &planned_paths).await {
        return out;
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
            Planned::Remove { absolute, display } => {
                let before = std::fs::read(&absolute).ok();
                let removed = util::remove_file_checked(&absolute, |path| util::is_inside(path, &session.workspace) && !core.sandbox.protected(path) && !core.sandbox.git_control_write_denied(&session.workspace, path));
                if removed.is_ok() {
                    core.tree.reserve(&absolute, &session.workspace, &session.context.session_id);
                    core.steps.open(&session.workspace).record_edit(&crate::undo::Attribution::session(&session.context.session_id, "edit", &display), &display, before.as_deref(), None, 0);
                }
                match removed {
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
                }
            }
        }
    }
    drop(guards);
    combined.display = json!({ "files": displays });
    combined
}

/// Run `command` on a member of the mesh, in its copy of the tree, through `mesh.call` and the
/// member's own `tools.run`; the output comes back with the member's paths shown as this tree's.
async fn run_remote(core: &Arc<Core>, host: &Arc<dyn Host>, session: &Session<'_>, runner: &crate::core::RunnerInfo, command: &str, cwd: Option<&Path>, timeout_ms: u64) -> Out {
    let relative = cwd.and_then(|dir| dir.strip_prefix(&session.workspace).ok()).filter(|rel| !rel.as_os_str().is_empty());
    let remote_command = match relative {
        Some(rel) => format!("cd {} && {command}", util::shell_quote(&rel.to_string_lossy())),
        None => command.to_owned(),
    };
    let params = json!({
        "node": runner.node,
        "method": cc_protocol::contracts::tools::methods::RUN,
        "params": {"workspace": runner.remote_workspace, "command": remote_command, "timeout_ms": timeout_ms, "session_id": format!("mesh-{}", session.context.session_id), "env": {}}
    });
    let started = Instant::now();
    let answer = match host.call(cc_protocol::contracts::mesh::methods::CALL, params).await {
        Ok(answer) => answer,
        Err(error) => {
            // The member is gone: this and the session's next commands run here.
            core.set_runner(&session.context.session_id, None);
            return Out::fail("unavailable", format!("Not run on {}: {}. The command runs here from now on; send it again.", runner.name, error.message));
        }
    };
    let exit_code = answer.get("exit_code").and_then(Value::as_i64).map(|code| code as i32);
    let duration_ms = answer.get("duration_ms").and_then(Value::as_u64).unwrap_or(started.elapsed().as_millis() as u64);
    let timed_out = answer.get("timed_out").and_then(Value::as_bool).unwrap_or(false);
    let output = answer.get("output").and_then(Value::as_str).unwrap_or("").replace(&runner.remote_workspace, &session.workspace.to_string_lossy());
    let mut text = util::relative_paths(&output, &session.workspace);
    if !text.is_empty() && !text.ends_with('\n') {
        text.push('\n');
    }
    let _ = writeln!(text, "[ran on {}; files it changed there come back here as a step]", runner.name);
    let (ok, kind) = if timed_out {
        let _ = writeln!(text, "{}", timeout_line(timeout_ms, None));
        (false, Some("timeout"))
    } else {
        match exit_code {
            Some(code) => {
                let _ = writeln!(text, "{}", exit_line(code, duration_ms));
                (code == 0, (code != 0).then_some("nonzero_exit"))
            }
            None => {
                let _ = writeln!(text, "[finished without an exit status]");
                (false, Some("nonzero_exit"))
            }
        }
    };
    let tests = answer.get("tests").cloned().unwrap_or(Value::Null);
    let mut out = Out { ok, output: text, error_kind: kind.map(str::to_owned), ..Out::default() };
    out.display = json!({ "exit_code": exit_code, "duration_ms": duration_ms, "timed_out": timed_out, "tests": tests, "remote": {"node": runner.node, "name": runner.name} });
    out
}

/// A refusal when one of `paths` is reserved by another session (its edit of this turn is not
/// done): who holds it and the step to take. None when every path is free.
async fn reserved_elsewhere(core: &Core, host: &Arc<dyn Host>, session: &Session<'_>, paths: &[PathBuf]) -> Option<Out> {
    for path in paths {
        let Some(other) = core.tree.reserved_by_other(path, &session.context.session_id) else { continue };
        let display = session.display(path);
        let who = match host.call("frame.session.get", json!({"id": other})).await {
            Ok(value) => match value.get("agent").and_then(Value::as_str).filter(|agent| !agent.is_empty()) {
                Some(agent) => format!("agent @{agent}"),
                None => "another chat session".to_owned(),
            },
            Err(_) => "another session".to_owned(),
        };
        return Some(Out::fail(
            "reserved",
            format!("Not written: {display} is being edited by {who} right now; it frees when their turn ends. Work on another file first, then come back to this one."),
        ));
    }
    None
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
        command_access: runtime.map(|runtime| runtime.command_access.clone()),
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

/// Bytes of output a test run may have for its compact view (see [`crate::testout`]).
pub const TEST_VIEW_BYTES: u64 = 1 << 20;

/// A failing test run shown in the form the model acts on (see [`crate::testout`]): its counts,
/// one line per failing test, the first failure whole, and the whole output kept as an artifact
/// for the rest. Any other output is `text` as given.
fn test_view(core: &Core, whole: Option<String>, text: String, artifact: Option<String>) -> (String, Option<String>) {
    let Some(whole) = whole else { return (text, artifact) };
    // Tests the runner could not reach: said last, passing run or not, since a run without them
    // reads as green (seen: the tests of the case being fixed never ran, and the fix was taken as done).
    let unreachable = never_run_line(&whole);
    // A runner that found nothing to run: what that means, and the step after it (seen: `npm test`
    // run twice, then a search of the whole disk for the tests).
    if crate::testout::found_none(&whole) {
        let gap = if text.is_empty() || text.ends_with('\n') { "" } else { "\n" };
        return (format!("{text}{gap}[No tests ran: this command found none in the project. Add tests to the project, or check the change with a probe in `tmp/`.]\n{unreachable}"), artifact);
    }
    let Some(summary) = crate::testout::present(&whole) else {
        if unreachable.is_empty() {
            return (text, artifact);
        }
        let gap = if text.is_empty() || text.ends_with('\n') { "" } else { "\n" };
        return (format!("{text}{gap}{unreachable}"), artifact);
    };
    let id = match artifact {
        Some(id) => id,
        None => match core.artifacts.store(whole.as_bytes()) {
            Ok(id) => id,
            Err(_) => return (text, None),
        },
    };
    (format!("{summary}\nEvery failure whole: read artifact://{id}\n{unreachable}"), Some(id))
}

/// A test run's output in its compact form (see [`crate::testout`]) for text that did not come from a
/// command of this session (`tools.tests.report`: output the user pasted or attached): the report and
/// the line naming tests that never ran (None when the text is not a runner's report with either),
/// and the notes alone, for output shown whole (what a failure means, tests that never ran).
pub fn tests_report(text: &str) -> (Option<String>, Option<String>) {
    let summary = crate::testout::present(text);
    let unreachable = never_run_line(text);
    let mut notes: String = crate::testout::notes(text).iter().map(|note| format!("{note}\n")).collect();
    notes.push_str(&unreachable);
    let notes = (!notes.is_empty()).then_some(notes);
    let report = match (summary, unreachable.is_empty()) {
        (None, true) => None,
        (None, false) => Some(unreachable),
        (Some(summary), _) => Some(format!("{summary}{unreachable}")),
    };
    (report, notes)
}

/// The line that names tests the runner found but could not run (see [`crate::testout::never_run`]),
/// ending with the step that makes them run; empty when there are none.
fn never_run_line(output: &str) -> String {
    let spots = crate::testout::never_run(output);
    if spots.is_empty() {
        return String::new();
    }
    let named: Vec<String> = spots.iter().map(|spot| format!("`{spot}`")).collect();
    let (count, verb) = if spots.len() == 1 { ("1 test".to_owned(), "is") } else { (format!("{} tests", spots.len()), "are") };
    format!(
        "[{count} never ran: {} {verb} inside another function, where the test runner cannot reach {}. A `}}` is missing above {}: close the function before {}.]\n",
        named.join(", "),
        if spots.len() == 1 { "it" } else { "them" },
        if spots.len() == 1 { "it" } else { "each" },
        if spots.len() == 1 { "it" } else { "each" },
    )
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

/// A plain push the git plugin can carry out: `git push [-u|--set-upstream] [remote [branch]]`,
/// as (remote, branch); `HEAD` is the current branch. None for any other form (a refspec with `:`,
/// `--force`, `--tags`, several branches), which runs as typed.
fn plain_push(command: &str) -> Option<(Option<String>, Option<String>)> {
    let argv = crate::approval::simple_argv(command)?;
    let [git, push, rest @ ..] = argv.as_slice() else { return None };
    if git != "git" || push != "push" {
        return None;
    }
    let words: Vec<&String> = rest.iter().filter(|word| !matches!(word.as_str(), "-u" | "--set-upstream")).collect();
    if words.len() > 2 || words.iter().any(|word| word.starts_with('-') || word.contains(':')) {
        return None;
    }
    let remote = words.first().map(|word| word.to_string());
    let branch = words.get(1).map(|word| word.to_string()).filter(|branch| branch != "HEAD");
    Some((remote, branch))
}

/// Programs that walk a directory tree for files or text.
const SEARCHERS: &[&str] = &["find", "grep", "egrep", "fgrep", "rg", "ag", "ack", "fd"];

/// Whether a command searches the whole machine: a tree-walking search given `/` or the home folder
/// as a path (structural: the program's name and a `/`, `~` or home-folder word, in any simple
/// command of the script; `$HOME` is an expansion, so it is matched in the raw words).
fn searches_whole_disk(command: &str) -> bool {
    let raw_home = command.split_whitespace().any(|word| matches!(word.trim_end_matches('/'), "$HOME" | "${HOME}" | "\"$HOME\"" | "\"${HOME}\""));
    crate::approval::simple_commands(command).iter().any(|words| {
        let Some(Some(program)) = words.first() else { return false };
        let name = program.rsplit('/').next().unwrap_or(program);
        // `/` or the home folder: both hold the whole machine's files (seen: `find ~` for minutes).
        let home = std::env::var("HOME").unwrap_or_default();
        // `/` (also `//`), never an empty word: `grep -rn "" tests/` searches `tests/`.
        let whole = |word: &str| {
            let trimmed = word.trim_end_matches('/');
            (!word.is_empty() && trimmed.is_empty())
                || matches!(trimmed, "~" | "$HOME" | "${HOME}")
                || (!home.is_empty() && !trimmed.is_empty() && trimmed == home.trim_end_matches('/'))
        };
        SEARCHERS.contains(&name) && (raw_home || words.iter().skip(1).any(|word| word.as_deref().is_some_and(whole)))
    })
}

/// The directories commands may write, in words, for model-facing explanations.
const WRITABLE: &str = "the workspace, temp dirs, package caches and your Python venv";

/// The step after the sandbox kept a program from starting.
const PROGRAM_STEP: &str = "When the user's request needs what it shows, run the same command with `permission` saying why: the user is asked once, then it runs outside the sandbox. Otherwise answer from what you can read.";

/// Whether `path` is a program file (a regular file someone may execute), as opposed to a place a
/// command tried to write.
fn is_program(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
}

/// The step after the sandbox stopped a command.
const LIFT_STEP: &str = "If the task cannot be done without it (an install), run the command that does it with `permission` saying why: an install changes the user's machine, so the user is asked first; once they approve it runs. Never ask for what commands may do already, and never change the user's shell setup, tool settings or files outside the project to get around it.";

/// The programs a shell reported missing in command output (`command not found`, as sh, bash and
/// zsh word it), in order, once each.
fn missing_programs(output: &str) -> Vec<String> {
    let word = |text: &str| text.split_whitespace().next().map(|word| word.trim_matches(|c| c == '\'' || c == '`').to_owned()).filter(|word| !word.is_empty());
    let mut programs: Vec<String> = Vec::new();
    for line in output.lines().map(str::trim_end) {
        let program = match line.split_once("command not found: ") {
            Some((_, rest)) => word(rest),
            None => line.strip_suffix(": command not found").and_then(|head| head.rsplit(": ").next()).and_then(word),
        };
        if let Some(program) = program
            && !programs.contains(&program)
        {
            programs.push(program);
        }
    }
    programs
}

/// The end of a run's result: its exit (the time only when it took over 10 s).
fn exit_line(code: i32, duration_ms: u64) -> String {
    if duration_ms > 10_000 { format!("[exit {code}, {}s]", duration_ms / 1000) } else { format!("[exit {code}]") }
}

/// A command that ran out of time, with what it was really running then (a test binary, not the
/// build: seen read as "the build is slow" four times while the tests hung).
fn timeout_line(timeout_ms: u64, running: Option<&str>) -> String {
    let running = running.map(|what| format!(" At that point it was running {what}.")).unwrap_or_default();
    format!(
        "[timed out after {} s; the command and everything it started were killed.{running} If it waits for input or never ends, fix that; a run that needs longer (a long build or test) or a server goes in background: true, which has no time limit.]",
        timeout_ms.div_ceil(1000)
    )
}

/// Stop this session's background jobs that run what `command` runs in `cwd`: running it again
/// means the earlier run is not waited for any more (seen: one test started three times, all
/// three running and growing). The note for the new run's result, when any was stopped.
async fn stop_repeats(core: &Core, session_id: &str, cwd: &Path, command: &str) -> Option<String> {
    let repeats = core.jobs.same_command(session_id, cwd, command);
    if repeats.is_empty() {
        return None;
    }
    let mut named = Vec::new();
    for job in &repeats {
        let what = crate::guard::describe(&crate::guard::tree(job.pid), &job.cwd).map(|what| format!(", which was running {what}")).unwrap_or_default();
        core.jobs.stop(&job.job_id, "[stopped by the host: the same command was started again]", StopMode::Escalate).await;
        named.push(format!("{}{what}", job.job_id));
    }
    Some(format!("[background job {} ran the same command; it was stopped]", named.join(" and ")))
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
                        Ok(value) => value
                            .get("argv")
                            .and_then(Value::as_array)
                            .map(|argv| argv.iter().filter_map(Value::as_str).map(str::to_owned).collect::<Vec<_>>()),
                        Err(error) => {
                            return Out::fail("unavailable", format!("the project's container gives no shell: {}", error.message));
                        }
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
        Err(error) => {
            return Out::fail("container_error", format!("could not run the command in the project's container: {}", error.message));
        }
    };
    let output = result.get("output").and_then(Value::as_str).unwrap_or("").as_bytes().to_vec();
    let code_hits = verifier_hits(host, command, &output[..]).await;
    let exit_code = result.get("exit_code").and_then(Value::as_i64).map(|code| code as i32);
    let duration_ms = result.get("duration_ms").and_then(Value::as_u64).unwrap_or(0);
    let timed_out = result.get("timed_out").and_then(Value::as_bool).unwrap_or(false);
    let captured = Captured::Mem(output);
    let whole = captured.whole(TEST_VIEW_BYTES);
    let tests = whole.as_deref().and_then(crate::testout::summary);
    // A test command that found no tests: the host's done check reads it (a check that ran nothing).
    let no_tests = whole.as_deref().is_some_and(crate::testout::found_none);
    let (text, artifact, bytes) = present_output(core, captured, core.config.run_limits);
    let (mut text, artifact) = test_view(core, whole, text, artifact);
    if !text.is_empty() && !text.ends_with('\n') {
        text.push('\n');
    }
    let (ok, kind) = match (timed_out, exit_code) {
        (true, _) => {
            let _ = writeln!(text, "{}", timeout_line(timeout_ms, None));
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
    out.display = json!({ "exit_code": exit_code, "duration_ms": duration_ms, "timed_out": timed_out, "artifact": artifact, "bytes": bytes, "tests": tests, "no_tests": no_tests, "container": container.id });
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
    // Every command starts in the workspace (see `crate::shell`); a relative `cwd` is from there.
    let shell_cwd = session.workspace.clone();
    let cwd = args.get("cwd").and_then(Value::as_str).map(|cwd| util::resolve(&shell_cwd, cwd));
    if let Some(cwd) = &cwd
        && !cwd.is_dir()
    {
        return Out::fail("not_found", format!("cwd {} is not a directory", cwd.display()));
    }
    let effective_cwd = cwd.clone().unwrap_or(shell_cwd);
    // A search of the whole machine takes minutes and finds nothing the project needs (seen: a
    // model hunting a missing file with `find /` and `grep -r /`): answered with where to search.
    // Files are written with edit (line anchors, the result shows the change, the host tracks
    // them), not through the terminal (seen: a check rewritten whole with `cat > f <<EOF` again
    // and again).
    // A plain look at one project file through the terminal (`cat f`, `head -40 f`, `sed -n
    // '10,40p' f`) is answered as a `read`: the same lines, with anchors for edit, and tracked as a
    // read (seen: `cat bowling.py` three times in one task; the next edit had no anchors and the
    // reads stayed whole in the history).
    if let Some((file, range)) = crate::approval::terminal_read(command)
        && let Ok(absolute) = resolve_in_workspace(core, session, &file)
        && absolute.is_file()
    {
        let mut read_args = Map::new();
        let path = match &range {
            Some(range) if range.starts_with('-') => {
                // `tail -N`: the last N lines of the file.
                let total = crate::util::read_host_text(&absolute).map(|text| text.lines().count()).unwrap_or(0);
                let last: usize = range[1..].parse().unwrap_or(10);
                format!("{file}:{}-{total}", total.saturating_sub(last) + 1)
            }
            Some(range) => format!("{file}:{range}"),
            None => file.clone(),
        };
        read_args.insert("path".to_owned(), Value::String(path));
        let mut out = exec_read(core, host, session, &read_args).await;
        out.output = format!("(Shown with the read tool, so the lines carry anchors for edit; use read for files.)\n{}", out.output);
        return out;
    }
    // A here-document into the scratch folder `tmp/` or outside the project (`/tmp/x`) runs: a
    // refusal there protects nothing and costs a step. Project files go through edit.
    let scratch = util::scratch_dir(&session.workspace);
    let own = |target: &str| {
        let absolute = util::resolve(&session.workspace, &util::expand_scratch(target, &scratch));
        util::is_inside(&absolute, &scratch) || !util::is_inside(&absolute, &session.workspace)
    };
    let edits = crate::approval::terminal_edits(command, &own);
    if !edits.is_empty() {
        return Out::fail(
            "terminal_edit",
            format!("Not run: it {} through the terminal. Write the file with edit (a project file by its path, a throwaway one as `tmp/<name>`), then run it.", edits.join(" and ")),
        );
    }
    if searches_whole_disk(command) {
        return Out::fail(
            "whole_disk_search",
            format!("Not run: `/` and the home folder hold the whole machine, and searching them takes minutes. Search the project ({}) or a folder you know, e.g. `grep -rn <text> .`.", session.workspace.display()),
        );
    }
    let runtime = match core.runtime(host.as_ref(), &session.context.session_id, &session.workspace, session.python_env).await {
        Ok(runtime) => runtime,
        Err(message) => return Out::fail("sandbox_unavailable", message),
    };
    let venv_path = runtime.venv.as_ref().map(|venv| venv.path().to_path_buf());
    let writable = |path: &Path| core.sandbox.writable(path, &session.workspace, venv_path.as_deref(), &runtime.grants);
    let session_id = &session.context.session_id;
    // Platform rule: the repository's state (commits, branches, files git restores) changes only on
    // the user's word, never on the model's decision. Each such command waits for the user's
    // approval of that exact command, in every mode; reading history runs at once (seen: a model's
    // `git checkout <file>` threw away twenty minutes of its own work).
    if let Some(git) = crate::approval::git_change(command) {
        let class = format!("git_change:{git}");
        // An approval runs the command once; the next one is asked again.
        if !core.take_approval(session_id, &class) {
            let mut out = Out::fail(
                "needs_approval",
                format!(
                    "Not run: `{git}` changes the repository, which only the user decides; they have been asked about this command. Meanwhile read with git (status, diff, log, show, blame) and keep working with edit; a file's committed text is `git show HEAD:<path>`."
                ),
            );
            out.needs_approval = true;
            out.display = json!({ "approval": { "classes": [class], "concerns": [], "command": command, "always": false } });
            return out;
        }
        // An approved plain push goes through the git plugin, the same push as the app's Push
        // button: the repository's own setup first, else the GitHub sign-in from Settings.
        if let Some((remote, branch)) = plain_push(command) {
            let params = json!({ "root": effective_cwd.display().to_string(), "remote": remote, "branch": branch });
            return match host.call(cc_protocol::contracts::vcs::methods::PUSH, params).await {
                Ok(result) => {
                    let text = |key: &str| result.get(key).and_then(Value::as_str).unwrap_or_default().to_owned();
                    if result.get("ok").and_then(Value::as_bool) == Some(true) {
                        Out::ok(format!("Pushed {} to {}. {}", text("branch"), text("remote"), text("detail")).trim_end().to_owned())
                    } else {
                        Out::fail("push_failed", format!("Push failed: {}", text("detail")))
                    }
                }
                Err(error) => Out::fail("push_failed", format!("Push failed: {}", error.message)),
            };
        }
    }
    // Past the sandbox: the model asks with `permission` (why the command needs it); the user decides
    // in every mode, and the approved command runs without the sandbox.
    let permission = args.get("permission").and_then(Value::as_str).map(str::trim).filter(|why| !why.is_empty());
    let lift = sandbox::lift_class(command);
    let lifted = core.approved(session_id).contains(&lift);
    let mut classes: Vec<String> = Vec::new();
    let mut reasons: Vec<String> = Vec::new();
    let mut concerns: Vec<approval::Concern> = Vec::new();
    if session.context.mode == "ask" {
        let approved = core.approved(session_id);
        let all = approval::assess(command, &approval::Context { cwd: &effective_cwd, workspace: &session.workspace });
        // A deletion or overwrite outside the workspace the user is asked about anyway widens the
        // sandbox for its folder; a command past the sandbox needs no folder.
        let mut write_classes: Vec<String> = Vec::new();
        if !lifted && permission.is_none() {
            for concern in &all {
                if let Some(path) = &concern.path
                    && !writable(path)
                {
                    write_classes.push(sandbox::write_class(path));
                }
            }
        }
        write_classes.sort();
        write_classes.dedup();
        concerns = all.into_iter().filter(|concern| !approved.contains(concern.class)).collect();
        classes = concerns.iter().map(|concern| concern.class.to_owned()).collect();
        classes.dedup();
        classes.extend(write_classes.iter().cloned());
        reasons = concerns.iter().map(|concern| format!("{} ({})", describe_class(concern.class), concern.detail)).collect();
        reasons.extend(write_classes.iter().map(|class| describe_class(class)));
    }
    // An offline workspace (the marker file) lets nothing past the sandbox: an install, a download
    // or a write outside the project cannot run here whoever approves it, so it is a result, not a
    // question (seen: an unattended run waiting for a human on a `pip install`). Said as what it
    // is: a write outside the project was refused as "offline" and read as a network problem.
    if permission.is_some() && !lifted && session.workspace.join(crate::sandbox::OFFLINE_MARKER).exists() {
        return Out::fail(
            "offline",
            "Not run: in this workspace no command runs past the sandbox, so nothing can be installed, downloaded or written outside the project. Work with what the project has; when a check cannot run here, say so.".to_owned(),
        );
    }
    let asks_lift = permission.is_some() && !lifted;
    if let Some(why) = permission
        && !lifted
    {
        classes.push(lift.clone());
        reasons.push(format!("runs past the sandbox: it may write outside the workspace and use the network ({why})"));
    }
    if !classes.is_empty() {
        let mut output = String::from("Not run: this command needs the user's approval because it ");
        output.push_str(&reasons.join("; "));
        output.push_str(". The user has been asked; continue with other work or wait for their answer.\n");
        let mut out = Out::fail("needs_approval", output);
        out.needs_approval = true;
        out.display = json!({ "approval": { "classes": classes, "concerns": concerns, "command": command, "always": asks_lift } });
        return out;
    }
    let mut identity = identity(core, session, Some(&runtime));
    if lifted {
        identity.sandbox = None;
    }
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
            ("CORTEX_COMMAND_URL".to_owned(), runtime.command_access.url.clone()),
            ("CORTEX_COMMAND_TOKEN".to_owned(), runtime.command_access.token.clone()),
        ];
        let repeats = stop_repeats(core, &identity.session_id, &effective_cwd, command).await;
        return match core.jobs.start(StartRequest {
            session_id: &identity.session_id,
            project_id: &identity.project_id,
            command,
            cwd: &effective_cwd,
            env_file: env_file.as_deref(),
            identity_env,
            wake: wake.clone(),
            sandbox: identity.sandbox.as_ref(),
            venv: venv_path.as_deref(),
            toolchain: toolchain.clone(),
        }) {
            Ok(meta) => {
                let mut line = repeats.map(|note| format!("{note}\n")).unwrap_or_default();
                line.push_str(&started_line(&meta.job_id, &meta.artifact, &wake));
                let mut out = Out::ok(line);
                out.display = json!({ "job_id": meta.job_id, "pid": meta.pid, "artifact": meta.artifact, "background": true });
                out
            }
            Err(error) => Out::fail("job_error", format!("could not start the job: {error}")),
        };
    }
    let timeout_ms = args.get("timeout_ms").and_then(Value::as_u64).unwrap_or(core.config.default_timeout_ms);
    let timeout = Duration::from_millis(timeout_ms);
    // Borrowed hands: a member of the mesh runs this session's commands in its copy of the tree
    // (kept in step by the mesh); what the command changes there comes back as a step here.
    // Only a session of a project (the model's own runs; not the host's checks, not the probe).
    if !lifted
        && !session.context.project_id.is_empty()
        && session.context.project_id != "probe"
        && session.python_env
        && let Some(runner) = core.runner(host.as_ref(), session_id, &session.workspace, &session.context.project_id).await
    {
        return run_remote(core, host, session, &runner, command, cwd.as_deref(), timeout_ms).await;
    }
    // One tree, many hands: a check (a verifier the language plugins declare) runs on a throw-away
    // copy of the tree while another session is editing or running in it, so a half-finished edit
    // of someone else cannot make it lie. Alone in the tree, it runs in the tree.
    let mut cwd = cwd;
    let mut check_copy: Option<crate::tree::CheckCopy> = None;
    if !lifted && core.tree.others_active(&session.workspace, session_id) && core.tree.is_check(host.as_ref(), command).await {
        let caches = core.tree.caches(host.as_ref()).await;
        let (home, git, workspace) = (core.home.clone(), core.steps.git().map(Path::to_path_buf), session.workspace.clone());
        match tokio::task::spawn_blocking(move || crate::tree::copy_for_check(&home, git.as_deref(), &workspace, &caches)).await {
            Ok(Ok(copy)) => {
                let mut grants = runtime.grants.clone();
                grants.push(copy.path.clone());
                if let Some(sandbox) = core.sandbox.sandbox(&session.workspace, venv_path.as_deref(), &grants) {
                    identity.sandbox = Some(sandbox);
                }
                let relative = cwd.as_ref().and_then(|dir| dir.strip_prefix(&session.workspace).ok().map(Path::to_path_buf));
                cwd = Some(match relative {
                    Some(relative) => copy.path.join(relative),
                    None => copy.path.clone(),
                });
                check_copy = Some(copy);
            }
            Ok(Err(error)) => tracing::warn!(%error, "could not copy the tree for a check; running in the tree"),
            Err(error) => tracing::warn!(%error, "could not copy the tree for a check; running in the tree"),
        }
    }
    let repeats = stop_repeats(core, session_id, &effective_cwd, command).await;
    let sink = items.clone();
    core.tree.begin_run(&session.workspace, session_id);
    let result = core
        .shells
        .run(&identity, RunRequest { command, cwd, env: Vec::new(), toolchain, timeout }, cancel, move |chunk: &[u8]| {
            if !chunk.is_empty() {
                sink.send(json!({ "kind": "output", "text": String::from_utf8_lossy(chunk) }));
            }
        })
        .await;
    core.tree.end_run(&session.workspace, session_id);
    let run = match result {
        Ok(run) => run,
        Err(error) => return Out::fail("shell_error", format!("could not run the command: {error}")),
    };
    // EPERM names a path. A program file there could not start: macOS never lets a sandboxed
    // command start a program that takes more rights (setuid tools such as `ps` and `top`), whatever
    // the profile says. Any other path is a refused write.
    let refused = if lifted { Vec::new() } else { refused_paths(&run.output) };
    let blocked_programs: Vec<String> = refused
        .iter()
        .filter(|path| !writable(path.as_path()) && is_program(path.as_path()))
        .filter_map(|path| path.file_name().map(|name| name.to_string_lossy().into_owned()))
        .collect();
    let eperm = refused.iter().any(|path| !writable(path.as_path()) && !is_program(path.as_path()));
    let code_hits = match &run.output {
        Captured::Mem(bytes) => verifier_hits(host, command, &bytes[..]).await,
        Captured::File { path, .. } => {
            use std::os::unix::fs::OpenOptionsExt;
            let opened = std::fs::OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW).open(path);
            match opened {
                Ok(file) => verifier_hits(host, command, std::io::BufReader::new(file)).await,
                Err(_) => Vec::new(),
            }
        }
    };
    let whole = run.output.whole(TEST_VIEW_BYTES).map(|whole| util::relative_paths(&whole, &session.workspace));
    let tests = whole.as_deref().and_then(crate::testout::summary);
    // A test command that found no tests: the host's done check reads it (a check that ran nothing).
    let no_tests = whole.as_deref().is_some_and(crate::testout::found_none);
    let (text, artifact, bytes) = present_output(core, run.output, core.config.run_limits);
    let text = match &check_copy {
        Some(copy) => text.replace(&copy.path.to_string_lossy().into_owned(), &session.workspace.to_string_lossy()),
        None => text,
    };
    let text = util::relative_paths(&text, &session.workspace);
    let (mut text, artifact) = test_view(core, whole, text, artifact);
    if !text.is_empty() && !text.ends_with('\n') {
        text.push('\n');
    }
    if check_copy.take().is_some() {
        text.push_str("[Ran on a copy of the tree as it is now, because another session is editing it; files the command changed were not kept.]\n");
    }
    let seconds = run.duration_ms as f64 / 1000.0;
    // Notes on how it ran come before its exit line, which stays last.
    if let Some(note) = &repeats {
        let _ = writeln!(text, "{note}");
    }
    if !blocked_programs.is_empty() {
        let names: Vec<String> = blocked_programs.iter().map(|name| format!("`{name}`")).collect();
        let _ = writeln!(text, "[{} could not start: a command in the sandbox cannot start a program that needs more rights than it has. {PROGRAM_STEP}]", names.join(", "));
    }
    if eperm {
        let _ = writeln!(text, "[\"{}\": the sandbox stopped this (commands may write only in {WRITABLE}). {LIFT_STEP}]", sandbox::REFUSED_TEXT);
    }
    let missing = missing_programs(&text);
    if !missing.is_empty() {
        let names: Vec<String> = missing.iter().map(|program| format!("`{program}`")).collect();
        let _ = writeln!(text, "[{} not installed. {LIFT_STEP}]", names.join(", "));
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
        let _ = writeln!(text, "{}", timeout_line(timeout_ms, run.running.as_deref()));
        (false, Some("timeout"))
    } else if let Some(note) = &run.stopped {
        let _ = writeln!(text, "{note}");
        (false, Some("stopped"))
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
                // A pipe ends with its last command's status: a failure before a `| tail` is said
                // (seen: `cargo test ... | tail -4` ended `[exit 0]` while the tests crashed).
                // 141 is a command that stopped because `head` stopped reading: not a failure.
                if code == 0
                    && let Some((stage, failed)) = run.pipe_status.iter().enumerate().find(|&(_, &status)| status != 0 && status != 141)
                {
                    let which = if stage == 0 { "the command before the first `|`".to_owned() } else { format!("command {} of the pipe", stage + 1) };
                    let _ = writeln!(text, "[{which} exited {failed}: the exit 0 above is only the pipe's last command]");
                }
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
        "tests": tests,
        "no_tests": no_tests,
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
