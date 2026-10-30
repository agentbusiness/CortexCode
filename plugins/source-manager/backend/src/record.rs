//! The live folder's record in the app's graph (`memory@1`, `docs/GRAPH-MODEL.md`). The folder
//! stays the materialised view plugins read as files (scripts and rules need real files); what
//! changed, when and every version's text are kept here:
//!
//! ```text
//! (:Plugin {key: id, name})-[:ON]->(:Machine)
//! (:DataFile {key: "<plugin>/<path>", plugin, path, shipped_hash,
//!             body*, hash*, origin*: shipped|edit|update|revert|restore})-[:OF]->(:Plugin)
//! (:LiveFolder {key: "live", recorded_commit, recorded_ms})      how far the record goes
//! stream `source.changes`   one record per change {at, commit, origin, summary, plugins, files, reverts}
//! ```
//!
//! `path` is the file's path in its plugin's folder (`data/skills/x.md`; in the live folder it is
//! `plugins/<plugin>/<path>`). `hash` is the content's git blob id, empty once the file is deleted
//! (the node is then retired); `body` is the text (at most [`MAX_BODY`] bytes), empty for a binary
//! file, which keeps only its hash. `shipped_hash` is the blob of the packages' data the folder
//! was last updated from (absent: a file the user added).
//!
//! Every change of the branch is recorded once, in order ([`sync`]): the files it changed are put
//! with their temporal values sampled at the change's time, then the stream record. A first start
//! replays the branch's first-parent history from the shipped root commit, so the graph holds
//! every version the folder ever had. Sample times only grow (a change in the same millisecond as
//! the one before is recorded a millisecond later), as IronGraph requires; the shipped root commit
//! (dated 1970) is recorded just before the first change after it.
//!
//! The git repository in the folder stays as the transport to GitHub (backup and sharing) and as
//! the three-way merge of releases; History and a file's restore read the graph.

use crate::live;
use crate::paths;
use crate::service::{Service, rpc};
use cc_protocol::contracts::memory::{GraphEdge, GraphLayer, GraphNode, GraphPutParams, GraphQueryResult, GraphRef, MachineInfo, methods};
use cc_protocol::contracts::source::{Change, FileHistory, FileHistoryParams, FileRestoreParams, FileRestoreResult, FileVersion};
use cc_protocol::RpcError;
use git2::{ObjectType, Oid, Repository, Tree};
use serde_json::{Map, Value, json};
use std::collections::{BTreeSet, HashMap};
use std::path::Path;
use std::sync::Arc;

/// The stream of changes, and how long it is kept (as long as the graph keeps temporal history).
pub const CHANGES: &str = "source.changes";
pub const CHANGES_DAYS: u32 = 3650;
/// The largest text kept as a body.
pub const MAX_BODY: usize = 1_000_000;
const DATA_FILE: &str = "DataFile";
const PLUGIN: &str = "Plugin";
const LIVE: (&str, &str) = ("LiveFolder", "live");
/// Files and body bytes per put.
const FILES_PER_PUT: usize = 200;
const BYTES_PER_PUT: usize = 8 * 1024 * 1024;
/// Times before this many days ago are older than the graph keeps history (the root commit).
const KEPT_DAYS: u64 = 3600;
const DAY_MS: u64 = 86_400_000;

/// How far the record goes, as the graph keeps it.
#[derive(Default)]
pub struct Mark {
    /// The last commit recorded; None before the first replay.
    commit: Option<String>,
    /// The time of the last sample written.
    ms: u64,
}

/// The recorder's state: one sync at a time.
#[derive(Default)]
pub struct Recorder {
    mark: tokio::sync::Mutex<Option<Mark>>,
    machine: std::sync::OnceLock<String>,
}

/// One file as a change left it.
#[derive(Debug, Clone)]
pub struct FileState {
    pub plugin: String,
    /// In the plugin's folder.
    pub path: String,
    /// The blob id; empty when deleted.
    pub hash: String,
    /// The text; empty for a binary or deleted file.
    pub body: String,
    pub shipped_hash: Option<String>,
}

/// One change of the branch, read from git.
#[derive(Debug, Clone)]
pub struct Pending {
    pub commit: String,
    pub ms: u64,
    pub origin: String,
    pub summary: String,
    pub plugins: Vec<String>,
    pub reverts: Option<String>,
    pub files: Vec<FileState>,
    /// Files whose shipped version moved while the folder's stayed: `shipped_hash` only.
    pub shipped: Vec<FileState>,
}

fn blob_of(tree: Option<&Tree>, file: &str) -> Option<Oid> {
    let entry = tree?.get_path(Path::new(file)).ok()?;
    (entry.kind() == Some(ObjectType::Blob)).then(|| entry.id())
}

/// The text of `bytes`, when it is text small enough to keep.
pub fn text_of(bytes: &[u8]) -> Option<&str> {
    if bytes.len() > MAX_BODY || bytes.contains(&0) {
        return None;
    }
    std::str::from_utf8(bytes).ok()
}

fn state_of(repo: &Repository, tree: &Tree, shipped: Option<&Tree>, file: &str) -> Result<Option<FileState>, String> {
    let Some(plugin) = paths::plugin_of(file) else { return Ok(None) };
    let path = file["plugins/".len() + plugin.len() + 1..].to_owned();
    let (hash, body) = match blob_of(Some(tree), file) {
        Some(oid) => {
            let blob = repo.find_blob(oid).map_err(live::err)?;
            (oid.to_string(), text_of(blob.content()).unwrap_or_default().to_owned())
        }
        None => (String::new(), String::new()),
    };
    let shipped_hash = blob_of(shipped, file).map(|oid| oid.to_string());
    Ok(Some(FileState { plugin: plugin.to_owned(), path, hash, body, shipped_hash }))
}

/// The branch's changes after `recorded`, oldest first: its first-parent line back to that
/// commit (from the root when None). When the branch no longer passes through it (a restore from
/// a backup starts a line of its own), the head is one change against the recorded tree.
pub fn pending(repo: &Repository, recorded: Option<&str>) -> Result<Vec<Pending>, String> {
    let head = repo.find_reference(live::MAIN).and_then(|r| r.peel_to_commit()).map_err(live::err)?;
    let recorded = recorded.and_then(|id| Oid::from_str(id).ok());
    if Some(head.id()) == recorded {
        return Ok(Vec::new());
    }
    let mut chain = Vec::new();
    let mut found = false;
    let mut commit = head.clone();
    loop {
        if Some(commit.id()) == recorded {
            found = true;
            break;
        }
        chain.push(commit.clone());
        match commit.parent(0) {
            Ok(parent) => commit = parent,
            Err(_) => break,
        }
    }
    let base = recorded.and_then(|oid| repo.find_commit(oid).ok());
    if recorded.is_some() && !found {
        chain = vec![head];
    }
    chain.reverse();
    let mut before: Option<Tree> = base.as_ref().map(|commit| commit.tree()).transpose().map_err(live::err)?;
    let mut shipped: Option<Tree> = base.as_ref().and_then(|commit| live::shipped_base(repo, commit.id()).ok()).and_then(|tree| repo.find_tree(tree).ok());
    let mut out = Vec::new();
    for commit in chain {
        let tree = commit.tree().map_err(live::err)?;
        let (summary, trailers) = live::parse_message(commit.message().unwrap_or_default());
        let root = commit.parent_count() == 0;
        let origin = if root { "shipped".to_owned() } else { trailers.get("Origin").cloned().unwrap_or_else(|| "edit".into()) };
        let now_shipped = match trailers.get("Shipped-Tree").and_then(|id| Oid::from_str(id).ok()).and_then(|id| repo.find_tree(id).ok()) {
            Some(tree) => Some(tree),
            None if root => Some(tree.clone()),
            None => shipped.clone(),
        };
        let changed = live::changed_files(repo, before.as_ref(), Some(&tree))?;
        let shipped_moved = match (&shipped, &now_shipped) {
            (Some(old), Some(new)) if old.id() == new.id() => Vec::new(),
            (old, new) => live::changed_files(repo, old.as_ref(), new.as_ref())?,
        };
        let mut files = Vec::new();
        for file in &changed {
            if let Some(state) = state_of(repo, &tree, now_shipped.as_ref(), file)? {
                files.push(state);
            }
        }
        let changed_set: BTreeSet<&String> = changed.iter().collect();
        let mut moved = Vec::new();
        for file in shipped_moved.iter().filter(|file| !changed_set.contains(file)) {
            if let Some(state) = state_of(repo, &tree, now_shipped.as_ref(), file)? {
                moved.push(state);
            }
        }
        let plugins = match trailers.get("Plugins") {
            Some(list) if !list.is_empty() => list.split(", ").filter(|s| !s.is_empty()).map(str::to_owned).collect(),
            _ => live::plugins_of(&changed),
        };
        out.push(Pending {
            commit: commit.id().to_string(),
            ms: u64::try_from(commit.time().seconds()).unwrap_or(0) * 1000,
            origin,
            summary,
            plugins,
            reverts: trailers.get("Reverts").cloned(),
            files,
            shipped: moved,
        });
        before = Some(tree);
        shipped = now_shipped;
    }
    Ok(out)
}

impl Recorder {
    async fn machine(&self, service: &Service) -> Result<String, RpcError> {
        if let Some(key) = self.machine.get() {
            return Ok(key.clone());
        }
        let info: MachineInfo = service.graph.ask(methods::GRAPH_MACHINE, json!({})).await?;
        let _ = self.machine.set(info.key.clone());
        Ok(info.key)
    }
}

fn node(label: &str, key: &str, props: Map<String, Value>, temporal: Map<String, Value>) -> GraphNode {
    GraphNode { label: label.into(), key: key.into(), props, temporal }
}

fn edge(from: GraphRef, rel: &str, to: GraphRef) -> GraphEdge {
    GraphEdge { from, rel: rel.into(), to, props: Map::new(), temporal: Map::new() }
}

fn at(label: &str, key: &str) -> GraphRef {
    GraphRef { label: label.into(), key: key.into() }
}

/// One put at `at_ms`, in the WORKSPACE layer.
async fn put(service: &Service, nodes: Vec<GraphNode>, edges: Vec<GraphEdge>, at_ms: u64) -> Result<(), RpcError> {
    let params = GraphPutParams { nodes, edges, layer: GraphLayer::Workspace, at_ms: Some(at_ms) };
    service.graph.ask::<Value>(methods::GRAPH_PUT, json!(params)).await.map(|_| ())
}

/// Put files (their nodes, their plugins and the edges) in batches.
async fn put_files(service: &Service, machine: &str, files: &[FileState], with_values: bool, at_ms: u64) -> Result<(), RpcError> {
    let mut start = 0;
    while start < files.len() {
        let mut end = start;
        let mut bytes = 0;
        while end < files.len() && end - start < FILES_PER_PUT && (end == start || bytes + files[end].body.len() <= BYTES_PER_PUT) {
            bytes += files[end].body.len();
            end += 1;
        }
        let mut nodes = Vec::new();
        let mut edges = Vec::new();
        let mut plugins = BTreeSet::new();
        for file in &files[start..end] {
            let key = format!("{}/{}", file.plugin, file.path);
            let mut props = Map::new();
            props.insert("plugin".into(), json!(file.plugin));
            props.insert("path".into(), json!(file.path));
            props.insert("shipped_hash".into(), json!(file.shipped_hash));
            nodes.push(node(DATA_FILE, &key, props, Map::new()));
            edges.push(edge(at(DATA_FILE, &key), "OF", at(PLUGIN, &file.plugin)));
            plugins.insert(file.plugin.clone());
        }
        if with_values {
            for (node, file) in nodes.iter_mut().zip(&files[start..end]) {
                node.temporal.insert("hash".into(), json!(file.hash));
                node.temporal.insert("body".into(), json!(file.body));
            }
        }
        for plugin in plugins {
            nodes.push(node(PLUGIN, &plugin, Map::from_iter([("name".to_owned(), json!(service.name_of(&plugin)))]), Map::new()));
            edges.push(edge(at(PLUGIN, &plugin), "ON", at("Machine", machine)));
        }
        put(service, nodes, edges, at_ms).await?;
        start = end;
    }
    Ok(())
}

/// Read the mark from the graph.
async fn load_mark(service: &Service) -> Result<Mark, RpcError> {
    let query = json!({"cypher": "MATCH (n:LiveFolder {key: 'live'}) RETURN n.recorded_commit AS commit, n.recorded_ms AS ms", "limit": 1});
    let result: GraphQueryResult = service.graph.ask(methods::GRAPH_QUERY, query).await?;
    let row = result.rows.into_iter().next().unwrap_or_default();
    Ok(Mark {
        commit: row.first().and_then(Value::as_str).map(str::to_owned).filter(|c| !c.is_empty()),
        ms: row.get(1).and_then(Value::as_u64).unwrap_or(0),
    })
}

/// Record the branch's changes the graph does not have yet. With `wait` false, returns at once
/// when a sync is running. Returns how many changes were recorded. A failure stops at that
/// change (nothing after it is written), so the next sync carries on from there.
pub async fn sync(service: &Arc<Service>, wait: bool) -> Result<usize, RpcError> {
    if !live::is_managed(&service.live_dir) {
        return Ok(0);
    }
    let mut guard = if wait {
        service.recorder.mark.lock().await
    } else {
        match service.recorder.mark.try_lock() {
            Ok(guard) => guard,
            Err(_) => return Ok(0),
        }
    };
    if guard.is_none() {
        *guard = Some(load_mark(service).await?);
    }
    let Some(mark) = guard.as_mut() else { return Ok(0) };
    let recorded = mark.commit.clone();
    let changes = service.git(move |repo| pending(repo, recorded.as_deref())).await?;
    if changes.is_empty() {
        return Ok(0);
    }
    let machine = service.recorder.machine(service).await?;
    let floor = cc_protocol::contracts::now_ms().saturating_sub(KEPT_DAYS * DAY_MS);
    let first = changes.len();
    for (index, change) in changes.iter().enumerate() {
        // The shipped root commit is dated 1970: it is recorded just before what follows it.
        let ms = if change.ms < floor { changes.get(index + 1).map(|next| next.ms).filter(|ms| *ms >= floor).unwrap_or_else(cc_protocol::contracts::now_ms).saturating_sub(1) } else { change.ms };
        let at_ms = ms.max(mark.ms + 1);
        let files = &change.files;
        // The values first, then the origin, so a file whose content is unchanged keeps its origin.
        put_files(service, &machine, files, true, at_ms).await?;
        let origins: Vec<GraphNode> = files
            .iter()
            .map(|file| node(DATA_FILE, &format!("{}/{}", file.plugin, file.path), Map::new(), Map::from_iter([("origin".to_owned(), json!(change.origin))])))
            .collect();
        for chunk in origins.chunks(1000) {
            put(service, chunk.to_vec(), Vec::new(), at_ms).await?;
        }
        put_files(service, &machine, &change.shipped, false, at_ms).await?;
        let gone: Vec<GraphRef> = files.iter().filter(|file| file.hash.is_empty()).map(|file| at(DATA_FILE, &format!("{}/{}", file.plugin, file.path))).collect();
        if !gone.is_empty() {
            service.graph.ask::<Value>(methods::GRAPH_RETIRE, json!({"refs": gone, "reason": "deleted", "at_ms": at_ms})).await?;
        }
        let shown: Vec<String> = files.iter().map(|file| format!("plugins/{}/{}", file.plugin, file.path)).collect();
        let record = json!({
            "at": at_ms, "commit": change.commit, "origin": change.origin, "summary": change.summary,
            "plugins": change.plugins, "files": shown, "reverts": change.reverts,
        });
        service.graph.ask::<Value>(methods::STREAM_APPEND, json!({"topic": CHANGES, "records": [record], "retention_days": CHANGES_DAYS})).await?;
        let props = Map::from_iter([("recorded_commit".to_owned(), json!(change.commit)), ("recorded_ms".to_owned(), json!(at_ms))]);
        put(service, vec![node(LIVE.0, LIVE.1, props, Map::new())], vec![edge(at(LIVE.0, LIVE.1), "ON", at("Machine", &machine))], at_ms).await?;
        mark.commit = Some(change.commit.clone());
        mark.ms = at_ms;
    }
    if first > 1 {
        tracing::info!(changes = first, "the live folder's history was recorded in the graph");
    }
    Ok(first)
}

/// `source.history` from the stream `source.changes`: the newest changes first, the shipped data
/// (the starting point) left out.
pub async fn history(service: &Arc<Service>, limit: usize) -> Result<Vec<Change>, RpcError> {
    if let Err(error) = sync(service, true).await {
        tracing::warn!(error = %error.message, "the newest changes could not be recorded before reading the history");
    }
    let entries = service.graph.newest(CHANGES, (limit as u64 + 1).min(5000)).await?;
    let mut reverted: HashMap<String, String> = HashMap::new();
    let mut changes = Vec::new();
    for entry in entries.into_iter().rev() {
        let value = entry.value;
        let text = |name: &str| value.get(name).and_then(Value::as_str).unwrap_or_default().to_owned();
        let list = |name: &str| value.get(name).and_then(Value::as_array).map(|items| items.iter().filter_map(Value::as_str).map(str::to_owned).collect()).unwrap_or_default();
        let (commit, origin) = (text("commit"), text("origin"));
        if let Some(target) = value.get("reverts").and_then(Value::as_str) {
            reverted.insert(target.to_owned(), commit.clone());
        }
        if origin == "shipped" {
            continue;
        }
        // An update lists no files of its own (they can be many).
        let files = if origin == "update" { Vec::new() } else { list("files") };
        changes.push(Change {
            reverted_by: reverted.get(&commit).cloned(),
            ms: value.get("at").and_then(Value::as_u64).unwrap_or(entry.at_ms),
            commit,
            origin,
            summary: text("summary"),
            plugins: list("plugins"),
            files,
            shared: None,
        });
        if changes.len() >= limit {
            break;
        }
    }
    Ok(changes)
}

/// The graph's key of a live-folder path (`plugins/<id>/<path>` -> `<id>/<path>`).
fn key_of(path: &str) -> Result<String, RpcError> {
    let clean = !path.starts_with('/') && !path.split('/').any(|part| part.is_empty() || part == "." || part == "..");
    match paths::plugin_of(path) {
        Some(_) if clean => Ok(path["plugins/".len()..].to_owned()),
        _ => Err(RpcError::invalid_params(format!("`{path}` is not a plugin's file in the live folder (plugins/<id>/...)"))),
    }
}

fn history_window() -> (u64, u64) {
    let now = cc_protocol::contracts::now_ms();
    (now.saturating_sub(KEPT_DAYS * DAY_MS), now + 60_000)
}

/// The samples of one temporal property of a file, oldest first: (ms, value).
async fn samples(service: &Service, key: &str, prop: &str) -> Result<Vec<(u64, String)>, RpcError> {
    let (from, to) = history_window();
    let cypher = format!(
        "MATCH (f:DataFile {{key: $key}}) HISTORY f.{prop} FROM datetime.fromepochmillis($from) TO datetime.fromepochmillis($to) AS s RETURN s.time / 1000000 AS at, s.value AS value ORDER BY at"
    );
    let result: GraphQueryResult = service.graph.ask(methods::GRAPH_QUERY, json!({"cypher": cypher, "params": {"key": key, "from": from, "to": to}, "limit": 5000})).await?;
    Ok(result.rows.into_iter().filter_map(|row| Some((row.first()?.as_u64()?, row.get(1)?.as_str()?.to_owned()))).collect())
}

/// `source.file.history`.
pub async fn file_history(service: &Arc<Service>, params: FileHistoryParams) -> Result<FileHistory, RpcError> {
    let key = key_of(&params.path)?;
    let _ = sync(service, true).await;
    let hashes = samples(service, &key, "hash").await?;
    let origins = samples(service, &key, "origin").await?;
    let mut versions: Vec<FileVersion> = hashes
        .into_iter()
        .map(|(at_ms, hash)| {
            let origin = origins.iter().rev().find(|(ms, _)| *ms <= at_ms).map(|(_, origin)| origin.clone()).unwrap_or_default();
            FileVersion { at_ms, hash, origin }
        })
        .collect();
    versions.reverse();
    versions.truncate(params.limit.unwrap_or(200).max(1));
    Ok(FileHistory { path: params.path, versions })
}

/// `source.file.restore`: the file as the graph had it at `at_ms`, written into the live folder
/// and committed as a `restore`; a file that did not exist then is deleted. The caller tells
/// listeners.
pub async fn file_restore(service: &Arc<Service>, params: FileRestoreParams) -> Result<FileRestoreResult, RpcError> {
    if !service.managed() || !live::is_managed(&service.live_dir) {
        return Err(RpcError::invalid_params("the live folder is a developer checkout: restore with git there"));
    }
    let key = key_of(&params.path)?;
    let cypher = "AT TIME datetime.fromepochmillis($at) MATCH (f:DataFile {key: $key}) RETURN f.hash AS hash, f.body AS body";
    let result: GraphQueryResult = service.graph.ask(methods::GRAPH_QUERY, json!({"cypher": cypher, "params": {"key": key, "at": params.at_ms}, "limit": 1})).await?;
    let row = result.rows.into_iter().next().unwrap_or_default();
    let hash = row.first().and_then(Value::as_str).map(str::to_owned);
    let body = row.get(1).and_then(Value::as_str).unwrap_or_default().to_owned();
    let Some(hash) = hash else {
        return Err(RpcError::not_found(format!("the graph has no version of {} at {}", params.path, params.at_ms)));
    };
    let path = params.path.clone();
    let plugin = paths::plugin_of(&path).unwrap_or_default().to_owned();
    let summary = format!("Restored {} in {}", path.rsplit('/').next().unwrap_or(&path), service.name_of(&plugin));
    let wanted = hash.clone();
    let (state, commit) = service
        .git(move |repo| {
            // The text the graph keeps; a binary file (or one too large) comes from git's objects.
            let bytes: Option<Vec<u8>> = if wanted.is_empty() {
                None
            } else if Oid::hash_object(ObjectType::Blob, body.as_bytes()).map(|oid| oid.to_string()).ok().as_deref() == Some(wanted.as_str()) {
                Some(body.into_bytes())
            } else {
                let oid = Oid::from_str(&wanted).map_err(live::err)?;
                Some(repo.find_blob(oid).map_err(|_| format!("{path} was binary at that time and its content is no longer in the live folder"))?.content().to_vec())
            };
            let work = repo.workdir().ok_or("the live folder has no work tree")?.join(&path);
            let current = std::fs::read(&work).ok();
            let head = repo.find_reference(live::MAIN).and_then(|r| r.peel_to_commit()).map_err(live::err)?;
            let committed = blob_of(Some(&head.tree().map_err(live::err)?), &path).map(|oid| oid.to_string()).unwrap_or_default();
            if current == bytes && committed == wanted {
                return Ok(("up_to_date", head.id().to_string()));
            }
            match &bytes {
                Some(bytes) => {
                    if let Some(parent) = work.parent() {
                        std::fs::create_dir_all(parent).map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
                    }
                    std::fs::write(&work, bytes).map_err(|e| format!("cannot write {}: {e}", work.display()))?;
                }
                None => {
                    let _ = std::fs::remove_file(&work);
                }
            }
            let text = live::message(&summary, &[("Origin", "restore".into()), ("Plugins", plugin)]);
            let oid = live::commit_files(repo, std::slice::from_ref(&path), &text)?;
            Ok((if bytes.is_some() { "restored" } else { "deleted" }, oid.to_string()))
        })
        .await?;
    if state != "up_to_date" {
        sync(service, true).await.map_err(|e| rpc(format!("restored, but not recorded yet: {}", e.message)))?;
    }
    Ok(FileRestoreResult { path: params.path, state: state.into(), hash, commit })
}
