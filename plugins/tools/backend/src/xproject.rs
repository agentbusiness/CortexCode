//! Cross-project references (docs/IDEATION-WORKFLOW.md part 2): URIs and read targets qualified by
//! another project, answered by that project's code graph (the memory plugin, by project id) and by
//! the language servers run on that project's root.
//!
//! - `graph://<project>/search|callers|callees|impact|path/...`, `symbol://<project>/<name>`,
//!   `outline://<project>/<path>`;
//! - `definition://<project>:<path:line:col>` (and `references://`, `hover://`),
//!   `diagnostics://<project>:<path>` (`diagnostics://<project>:` for every file);
//! - the read target `<project>:<path>[:range]`.
//!
//! Memory's queries of a project wait for its index. A project that is still being indexed (a
//! reference opened in the background, or one not opened since memory started, which is then
//! opened in the background) is waited for at most `wait` (the `reference_wait_ms` config); a
//! graph, symbol or outline read that would wait longer answers at once with the progress
//! (`<name> is still being indexed: N of M files; try again shortly`).
//!
//! `<project>` is a known project's name or id (`memory.project.list`), or an absolute directory
//! (`graph:///Users/x/repo/search/...`, `definition:///Users/x/repo:src/a.rs:3:5`), opened as a
//! read-only reference when memory does not know it yet. Unqualified forms are untouched: a target
//! is only treated as qualified when its shape cannot be an unqualified one (a graph verb after the
//! qualifier, a position after `<project>:`) or when its first segment names a known project and
//! the unqualified reading names nothing in the workspace.

use crate::host::Host;
use crate::uris::{self, UriError};
use cc_protocol::contracts::memory::{ProjectInfo, ProjectListResult, ProjectStatus};
use serde_json::json;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const GRAPH_VERBS: &[&str] = &["search", "used-by", "uses", "callers", "callees", "impact", "path"];
const POSITION_SCHEMES: &[&str] = &["definition", "references", "hover"];

/// How often the status of a project being indexed is asked while waiting for it.
const INDEX_POLL: Duration = Duration::from_millis(100);

/// A project a qualifier names.
#[derive(Debug, Clone, PartialEq)]
pub struct Known {
    pub id: String,
    pub name: String,
    pub root: PathBuf,
    /// Memory's state of it when it was named (`indexing | ready | error | closed`).
    pub state: String,
}

fn error(kind: &'static str, message: impl Into<String>) -> UriError {
    UriError { kind, message: message.into() }
}

fn decode(text: &str) -> String {
    percent_encoding::percent_decode_str(text).decode_utf8_lossy().into_owned()
}

/// The projects memory knows (an error when memory cannot answer).
pub async fn projects(host: &dyn Host) -> Result<Vec<ProjectInfo>, String> {
    let value = host.call("memory.project.list", json!({})).await.map_err(|e| e.message)?;
    serde_json::from_value::<ProjectListResult>(value).map(|list| list.projects).map_err(|e| e.to_string())
}

fn known(info: &ProjectInfo) -> Known {
    Known { id: info.project_id.clone(), name: info.name.clone(), root: PathBuf::from(&info.root), state: info.state.clone() }
}

fn listing(projects: &[ProjectInfo]) -> String {
    if projects.is_empty() {
        return "none yet".to_owned();
    }
    projects.iter().map(|p| format!("{} ({})", p.name, p.root)).collect::<Vec<_>>().join(", ")
}

fn unknown(qualifier: &str, projects: &[ProjectInfo]) -> UriError {
    error(
        "not_found",
        format!("no project `{qualifier}`; known projects: {}. An absolute directory path opens another repository read-only.", listing(projects)),
    )
}

/// A known project by name or id.
fn by_name<'a>(projects: &'a [ProjectInfo], qualifier: &str) -> Option<&'a ProjectInfo> {
    projects.iter().find(|p| p.name == qualifier).or_else(|| projects.iter().find(|p| p.project_id == qualifier))
}

/// The project of an absolute directory: a known one with that root, else opened as a reference.
async fn by_root(host: &dyn Host, projects: &[ProjectInfo], dir: &Path) -> Result<Known, UriError> {
    let canonical = std::fs::canonicalize(dir).map_err(|e| error("not_found", format!("{}: {e}", dir.display())))?;
    if let Some(info) = projects.iter().find(|p| Path::new(&p.root) == canonical) {
        return Ok(known(info));
    }
    let value = host
        .call("memory.project.open", json!({"root": canonical.to_string_lossy(), "reference": true}))
        .await
        .map_err(|e| error("unavailable", format!("opening {} as a reference project: {}", canonical.display(), e.message)))?;
    let status: ProjectStatus = serde_json::from_value(value).map_err(|e| error("uri_error", format!("memory.project.open returned an unexpected shape: {e}")))?;
    let name = if status.name.is_empty() { status.project_id.clone() } else { status.name };
    Ok(Known { id: status.project_id, name, root: PathBuf::from(status.root), state: status.state })
}

/// The answer for a project whose index is still running.
pub fn progress_message(name: &str, status: Option<&ProjectStatus>) -> String {
    match status.filter(|s| s.total_files > 0) {
        Some(s) => format!(
            "{name} is still being indexed ({} of {} files). Its files can be read now: {name}:<path>.",
            s.indexed_files.min(s.total_files),
            s.total_files
        ),
        None => format!("{name} is still being indexed. Its files can be read now: {name}:<path>."),
    }
}

/// None when the project's code graph can answer now (or within `wait`); else the progress
/// message to answer with instead of a query that would wait for the index. A project memory has
/// not opened since it started (`closed`) is opened in the background first, so that its first
/// query does not open it in the foreground.
async fn still_indexing(host: &dyn Host, known: &Known, wait: Duration) -> Option<String> {
    if !matches!(known.state.as_str(), "indexing" | "closed") {
        return None;
    }
    let deadline = Instant::now() + wait;
    if known.state == "closed" {
        let open = json!({"root": known.root.to_string_lossy(), "project_id": known.id, "reference": true});
        match tokio::time::timeout(wait.max(INDEX_POLL), host.call("memory.project.open", open)).await {
            Ok(Ok(value)) if value.get("state").and_then(serde_json::Value::as_str) != Some("indexing") => return None,
            Ok(Ok(_)) => {}
            // Memory could not open it in the background: the query says what is wrong.
            Ok(Err(_)) => return None,
            Err(_) => return Some(progress_message(&known.name, None)),
        }
    }
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        let asked = tokio::time::timeout(left.max(INDEX_POLL), host.call("memory.project.status", json!({"project_id": known.id}))).await;
        let status: ProjectStatus = match asked {
            Ok(Ok(value)) => match serde_json::from_value(value) {
                Ok(status) => status,
                Err(_) => return None,
            },
            // Memory cannot say (an older memory, an error): the query answers for itself.
            Ok(Err(_)) => return None,
            // Its status waits for the index (a memory that answers it only when indexed).
            Err(_) => return Some(progress_message(&known.name, None)),
        };
        if status.state != "indexing" {
            return None;
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Some(progress_message(&known.name, Some(&status)));
        }
        tokio::time::sleep(left.min(INDEX_POLL)).await;
    }
}

/// Resolve a qualifier that must name a project (a name, an id or an absolute directory).
async fn resolve(host: &dyn Host, qualifier: &str) -> Result<Known, UriError> {
    let projects = projects(host).await.map_err(|message| error("unavailable", format!("the project list is not available right now ({message})")))?;
    if let Some(info) = by_name(&projects, qualifier) {
        return Ok(known(info));
    }
    if qualifier.starts_with('/') && Path::new(qualifier).is_dir() {
        return by_root(host, &projects, Path::new(qualifier)).await;
    }
    Err(unknown(qualifier, &projects))
}

/// `graph://` path `<q>/<verb>/...` -> (q, `<verb>/...`) when the second segment is a graph verb
/// and the first is not (the unqualified form starts with the verb).
pub fn graph_split(path: &str) -> Option<(String, String)> {
    let (first, rest) = path.split_once('/')?;
    let verb = rest.split('/').next().unwrap_or("");
    (!first.is_empty() && !GRAPH_VERBS.contains(&first) && GRAPH_VERBS.contains(&verb)).then(|| (decode(first), rest.to_owned()))
}

/// An absolute qualifier inside `path` (which starts with `/`): the longest directory prefix whose
/// remainder `accept`s, preferring a known project root, then a repository root (`.git`), then
/// (when `any_dir`) any directory.
pub fn absolute_split(path: &str, roots: &[PathBuf], any_dir: bool, accept: impl Fn(&Path, &str) -> bool) -> Option<(PathBuf, String)> {
    let segments: Vec<&str> = path.trim_start_matches('/').split('/').collect();
    let candidates: Vec<(PathBuf, String)> = (1..segments.len())
        .rev()
        .map(|i| (PathBuf::from(decode(&format!("/{}", segments[..i].join("/")))), segments[i..].join("/")))
        .filter(|(dir, rest)| !rest.is_empty() && dir.is_dir() && accept(dir, rest))
        .collect();
    let pick = |test: &dyn Fn(&Path) -> bool| candidates.iter().find(|(dir, _)| test(dir)).cloned();
    pick(&|dir| roots.iter().any(|root| root == dir)).or_else(|| pick(&|dir| dir.join(".git").exists())).or_else(|| if any_dir { candidates.first().cloned() } else { None })
}

/// `<q>:<path>:<line>[:<col>]` -> (q, `<path>:<line>[:<col>]`) when the remainder is a position
/// whose path is not a number (so `a.rs:12:5`, an unqualified position, is not split). q is a name
/// without `/`, or an absolute directory.
pub fn position_split(target: &str) -> Option<(String, String)> {
    let (qualifier, rest) = target.split_once(':')?;
    let (path, _, _) = crate::langs::position(rest)?;
    if path.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let qualifier = match qualifier.strip_prefix('/') {
        Some(absolute) => format!("/{}", absolute.trim_end_matches('/')),
        None => qualifier.to_owned(),
    };
    if qualifier.is_empty() || qualifier == "/" || (!qualifier.starts_with('/') && qualifier.contains('/')) {
        return None;
    }
    Some((qualifier, rest.to_owned()))
}

/// `diagnostics://` path `<q>:<path>` -> (q, `<path>`), `<q>:` -> (q, every file). q is a name
/// without `/`, or an absolute directory (an unqualified diagnostics path has no colon).
pub fn diagnostics_split(target: &str) -> Option<(String, String)> {
    let (qualifier, rest) = target.split_once(':')?;
    let qualifier = match qualifier.strip_prefix('/') {
        Some(absolute) => format!("/{}", absolute.trim_end_matches('/')),
        None => qualifier.to_owned(),
    };
    if qualifier.is_empty() || qualifier == "/" || (!qualifier.starts_with('/') && qualifier.contains('/')) || rest.contains(':') {
        return None;
    }
    Some((qualifier, rest.trim_matches('/').to_owned()))
}

/// `<q>:<path>[:range]` -> (q, `<path>[:range]`) for a read target: q has no `/` (or is an absolute
/// directory) and the remainder is a path, not only a range.
pub fn read_split(target: &str) -> Option<(String, String)> {
    let (qualifier, rest) = target.split_once(':')?;
    let (path, _) = crate::read::split_path_range(rest);
    let is_range = rest.bytes().all(|b| b.is_ascii_digit() || b"-+, ".contains(&b));
    let shaped = !qualifier.is_empty() && (qualifier.starts_with('/') || !qualifier.contains('/'));
    (shaped && !is_range && !path.is_empty()).then(|| (qualifier.trim_end_matches('/').to_owned(), rest.to_owned()))
}

/// Model-facing text of a qualified read: which project it came from, and the follow-up URIs the
/// renderers print, qualified (the first line and the symbol's used-by / uses lines).
fn label(known: &Known, text: String) -> String {
    let name = &known.name;
    let qualify = |line: &str| {
        line.replacen("graph://", &format!("graph://{name}/"), 1).replacen("symbol://", &format!("symbol://{name}/"), 1).replacen("outline://", &format!("outline://{name}/"), 1)
    };
    let mut out = format!("[project {name}, read-only]\n");
    for (index, line) in text.split_inclusive('\n').enumerate() {
        if index == 0 {
            out.push_str(&qualify(line));
        } else if line.starts_with("Used in ") || line.starts_with("Uses ") {
            out.push_str(&line.replacen("graph://", &format!("graph://{name}/"), 1));
        } else {
            out.push_str(line);
        }
    }
    out
}

/// Read a qualified URI; None when `uri` is not a qualified form (the caller reads it as today).
/// `wait`: how long a graph, symbol or outline read waits for a project still being indexed.
pub async fn read_uri(host: &dyn Host, workspace: &Path, session_project: &str, uri: &str, wait: Duration) -> Option<Result<String, UriError>> {
    let (scheme, path, _) = uris::split_uri(uri)?;
    let query = uri.split_once('?').map(|(_, query)| format!("?{query}")).unwrap_or_default();
    if POSITION_SCHEMES.contains(&scheme) {
        let decoded = decode(path);
        // A workspace file whose name has a colon stays a workspace file.
        if crate::langs::position(&decoded).is_some_and(|(path, _, _)| workspace.join(path).exists()) {
            return None;
        }
        let (qualifier, rest) = position_split(&decoded)?;
        return Some(match resolve(host, &qualifier).await {
            Ok(known) => crate::langs::read_uri(host, &known.root, scheme, &rest).await.map(|text| label(&known, text)).map_err(|message| error("unavailable", message)),
            Err(error) => Err(error),
        });
    }
    if scheme == "diagnostics" {
        let decoded = decode(path);
        if decoded.is_empty() || workspace.join(decoded.trim_matches('/')).exists() {
            return None;
        }
        let (qualifier, rest) = diagnostics_split(&decoded)?;
        return Some(match resolve(host, &qualifier).await {
            Ok(known) => crate::langs::read_uri(host, &known.root, scheme, &rest).await.map(|text| label(&known, text)).map_err(|message| error("unavailable", message)),
            Err(error) => Err(error),
        });
    }
    // (the project, the unqualified URI to read in it)
    let resolved: Result<(Known, String), UriError> = match scheme {
        "graph" if path.starts_with('/') => {
            let projects = projects(host).await.unwrap_or_default();
            let roots: Vec<PathBuf> = projects.iter().map(|p| PathBuf::from(&p.root)).collect();
            match absolute_split(path, &roots, true, |_, rest| GRAPH_VERBS.contains(&rest.split('/').next().unwrap_or(""))) {
                Some((dir, rest)) => by_root(host, &projects, &dir).await.map(|known| (known, format!("graph://{rest}{query}"))),
                None => Err(unknown(path, &projects)),
            }
        }
        "graph" => {
            let (qualifier, rest) = graph_split(path)?;
            resolve(host, &qualifier).await.map(|known| (known, format!("graph://{rest}{query}")))
        }
        "symbol" | "outline" => {
            let decoded = decode(path);
            if decoded.starts_with('/') {
                let file = Path::new(&decoded);
                if scheme == "outline" && (crate::util::is_inside(file, workspace) || !file.is_file()) {
                    return None;
                }
                let projects = projects(host).await.unwrap_or_default();
                let roots: Vec<PathBuf> = projects.iter().map(|p| PathBuf::from(&p.root)).collect();
                match absolute_split(&decoded, &roots, scheme == "symbol", |dir, rest| scheme == "symbol" || dir.join(rest).is_file()) {
                    Some((dir, rest)) => by_root(host, &projects, &dir).await.map(|known| (known, format!("{scheme}://{rest}"))),
                    None if scheme == "symbol" => Err(unknown(&decoded, &projects)),
                    None => return None,
                }
            } else {
                let (first, rest) = decoded.split_once('/')?;
                if rest.is_empty() || (scheme == "outline" && workspace.join(&decoded).exists()) {
                    return None;
                }
                let projects = projects(host).await.ok()?;
                let info = by_name(&projects, first)?;
                Ok((known(info), format!("{scheme}://{rest}")))
            }
        }
        _ => return None,
    };
    Some(match resolved {
        Ok((known, unqualified)) => {
            // The session's own project keeps its scope (the session's workspace, e.g. an agent's clone).
            let own = known.id == session_project;
            if !own && let Some(progress) = still_indexing(host, &known, wait).await {
                return Some(Ok(progress));
            }
            let scope = uris::Scope { project_id: &known.id, workspace: if own { workspace } else { &known.root } };
            uris::read(host, &scope, &unqualified).await.map(|text| if own { text } else { label(&known, text) })
        }
        Err(error) => Err(error),
    })
}

/// A read target `<project>:<path>[:range]`: the absolute path (with its range suffix) in that
/// project; None when `target` is not that form or names a workspace file (the caller reads it as
/// today). An unknown project is an error listing the known ones.
pub async fn read_target(host: &dyn Host, workspace: &Path, target: &str) -> Option<Result<String, UriError>> {
    if uris::is_uri(target) {
        return None;
    }
    let (qualifier, rest) = read_split(target)?;
    let (base, _) = crate::read::split_path_range(target);
    if crate::util::resolve(workspace, target).exists() || crate::util::resolve(workspace, &base).exists() {
        return None;
    }
    let projects = match projects(host).await {
        Ok(projects) => projects,
        Err(_) => return None,
    };
    let found = if qualifier.starts_with('/') {
        if !Path::new(&qualifier).is_dir() {
            return None;
        }
        by_root(host, &projects, Path::new(&qualifier)).await
    } else {
        match by_name(&projects, &qualifier) {
            Some(info) => Ok(known(info)),
            None => Err(unknown(&qualifier, &projects)),
        }
    };
    Some(found.map(|known| known.root.join(rest.trim_start_matches('/')).to_string_lossy().into_owned()))
}
