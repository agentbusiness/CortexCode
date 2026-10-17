//! Cross-project references in a turn (docs/IDEATION-WORKFLOW.md part 2): the composer's
//! `@<project>` and `@<project>:<symbol>` mentions (`TurnParams::mentions`) and folder attachments
//! of another repository's root.
//!
//! - Each referenced project is opened read-only (`memory.project.open` with `reference: true`:
//!   registered at once, indexed in the background; a session project stays one).
//! - A mentioned symbol is resolved now (`memory.symbol`) when its project's graph is ready; while
//!   it is being indexed the note says so instead of waiting.
//! - The run gets one host note ([`NOTE_PURPOSE`]): per project its root and the qualified forms
//!   the tools answer (`graph://<project>/...`, `symbol://<project>/<name>`, `outline://`,
//!   `definition://<project>:...`, the read target `<project>:<path>`), per symbol its location.
//!
//! A folder counts as another repository when it is outside the session's project and workspace
//! and is a known project's root or has a `.git` entry. Memory is asked once per call, without
//! retries: a turn never waits for a peer's restart; what could not be opened is named in the note.

use crate::data::Dataset;
use crate::host::Host;
use cc_protocol::contracts::chatflow::{Attachment, Mention, attachment_kinds};
use cc_protocol::contracts::memory::{ProjectInfo, ProjectListResult, SymbolMatches, SymbolRef, methods as memory_methods};
use cc_protocol::{RpcError, codes};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

/// Purpose of the note that names the referenced projects to the run.
pub const NOTE_PURPOSE: &str = "references";
/// Locations shown for an ambiguous symbol; the rest are counted.
pub const SYMBOL_MATCHES: usize = 3;

/// A project to open: a known one (by id), or a folder memory does not know yet (empty id).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Target {
    pub id: String,
    pub root: String,
    /// Mentioned symbols, in mention order, without repeats.
    pub symbols: Vec<String>,
}

/// What a mention or folder named that could not be opened.
#[derive(Debug, Clone, PartialEq)]
pub struct NotOpened {
    pub asked: String,
    /// None: no known project has that name or id.
    pub error: Option<String>,
}

/// Where a mentioned symbol is.
#[derive(Debug, Clone, PartialEq)]
pub enum Located {
    /// The first [`SYMBOL_MATCHES`] matches and how many there are.
    At(Vec<SymbolRef>, usize),
    /// The project's graph is still being indexed.
    Pending,
    Missing,
    Failed(String),
}

/// An opened project as the run is told about it.
#[derive(Debug, Clone, PartialEq)]
pub struct Referenced {
    pub id: String,
    pub name: String,
    pub root: String,
    /// `(indexed, total)` files while memory indexes it (`total` 0 while it lists them).
    pub indexing: Option<(u64, u64)>,
    pub symbols: Vec<(String, Located)>,
}

/// A known project by name, else by id (as the tools' qualifiers resolve it).
fn known<'a>(projects: &'a [ProjectInfo], asked: &str) -> Option<&'a ProjectInfo> {
    projects.iter().find(|p| p.name == asked).or_else(|| projects.iter().find(|p| p.project_id == asked))
}

/// The folder attachments that are another repository's root: outside `own` (the session's
/// project root and workspace, canonical) and a known project's root or holding a `.git` entry.
pub fn repository_folders(attachments: &[Attachment], own: &[PathBuf], projects: &[ProjectInfo]) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    for attachment in attachments.iter().filter(|attachment| attachment.kind == attachment_kinds::FOLDER) {
        let Ok(path) = Path::new(&attachment.path).canonicalize() else { continue };
        if own.iter().any(|root| path.starts_with(root)) || out.contains(&path) {
            continue;
        }
        let known_root = projects.iter().any(|p| Path::new(&p.root).canonicalize().is_ok_and(|root| root == path));
        if known_root || path.join(".git").exists() {
            out.push(path);
        }
    }
    out
}

/// The projects to open for `mentions` and `folders`, one target per project (a mention of the
/// session's own project `own_id` is not a reference), and the mentions naming no known project.
pub fn targets(mentions: &[Mention], folders: &[PathBuf], projects: &[ProjectInfo], own_id: &str) -> (Vec<Target>, Vec<NotOpened>) {
    let mut out: Vec<Target> = Vec::new();
    let mut unknown: Vec<NotOpened> = Vec::new();
    for mention in mentions {
        let asked = mention.project.trim();
        if asked == own_id {
            continue;
        }
        let Some(info) = known(projects, asked) else {
            if !unknown.iter().any(|missing| missing.asked == asked) {
                unknown.push(NotOpened { asked: asked.to_owned(), error: None });
            }
            continue;
        };
        if info.project_id == own_id {
            continue;
        }
        let at = match out.iter().position(|target| target.id == info.project_id) {
            Some(at) => at,
            None => {
                out.push(Target { id: info.project_id.clone(), root: info.root.clone(), symbols: Vec::new() });
                out.len() - 1
            }
        };
        if let Some(symbol) = mention.symbol.as_deref().map(str::trim).filter(|symbol| !symbol.is_empty())
            && !out[at].symbols.iter().any(|known| known == symbol)
        {
            out[at].symbols.push(symbol.to_owned());
        }
    }
    for folder in folders {
        let info = projects.iter().find(|p| Path::new(&p.root).canonicalize().is_ok_and(|root| &root == folder));
        let (id, root) = match info {
            Some(info) => (info.project_id.clone(), info.root.clone()),
            None => (String::new(), folder.to_string_lossy().into_owned()),
        };
        if id == own_id || out.iter().any(|target| (!id.is_empty() && target.id == id) || target.root == root) {
            continue;
        }
        out.push(Target { id, root, symbols: Vec::new() });
    }
    (out, unknown)
}

/// What the run reads about the referenced projects (empty when there are none).
pub fn note(ds: &Dataset, referenced: &[Referenced], not_opened: &[NotOpened]) -> String {
    if referenced.is_empty() && not_opened.is_empty() {
        return String::new();
    }
    let mut items: Vec<String> = Vec::new();
    for project in referenced {
        let name = project.name.as_str();
        let state = match project.indexing {
            Some((_, 0)) => ds.text("reference_listing", &[]),
            Some((done, total)) => ds.text("reference_indexing", &[("done", &done.min(total).to_string()), ("total", &total.to_string())]),
            None => String::new(),
        };
        let mut item = ds.text("reference_project", &[("name", name), ("root", &project.root), ("state", &state)]);
        for (asked, located) in &project.symbols {
            let asked = asked.as_str();
            match located {
                Located::At(matches, total) => {
                    for symbol in matches {
                        let (start, end) = (symbol.start_line.to_string(), symbol.end_line.to_string());
                        item.push_str(&ds.text(
                            "reference_symbol",
                            &[("asked", asked), ("kind", &symbol.kind), ("qname", &symbol.qname), ("name", name), ("path", &symbol.path), ("start", &start), ("end", &end)],
                        ));
                    }
                    if *total > matches.len() {
                        item.push_str(&ds.text("reference_symbol_more", &[("count", &(total - matches.len()).to_string()), ("name", name), ("asked", asked)]));
                    }
                }
                Located::Pending => item.push_str(&ds.text("reference_symbol_pending", &[("asked", asked), ("name", name)])),
                Located::Missing => item.push_str(&ds.text("reference_symbol_missing", &[("asked", asked), ("name", name)])),
                Located::Failed(error) => item.push_str(&ds.text("reference_symbol_failed", &[("asked", asked), ("error", error)])),
            }
        }
        items.push(item);
    }
    for missing in not_opened {
        items.push(match &missing.error {
            Some(error) => ds.text("reference_failed", &[("project", &missing.asked), ("error", error)]),
            None => ds.text("reference_unknown", &[("project", &missing.asked)]),
        });
    }
    ds.text("references", &[("items", &items.join("\n"))])
}

fn str_of<'a>(value: &'a Value, field: &str) -> &'a str {
    value.get(field).and_then(Value::as_str).unwrap_or("")
}

/// One mentioned symbol of an opened project.
async fn locate(host: &Host, project_id: &str, symbol: &str) -> Located {
    match host.ctx().call(memory_methods::SYMBOL, json!({"project_id": project_id, "name": symbol})).await {
        Ok(value) => match serde_json::from_value::<SymbolMatches>(value) {
            Ok(found) if found.matches.is_empty() => Located::Missing,
            Ok(found) => {
                let total = found.matches.len();
                Located::At(found.matches.into_iter().take(SYMBOL_MATCHES).map(|detail| detail.symbol).collect(), total)
            }
            Err(error) => Located::Failed(error.to_string()),
        },
        Err(error) if error.code == codes::NOT_FOUND => Located::Missing,
        Err(error) => Located::Failed(error.message),
    }
}

/// Open one target read-only and resolve its symbols.
async fn open(host: &Host, target: &Target) -> Result<Referenced, String> {
    let status = host
        .ctx()
        .call(memory_methods::PROJECT_OPEN, json!({"root": target.root, "project_id": target.id, "reference": true}))
        .await
        .map_err(|error| error.message)?;
    let id = str_of(&status, "project_id").to_owned();
    let name = match str_of(&status, "name") {
        "" => id.clone(),
        name => name.to_owned(),
    };
    let root = match str_of(&status, "root") {
        "" => target.root.clone(),
        root => root.to_owned(),
    };
    let count = |field: &str| status.get(field).and_then(Value::as_u64).unwrap_or(0);
    let state = str_of(&status, "state");
    let indexing = (state == "indexing").then(|| (count("indexed_files"), count("total_files")));
    let mut symbols = Vec::new();
    for symbol in &target.symbols {
        let located = match state {
            "indexing" => Located::Pending,
            "error" => Located::Failed(str_of(&status, "error").to_owned()),
            _ => locate(host, &id, symbol).await,
        };
        symbols.push((symbol.clone(), located));
    }
    Ok(Referenced { id, name, root, indexing, symbols })
}

/// The references of a message, opened and resolved: the note's entry data (`text`, `purpose`,
/// `projects`, `host_message`), or None when the message references no other project.
pub async fn resolve(host: &Host, ds: &Dataset, session_id: &str, mentions: &[Mention], attachments: &[Attachment]) -> Result<Option<Value>, RpcError> {
    if mentions.is_empty() && !attachments.iter().any(|attachment| attachment.kind == attachment_kinds::FOLDER) {
        return Ok(None);
    }
    let session = host.session_get(session_id).await?;
    let project = host.project_get(&session.project_id).await?;
    let own: Vec<PathBuf> = [project.root.as_str(), session.workspace.as_str()].iter().filter(|root| !root.is_empty()).filter_map(|root| Path::new(root).canonicalize().ok()).collect();
    let listed = match host.ctx().call(memory_methods::PROJECT_LIST, json!({})).await {
        Ok(value) => serde_json::from_value::<ProjectListResult>(value).map(|list| list.projects).map_err(|error| error.to_string()),
        Err(error) => Err(error.message),
    };
    let (projects, list_error) = match listed {
        Ok(projects) => (projects, None),
        Err(error) => (Vec::new(), Some(error)),
    };
    let folders = repository_folders(attachments, &own, &projects);
    let (targets, mut not_opened) = targets(mentions, &folders, &projects, &project.id);
    if let Some(error) = list_error {
        for missing in &mut not_opened {
            missing.error = Some(error.clone());
        }
    }
    if targets.is_empty() && not_opened.is_empty() {
        return Ok(None);
    }
    let mut referenced = Vec::new();
    for target in &targets {
        match open(host, target).await {
            Ok(opened) => referenced.push(opened),
            Err(error) => {
                let asked = projects.iter().find(|p| p.project_id == target.id).map(|p| p.name.clone()).unwrap_or_else(|| target.root.clone());
                not_opened.push(NotOpened { asked, error: Some(error) });
            }
        }
    }
    let listed: Vec<Value> = referenced.iter().map(|p| json!({"id": p.id, "name": p.name, "root": p.root})).collect();
    Ok(Some(json!({"text": note(ds, &referenced, &not_opened), "purpose": NOTE_PURPOSE, "projects": listed, "host_message": true})))
}

/// Entry data for a message with its mentions (left out when there are none).
pub fn with(mut data: Value, mentions: &[Mention]) -> Value {
    if !mentions.is_empty()
        && let Some(object) = data.as_object_mut()
    {
        object.insert("mentions".into(), json!(mentions));
    }
    data
}

/// A steer's mentions carried onto the `user_message` it becomes (left out when there are none).
pub fn carry(mut data: Value, from: &Value) -> Value {
    if let Some(mentions) = from.get("mentions").filter(|value| value.as_array().is_some_and(|items| !items.is_empty()))
        && let Some(object) = data.as_object_mut()
    {
        object.insert("mentions".into(), mentions.clone());
    }
    data
}
