//! One indexed source tree (a project root or a workspace clone) and its IronGraph project:
//! scanning, parallel reading, extraction by the language plugins, resolution, and batched graph
//! synchronisation.
//!
//! Every change (cold index, file save, rename, delete) goes through [`CodeTree::sync`]: the changed
//! files' subgraphs are replaced in a fixed number of batched statements, edges are replaced only
//! for files whose outgoing edge set changed, and nodes are rewritten only when their properties
//! changed. Stable ids (`<lang>:<path>#<qname>`) plus rename detection by body hash keep workspace
//! links (tasks, rules) and knowledge (descriptions) attached across edits.

use crate::db::{Graph, Rows, layer};
use crate::facts::{self, BINARY, FileFacts, Origin, TEXT, hash_hex};
use crate::langs::{Languages, Snapshot, Source};
use crate::linestore::LineStore;
use crate::summary;
use crate::model::{self, EdgeRow, Index, Model, NodeRow, label_of};
use crate::params;
use cc_protocol::RpcError;
use rayon::prelude::*;
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Instant;
use tokio::sync::{Mutex, RwLock};

/// Rows per write statement.
const BATCH_ROWS: usize = 4000;
/// Approximate parameter bytes per write statement.
const BATCH_BYTES: usize = 8 * 1024 * 1024;

#[derive(Debug, Clone, Default)]
pub struct TreeStatus {
    pub state: String,
    pub files: u64,
    pub symbols: u64,
    pub edges: u64,
    pub last_index_ms: u64,
    pub languages: BTreeSet<String>,
    /// Packages the code imports from outside the repository and the manifests' dependencies
    /// ([`model::Index::packages`]), as of the last sync.
    pub packages: BTreeSet<String>,
    pub error: Option<String>,
    pub externals_state: String,
    pub external_symbols: u64,
    /// Language -> (plugin category, plugin version, files) of the files a language plugin extracted.
    pub language_plugins: BTreeMap<String, (String, String, u64)>,
    /// (extension, text | binary) -> (files, an example path) of files no language plugin covers.
    pub uncovered: BTreeMap<(String, String), (u64, String)>,
    /// Files a language plugin could not extract now: (path, plugin, error, indexed as `text` | `kept`).
    pub problems: Vec<(String, String, String, String)>,
    /// What went wrong reaching the language plugins (listing, describe), and the ones not active.
    pub plugin_problems: Vec<String>,
}

/// What one sync did (also the `memory.indexed` payload source).
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct SyncReport {
    pub files_changed: usize,
    pub files_deleted: usize,
    pub nodes_written: usize,
    pub nodes_deleted: usize,
    pub renamed: usize,
    pub edge_files: usize,
    pub edges_written: usize,
    pub statements: usize,
    pub parse_ms: u64,
    pub resolve_ms: u64,
    pub write_ms: u64,
    pub total_ms: u64,
    pub changed_paths: Vec<String>,
    pub renames: Vec<(String, String)>,
}

pub struct CodeTree {
    pub root: PathBuf,
    /// IronGraph project name and UUID.
    pub ig_name: String,
    pub ig_id: String,
    pub graph: Arc<Graph>,
    pub model: RwLock<Model>,
    pub lines: LineStore,
    pub status: Mutex<TreeStatus>,
    /// Serialises indexing preparation and writes of this tree.
    sync_lock: Mutex<()>,
    /// A write that failed or was cancelled must be reconciled before content can be skipped.
    sync_pending: AtomicBool,
    /// Outgoing-edge hash per file as stored in the graph (`File.edge_hash`).
    edge_hashes: Mutex<HashMap<String, String>>,
    /// Directory structure hash as stored (`Repo.structure_hash`).
    structure_hash: Mutex<String>,
    indexes_created: Mutex<bool>,
    /// Properties hash of each shared node (dependency, environment variable) last written.
    shared_hashes: Mutex<HashMap<String, String>>,
    /// Edges written by the indexer (all files plus the directory structure).
    edge_total: Mutex<u64>,
    /// Last written symbol (sid, text) for the embedding readiness probe.
    pub last_written: Mutex<Vec<(String, String)>>,
    /// `summary_hash` stored per definition, by the hash of its sid (loaded on the first sync).
    summary_hashes: Mutex<Option<HashMap<u64, u64>>>,
    /// The definitions each file's words reach (call-site comments, tests, mentions), by path.
    summary_reach: Mutex<HashMap<String, Vec<String>>>,
    /// The language plugins.
    pub langs: Arc<Languages>,
    /// [`Snapshot::key`] of the language plugins every file was last matched against.
    coverage_key: Mutex<Option<String>>,
    /// Progress of the running (or last) cold index: the files it covers (0 until the tree is
    /// scanned) and the files whose facts are in.
    pub files_total: AtomicU64,
    pub files_done: Arc<AtomicU64>,
}

/// Files of a tree: everything the ignore rules keep but CORTEX.md (rules, never code facts).
/// Package-manager install directories (`node_modules`), virtualenvs (marked by `pyvenv.cfg`) and
/// build output (marked by `CACHEDIR.TAG`, which cargo writes into `target/`) are skipped:
/// structural markers.
pub fn scan(root: &Path) -> Vec<String> {
    let walker = ignore::WalkBuilder::new(root)
        .hidden(false)
        .git_ignore(true)
        .git_exclude(true)
        .git_global(true)
        .require_git(false)
        .parents(true)
        .filter_entry(|entry| {
            let name = entry.file_name().to_string_lossy();
            if entry.file_type().is_some_and(|t| t.is_dir()) {
                if name == ".git" || name == "node_modules" {
                    return false;
                }
                let path = entry.path();
                if path.join("CACHEDIR.TAG").is_file() || path.join("pyvenv.cfg").is_file() {
                    return false;
                }
            }
            true
        })
        .build_parallel();
    let (tx, rx) = std::sync::mpsc::channel();
    walker.run(|| {
        let tx = tx.clone();
        let root = root.to_path_buf();
        Box::new(move |entry| {
            if let Ok(entry) = entry
                && entry.file_type().is_some_and(|t| t.is_file())
                && let Ok(rel) = entry.path().strip_prefix(&root)
            {
                let rel = rel.to_string_lossy().replace('\\', "/");
                if indexed_name(&rel) {
                    let _ = tx.send(rel);
                }
            }
            ignore::WalkState::Continue
        })
    });
    drop(tx);
    let mut files: Vec<String> = rx.into_iter().collect();
    files.sort();
    files
}

/// Every file but CORTEX.md (the rules file) is indexed.
pub fn indexed_name(rel: &str) -> bool {
    rel.rsplit('/').next() != Some("CORTEX.md")
}

/// Text: no NUL byte and valid UTF-8 (a character cut off at the end of the sample is fine).
fn is_text(head: &[u8]) -> bool {
    !head.contains(&0) && std::str::from_utf8(head).map(|_| true).unwrap_or_else(|error| error.error_len().is_none())
}

/// Text files larger than this are recorded like binary ones (metadata and their leading text):
/// a data dump is not read in full on every index pass.
const MAX_TEXT_BYTES: u64 = 4 * 1024 * 1024;

/// What reading one file found.
pub enum Read {
    /// Complete facts (text or binary, no plugin covers the file; or a covered file that is not
    /// UTF-8) and the text for the line store.
    Done(FileFacts, String),
    /// A source for its language plugin.
    Source(Source),
}

/// Read one file and match it to its language plugin (structural rules): by name (exact name,
/// pattern, extension), else by the `#!` line of a text file; a file no plugin covers is text or
/// binary by its content.
pub fn read_one(root: &Path, rel: &str, snapshot: &Snapshot) -> Option<Read> {
    let abs = root.join(rel);
    let meta = std::fs::metadata(&abs).ok()?;
    let mut pick = snapshot.by_name(rel);
    if pick.is_none() {
        let mut head = [0u8; 8192];
        let read = std::fs::File::open(&abs).and_then(|mut f| std::io::Read::read(&mut f, &mut head)).ok()?;
        let head = &head[..read];
        let text = is_text(head);
        if text {
            pick = snapshot.by_shebang(&String::from_utf8_lossy(&head[..head.len().min(512)]));
        }
        if pick.is_none() && (!text || meta.len() > MAX_TEXT_BYTES) {
            // Identity by size and modification time: no full read.
            let modified = meta.modified().ok().and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok()).map(|d| d.as_nanos()).unwrap_or(0);
            let facts = FileFacts {
                path: rel.to_owned(),
                language: if text { TEXT } else { BINARY }.to_owned(),
                sid_prefix: if text { "txt" } else { "bin" }.to_owned(),
                content_hash: hash_hex(format!("{}:{modified}", meta.len()).as_bytes()),
                size: meta.len(),
                text: text.then(|| facts::excerpt(&String::from_utf8_lossy(head))),
                ..FileFacts::default()
            };
            return Some(Read::Done(facts, String::new()));
        }
    }
    let bytes = std::fs::read(&abs).ok()?;
    match String::from_utf8(bytes) {
        Ok(text) => {
            let facts = FileFacts::of_text(rel, &text);
            Some(match pick {
                Some(pick) => Read::Source(Source::new(facts, text, pick)),
                None => Read::Done(facts, text),
            })
        }
        Err(error) => {
            // A text file with a stray invalid byte past its sample is still indexed as text; a
            // source file is too, with the reason its plugin did not see it.
            let text = String::from_utf8_lossy(error.as_bytes()).into_owned();
            let mut facts = FileFacts::of_text(rel, &text);
            if let Some(pick) = pick {
                facts.origin = Origin::Failed { category: pick.plugin.category.clone(), error: "not valid UTF-8; indexed as text".into() };
            }
            Some(Read::Done(facts, text))
        }
    }
}

/// Read `rels` under `root` in parallel and extract the sources with their language plugins.
pub async fn parse(langs: &Languages, snapshot: &Arc<Snapshot>, root: &Path, rels: Vec<String>) -> Result<Vec<(FileFacts, String)>, RpcError> {
    parse_counted(langs, snapshot, root, rels, None).await
}

/// [`parse`], adding each file to `done` once its facts are in (read as text or binary, or
/// extracted by its plugin): the progress of a cold index.
pub async fn parse_counted(langs: &Languages, snapshot: &Arc<Snapshot>, root: &Path, rels: Vec<String>, done: Option<Arc<AtomicU64>>) -> Result<Vec<(FileFacts, String)>, RpcError> {
    let read_root = root.to_path_buf();
    let read_snapshot = snapshot.clone();
    let reads: Vec<Read> = tokio::task::spawn_blocking(move || rels.par_iter().filter_map(|rel| read_one(&read_root, rel, &read_snapshot)).collect())
        .await
        .map_err(|e| RpcError::internal(format!("read task failed: {e}")))?;
    let mut out = Vec::with_capacity(reads.len());
    let mut sources = Vec::new();
    for read in reads {
        match read {
            Read::Done(facts, text) => out.push((facts, text)),
            Read::Source(source) => sources.push(source),
        }
    }
    if let Some(done) = &done {
        done.fetch_add(out.len() as u64, Ordering::Relaxed);
    }
    out.extend(langs.extract_counted(snapshot, sources, Some(root), done).await);
    Ok(out)
}

/// What the graph holds of a file (`File` node).
struct StoredFile {
    content_hash: String,
    language: String,
    extractor: Option<String>,
    extractor_version: Option<String>,
}

impl StoredFile {
    /// Whether `facts` would write the same file (same content, extracted the same way).
    fn same_as(&self, facts: &FileFacts) -> bool {
        let (extractor, version) = match &facts.origin {
            Origin::Plugin { category, version } => (Some(category.as_str()), Some(version.as_str())),
            _ => (None, None),
        };
        self.content_hash == facts.content_hash && self.language == facts.language && self.extractor.as_deref() == extractor && self.extractor_version.as_deref() == version
    }
}

/// A file no plugin can extract now whose unchanged content the last index extracted keeps that
/// index's nodes ([`Origin::Kept`]): a plugin that is not active yet (or failed) must not cost the
/// file its definitions and what links to them (tasks, rules, descriptions).
fn keep_extracted(facts: FileFacts, stored: Option<&StoredFile>) -> FileFacts {
    let Some(stored) = stored else { return facts };
    if !facts.is_plain() || stored.language == TEXT || stored.language == BINARY || stored.content_hash != facts.content_hash {
        return facts;
    }
    let (plugin, reason) = match &facts.origin {
        Origin::Failed { category, error } => (category.clone(), format!("{error}; its {} nodes from the last index are kept", stored.language)),
        _ => (String::new(), format!("no active language plugin covers it now; its {} nodes from the last index are kept", stored.language)),
    };
    FileFacts {
        path: facts.path,
        language: stored.language.clone(),
        content_hash: facts.content_hash,
        size: facts.size,
        lines: facts.lines,
        // A parsed file keeps no leading text (code is read from the file).
        text: None,
        origin: Origin::Kept { plugin, reason },
        ..FileFacts::default()
    }
}

fn chunk_rows(rows: Vec<Value>) -> Vec<Vec<Value>> {
    let mut out = Vec::new();
    let mut current = Vec::new();
    let mut bytes = 0usize;
    for row in rows {
        let size = row.to_string().len();
        if !current.is_empty() && (current.len() >= BATCH_ROWS || bytes + size > BATCH_BYTES) {
            out.push(std::mem::take(&mut current));
            bytes = 0;
        }
        bytes += size;
        current.push(row);
    }
    if !current.is_empty() {
        out.push(current);
    }
    out
}

fn props_hash(props: &Map<String, Value>) -> String {
    hash_hex(serde_json::to_string(props).unwrap_or_default().as_bytes())
}

fn labels_for(kind: &str) -> String {
    let label = label_of(kind);
    if model::SYMBOL_KINDS.contains(&kind) { format!("Code:Sym:{label}") } else { format!("Code:{label}") }
}

/// Text a symbol is embedded with (used by the embedding-readiness probe): its name and doc.
fn probe_text(props: &Map<String, Value>) -> Option<String> {
    let name = props.get("name")?.as_str()?;
    let signature = props.get("signature").and_then(|s| s.as_str()).unwrap_or("");
    Some(format!("{name} {signature}"))
}

impl CodeTree {
    pub async fn open(root: PathBuf, ig_name: String, graph: Arc<Graph>, lines_dir: PathBuf, langs: Arc<Languages>) -> Result<Arc<Self>, RpcError> {
        let ig_id = graph.ensure_project(&ig_name).await?;
        let lines = LineStore::new(lines_dir);
        // This tree is not exposed yet: collect old sessions' entries without an indexing or
        // model lock. Ordinary saves never walk the line store.
        let rows = graph.run(&ig_id, &format!("{} MATCH (f:File) RETURN f.content_hash", layer::OBSERVED_READ), Map::new()).await?;
        let live: HashSet<String> = rows.column(0).filter_map(Value::as_str).map(str::to_owned).collect();
        let cleanup = lines.clone();
        match tokio::task::spawn_blocking(move || cleanup.retain(&live)).await {
            Ok(Ok(_)) => {}
            Ok(Err(error)) => tracing::warn!(%error, "line store cleanup failed"),
            Err(error) => tracing::warn!(%error, "line store cleanup task failed"),
        }
        Ok(Arc::new(Self {
            model: RwLock::new(Model::new(root.clone())),
            root,
            ig_name,
            ig_id,
            graph,
            lines,
            status: Mutex::new(TreeStatus { state: "indexing".into(), externals_state: "pending".into(), ..TreeStatus::default() }),
            sync_lock: Mutex::new(()),
            sync_pending: AtomicBool::new(false),
            edge_hashes: Mutex::new(HashMap::new()),
            structure_hash: Mutex::new(String::new()),
            indexes_created: Mutex::new(false),
            shared_hashes: Mutex::new(HashMap::new()),
            edge_total: Mutex::new(0),
            last_written: Mutex::new(Vec::new()),
            summary_hashes: Mutex::new(None),
            summary_reach: Mutex::new(HashMap::new()),
            langs,
            coverage_key: Mutex::new(None),
            files_total: AtomicU64::new(0),
            files_done: Arc::new(AtomicU64::new(0)),
        }))
    }

    pub async fn run(&self, cypher: &str, params: Map<String, Value>) -> Result<Rows, RpcError> {
        self.graph.run(&self.ig_id, cypher, params).await
    }

    /// Cold index: read every file, extract the sources with their language plugins and bring the
    /// graph in line with the tree (only differences are written when the graph already holds an
    /// earlier index of it).
    pub async fn full_index(&self) -> Result<SyncReport, RpcError> {
        let _guard = self.sync_lock.lock().await;
        self.full_index_owned().await
    }

    /// The existing sync gate owns preparation as well as persistence, so concurrent refreshes
    /// cannot compare against an older model and then publish out of order.
    async fn full_index_owned(&self) -> Result<SyncReport, RpcError> {
        let started = Instant::now();
        let snapshot = self.langs.snapshot().await;
        self.note_plugins(&snapshot).await;
        let root = self.root.clone();
        let rels = tokio::task::spawn_blocking(move || scan(&root)).await.map_err(|e| RpcError::internal(format!("scan task failed: {e}")))?;
        self.files_done.store(0, Ordering::Relaxed);
        self.files_total.store(rels.len() as u64, Ordering::Relaxed);
        let parsed = parse_counted(&self.langs, &snapshot, &self.root, rels, Some(self.files_done.clone())).await?;
        // Unreadable files drop out of the index: every file it covers is in now.
        self.files_total.store(parsed.len() as u64, Ordering::Relaxed);
        self.files_done.store(parsed.len() as u64, Ordering::Relaxed);
        let parse_ms = started.elapsed().as_millis() as u64;
        // What the graph already holds.
        let existing = self
            .run(&format!("{} MATCH (f:File) RETURN f.path, f.content_hash, f.language, f.extractor, f.extractor_version", layer::OBSERVED_READ), Map::new())
            .await?;
        let mut stored: HashMap<String, StoredFile> = HashMap::new();
        for row in &existing.rows {
            let text = |i: usize| row.get(i).and_then(|v| v.as_str()).map(str::to_owned);
            if let (Some(path), Some(content_hash)) = (text(0), text(1)) {
                stored.insert(path, StoredFile { content_hash, language: text(2).unwrap_or_default(), extractor: text(3), extractor_version: text(4) });
            }
        }
        // Rebuild outgoing edges on reconciliation/cold passes, including a previous process
        // that stopped after writing File metadata but before writing its edges.
        let repairing = self.sync_pending.load(Ordering::Acquire);
        self.invalidate_write_caches().await;
        self.sync_pending.store(true, Ordering::Release);
        let on_disk: HashSet<String> = parsed.iter().map(|(f, _)| f.path.clone()).collect();
        let mut model = self.model.read().await.clone();
        let deleted: Vec<String> = stored.keys().chain(model.files.keys()).filter(|p| !on_disk.contains(*p)).cloned().collect::<BTreeSet<_>>().into_iter().collect();
        let mut changed: Vec<(FileFacts, String)> = Vec::new();
        {
            model.files.clear();
            for (facts, text) in parsed {
                let previous = stored.get(&facts.path);
                let facts = keep_extracted(facts, previous);
                let unchanged = matches!(facts.origin, Origin::Kept { .. }) || (!repairing && previous.is_some_and(|p| p.same_as(&facts)));
                // The line store may have been cleaned independently; keep it complete.
                if unchanged {
                    self.lines.put(&facts.content_hash, &text).map_err(|e| RpcError::internal(format!("line store: {e}")))?;
                    model.files.insert(facts.path.clone(), Arc::new(facts));
                } else {
                    changed.push((facts, text));
                }
            }
        }
        (model.commits, model.history) = self.read_history().await;
        let mut report = self.sync_prepared(changed, deleted, model).await?;
        if let Err(error) = self.drop_code().await {
            tracing::warn!(%error, "removing stored definition code failed");
        }
        if let Err(error) = self.move_descriptions().await {
            tracing::warn!(%error, "moving descriptions onto their symbols failed");
        }
        *self.coverage_key.lock().await = Some(snapshot.key.clone());
        report.parse_ms += parse_ms;
        report.total_ms = started.elapsed().as_millis() as u64;
        Ok(report)
    }

    /// Re-index specific paths (repository-relative): re-read those that exist, delete the rest.
    /// When the language plugins changed since the files were matched, the files whose plugin
    /// changed come along ([`CodeTree::recover`]).
    pub async fn refresh_paths(&self, paths: &[String]) -> Result<SyncReport, RpcError> {
        let _guard = self.sync_lock.lock().await;
        if self.sync_pending.load(Ordering::Acquire) || paths.iter().any(|path| crate::watch::changes_membership(&self.root, &self.root.join(path))) {
            return self.full_index_owned().await;
        }
        let snapshot = self.langs.snapshot().await;
        let coverage = if self.coverage_key.lock().await.as_deref() == Some(snapshot.key.as_str()) { Vec::new() } else { self.coverage_changes(&snapshot).await };
        self.reindex(&snapshot, paths, coverage).await
    }

    /// Bring every file in line with the language plugins active now: files that had no plugin
    /// (text, failed, kept) and one covers now, and files whose plugin is another one or another
    /// version, are extracted again. Nothing to do when the plugins did not change.
    pub async fn recover(&self) -> Result<SyncReport, RpcError> {
        let _guard = self.sync_lock.lock().await;
        if self.sync_pending.load(Ordering::Acquire) {
            return self.full_index_owned().await;
        }
        let snapshot = self.langs.snapshot().await;
        if self.coverage_key.lock().await.as_deref() == Some(snapshot.key.as_str()) {
            return Ok(SyncReport::default());
        }
        let coverage = self.coverage_changes(&snapshot).await;
        self.reindex(&snapshot, &[], coverage).await
    }

    /// Indexed files the language plugins of `snapshot` would extract differently.
    async fn coverage_changes(&self, snapshot: &Arc<Snapshot>) -> Vec<String> {
        let files: Vec<(String, Origin)> = self.model.read().await.files.values().map(|f| (f.path.clone(), f.origin.clone())).collect();
        let root = self.root.clone();
        let snapshot = snapshot.clone();
        tokio::task::spawn_blocking(move || {
            files
                .into_par_iter()
                .filter(|(path, origin)| {
                    let head = || {
                        let mut head = [0u8; 512];
                        let read = std::fs::File::open(root.join(path)).and_then(|mut f| std::io::Read::read(&mut f, &mut head)).unwrap_or(0);
                        String::from_utf8_lossy(&head[..read]).into_owned()
                    };
                    let pick = match snapshot.by_name(path) {
                        Some(pick) => Some(pick),
                        None if snapshot.has_shebangs() => snapshot.by_shebang(&head()),
                        None => None,
                    };
                    match (origin, pick) {
                        (Origin::Plugin { category, version }, Some(pick)) => pick.plugin.category != *category || pick.plugin.version != *version,
                        // A plugin that is gone keeps its files' facts (content unchanged).
                        (Origin::Plugin { .. }, None) => false,
                        (_, pick) => pick.is_some(),
                    }
                })
                .map(|(path, _)| path)
                .collect()
        })
        .await
        .unwrap_or_default()
    }

    /// Re-read `paths` (content changes) and `coverage` (files to extract again although their
    /// content did not change), delete the paths that are gone.
    async fn reindex(&self, snapshot: &Arc<Snapshot>, paths: &[String], coverage: Vec<String>) -> Result<SyncReport, RpcError> {
        let started = Instant::now();
        self.note_plugins(snapshot).await;
        let forced: HashSet<String> = coverage.iter().cloned().collect();
        let mut wanted: Vec<String> = paths.to_vec();
        wanted.extend(coverage);
        wanted.sort();
        wanted.dedup();
        let root = self.root.clone();
        let (present, deleted): (Vec<String>, Vec<String>) = tokio::task::spawn_blocking(move || {
            let mut present = Vec::new();
            let mut deleted = Vec::new();
            for rel in wanted {
                if root.join(&rel).is_file() && indexed_name(&rel) && included(&root, &rel) {
                    present.push(rel);
                } else {
                    deleted.push(rel);
                }
            }
            (present, deleted)
        })
        .await
        .map_err(|e| RpcError::internal(format!("read task failed: {e}")))?;
        let parsed = parse(&self.langs, snapshot, &self.root, present).await?;
        // Unchanged content: nothing to do for that file, unless its plugin changed. A plugin that
        // cannot extract unchanged content now never replaces what an earlier extraction found.
        let (changed, deleted) = {
            let model = self.model.read().await;
            let changed: Vec<(FileFacts, String)> = parsed
                .into_iter()
                .filter(|(new, _)| match model.files.get(&new.path) {
                    None => true,
                    Some(old) if old.content_hash != new.content_hash => true,
                    Some(old) if new.is_plain() && !old.is_plain() => false,
                    Some(old) => forced.contains(&new.path) && (old.language != new.language || old.origin != new.origin),
                })
                .collect();
            let deleted: Vec<String> = deleted.into_iter().filter(|p| model.files.contains_key(p)).collect();
            (changed, deleted)
        };
        if changed.is_empty() && deleted.is_empty() {
            *self.coverage_key.lock().await = Some(snapshot.key.clone());
            return Ok(SyncReport { total_ms: started.elapsed().as_millis() as u64, ..SyncReport::default() });
        }
        let parse_ms = started.elapsed().as_millis() as u64;
        let model = self.model.read().await.clone();
        let mut report = self.sync_prepared(changed, deleted, model).await?;
        // Coverage advances only after the corresponding facts reach the graph.
        *self.coverage_key.lock().await = Some(snapshot.key.clone());
        report.parse_ms = parse_ms;
        report.total_ms = started.elapsed().as_millis() as u64;
        Ok(report)
    }

    /// Apply parsed changes and deletions to the model and the graph.
    pub async fn sync(&self, changed: Vec<(FileFacts, String)>, deleted: Vec<String>) -> Result<SyncReport, RpcError> {
        let _guard = self.sync_lock.lock().await;
        if self.sync_pending.load(Ordering::Acquire) {
            return self.full_index_owned().await;
        }
        let model = self.model.read().await.clone();
        self.sync_prepared(changed, deleted, model).await
    }

    async fn invalidate_write_caches(&self) {
        self.edge_hashes.lock().await.clear();
        self.shared_hashes.lock().await.clear();
        self.structure_hash.lock().await.clear();
        *self.summary_hashes.lock().await = None;
        self.summary_reach.lock().await.clear();
        *self.coverage_key.lock().await = None;
    }

    async fn sync_prepared(&self, changed: Vec<(FileFacts, String)>, deleted: Vec<String>, model: Model) -> Result<SyncReport, RpcError> {
        // Leave this set on error or cancellation: the next refresh reconciles all persisted work.
        self.sync_pending.store(true, Ordering::Release);
        let result = self.write_sync(changed, deleted, model).await;
        if result.is_ok() {
            self.sync_pending.store(false, Ordering::Release);
        } else {
            self.invalidate_write_caches().await;
        }
        result
    }

    async fn write_sync(&self, changed: Vec<(FileFacts, String)>, deleted: Vec<String>, mut model: Model) -> Result<SyncReport, RpcError> {
        let started = Instant::now();
        let mut report = SyncReport { files_changed: changed.len(), files_deleted: deleted.len(), ..SyncReport::default() };
        report.changed_paths = changed.iter().map(|(f, _)| f.path.clone()).chain(deleted.iter().cloned()).collect();
        for (facts, text) in &changed {
            self.lines.put(&facts.content_hash, text).map_err(|e| RpcError::internal(format!("line store: {e}")))?;
        }
        let changed_paths: Vec<String> = changed.iter().map(|(f, _)| f.path.clone()).collect();
        // 1. Model update and full re-resolution (cheap, parallel); only differing edge sets are written.
        let resolve_started = Instant::now();
        let (new_nodes, shared_nodes, all_edges, structure, kept, packages) = {
            for path in &deleted {
                model.files.remove(path);
            }
            for (facts, _) in changed {
                model.files.insert(facts.path.clone(), Arc::new(facts));
            }
            let model = &model;
            let index = Index::build(model);
            let all_edges: Vec<(String, Vec<EdgeRow>, String)> = model
                .files
                .par_iter()
                .map(|(path, facts)| {
                    let edges = model::file_edges(&index, facts);
                    let hash = edge_hash(&edges);
                    (path.clone(), edges, hash)
                })
                .collect();
            let mut new_nodes: Vec<NodeRow> = Vec::new();
            for path in &changed_paths {
                if let Some(facts) = model.files.get(path) {
                    new_nodes.extend(model::file_nodes(facts));
                }
            }
            let mut shared: BTreeMap<String, NodeRow> = BTreeMap::new();
            for facts in model.files.values() {
                for row in model::shared_nodes(facts) {
                    shared.insert(row.sid.clone(), row);
                }
            }
            for row in model::commit_nodes(model) {
                shared.insert(row.sid.clone(), row);
            }
            let structure = model::directory_nodes(model);
            // Kept files' nodes and edges stay as the last index wrote them.
            let kept: HashSet<String> = model.files.values().filter(|f| matches!(f.origin, Origin::Kept { .. })).map(|f| f.path.clone()).collect();
            (new_nodes, shared.into_values().collect::<Vec<_>>(), all_edges, structure, kept, index.packages())
        };
        report.resolve_ms = resolve_started.elapsed().as_millis() as u64;
        let write_started = Instant::now();
        let mut statements = 0usize;

        // 2. Old nodes of changed and deleted files.
        let mut touched: Vec<String> = changed_paths.clone();
        touched.extend(deleted.iter().cloned());
        let mut old: HashMap<String, (String, String, String, String)> = HashMap::new(); // sid -> (kind, body_hash, props_hash, path)
        for chunk in touched.chunks(BATCH_ROWS) {
            let rows = self
                .run(
                    &format!("{} UNWIND $paths AS p MATCH (n:Code {{path: p}}) WHERE n.kind <> 'directory' AND n.kind <> 'repo' RETURN n.sid, n.kind, n.body_hash, n.props_hash, p", layer::OBSERVED_READ),
                    params! { "paths" => chunk },
                )
                .await?;
            statements += 1;
            for row in rows.rows {
                let s = |i: usize| row.get(i).and_then(|v| v.as_str()).unwrap_or("").to_owned();
                old.insert(s(0), (s(1), s(2), s(3), s(4)));
            }
        }
        let new_by_sid: HashMap<&str, &NodeRow> = new_nodes.iter().map(|n| (n.sid.as_str(), n)).collect();
        // Removed: gone, or kind changed (labels differ).
        let mut removed: Vec<String> = old.iter().filter(|(sid, (kind, ..))| new_by_sid.get(sid.as_str()).is_none_or(|n| &n.kind != kind)).map(|(sid, _)| sid.clone()).collect();
        let added: Vec<&NodeRow> = new_nodes.iter().filter(|n| old.get(&n.sid).is_none_or(|(kind, ..)| *kind != n.kind)).collect();
        // 3. Rename detection: a removed and an added definition of the same kind with the same
        // (non-empty) body hash, each unique in this change set.
        let mut renames: Vec<(String, String)> = Vec::new();
        {
            let mut by_hash_removed: HashMap<(String, String), Vec<String>> = HashMap::new();
            for sid in &removed {
                let (kind, body, _, _) = &old[sid];
                if !body.is_empty() && kind != "param" && kind != "file" {
                    by_hash_removed.entry((kind.clone(), body.clone())).or_default().push(sid.clone());
                }
            }
            let mut by_hash_added: HashMap<(String, String), Vec<&NodeRow>> = HashMap::new();
            for node in &added {
                if let Some(body) = node.props.get("body_hash").and_then(|b| b.as_str())
                    && !body.is_empty()
                {
                    by_hash_added.entry((node.kind.clone(), body.to_owned())).or_default().push(node);
                }
            }
            for (key, olds) in &by_hash_removed {
                if let Some(news) = by_hash_added.get(key)
                    && olds.len() == 1
                    && news.len() == 1
                {
                    renames.push((olds[0].clone(), news[0].sid.clone()));
                }
            }
            let renamed_old: HashSet<&String> = renames.iter().map(|(o, _)| o).collect();
            removed.retain(|sid| !renamed_old.contains(sid));
        }
        report.renamed = renames.len();
        report.renames = renames.clone();
        let renamed_new: HashSet<&str> = renames.iter().map(|(_, n)| n.as_str()).collect();

        // 4. Renames keep node identity (and so every workspace / knowledge link to the node).
        if !renames.is_empty() {
            let rows: Vec<Value> = renames.iter().map(|(o, n)| json!({"old": o, "new": n})).collect();
            for chunk in chunk_rows(rows) {
                self.run(&format!("{} UNWIND $rows AS r MATCH (n:Code {{sid: r.old}}) SET n.sid = r.new", layer::OBSERVED_WRITE), params! { "rows" => chunk }).await?;
                statements += 1;
            }
        }
        // 5. Delete removed nodes (and files that are gone).
        if !removed.is_empty() {
            for chunk in removed.chunks(BATCH_ROWS) {
                self.run(&format!("{} UNWIND $sids AS s MATCH (n:Code {{sid: s}}) DETACH DELETE n", layer::OBSERVED_WRITE), params! { "sids" => chunk }).await?;
                statements += 1;
            }
        }
        report.nodes_deleted = removed.len();

        // 6. Which files need their outgoing edges replaced.
        let stored_hashes = self.edge_hashes.lock().await.clone();
        let changed_set: HashSet<&str> = changed_paths.iter().map(String::as_str).collect();
        let dirty: Vec<&(String, Vec<EdgeRow>, String)> =
            all_edges.iter().filter(|(path, _, hash)| !kept.contains(path) && (changed_set.contains(path.as_str()) || stored_hashes.get(path) != Some(hash))).collect();
        report.edge_files = dirty.len();
        // Source sids of dirty files that already exist in the graph (new nodes have no edges yet).
        let mut dirty_sources: Vec<String> = Vec::new();
        {
            for (path, _, _) in &dirty {
                if let Some(facts) = model.files.get(path) {
                    for row in model::file_nodes(facts) {
                        let exists = if changed_set.contains(path.as_str()) { old.contains_key(&row.sid) || renamed_new.contains(row.sid.as_str()) } else { true };
                        if exists {
                            dirty_sources.push(row.sid);
                        }
                    }
                }
            }
        }
        for chunk in dirty_sources.chunks(BATCH_ROWS) {
            self.run(&format!("{} UNWIND $sids AS s MATCH (a:Code {{sid: s}})-[r]->() DELETE r", layer::OBSERVED_WRITE), params! { "sids" => chunk }).await?;
            statements += 1;
        }

        // 6b. Summaries (src/summary.rs), recomposed only for definitions whose words may have
        // changed: those of changed files, and those the words of re-resolved files reach (now or
        // before). Written only when the text differs from the stored one.
        let mut pending = self.summaries(&model, &all_edges, &changed_paths, &dirty, &deleted).await?;
        let mut summary_updates: Vec<(u64, u64)> = Vec::with_capacity(pending.len());

        // 7. Upsert nodes whose properties changed. Symbols first: IronGraph embeds in write order,
        // so the searchable definitions become searchable before parameters and files.
        let edge_hash_of: HashMap<&str, &str> = all_edges.iter().map(|(p, _, h)| (p.as_str(), h.as_str())).collect();
        let mut to_write: Vec<NodeRow> = Vec::new();
        for node in &new_nodes {
            let mut props = node.props.clone();
            if node.kind == "file" {
                props.insert("edge_hash".into(), json!(edge_hash_of.get(node.file.as_str()).copied().unwrap_or("")));
            }
            let hash = props_hash(&props);
            let unchanged = old.get(&node.sid).is_some_and(|(kind, _, ph, _)| *kind == node.kind && *ph == hash) && !renamed_new.contains(node.sid.as_str());
            if !unchanged {
                props.insert("props_hash".into(), json!(hash));
                // A summary rides along with its node's write (one embedding, not two).
                if let Some((text, summary_hash)) = pending.remove(&node.sid) {
                    props.insert("summary".into(), json!(text));
                    props.insert("summary_hash".into(), summary_hash_value(summary_hash));
                    summary_updates.push((summary::hash64(&node.sid), summary_hash));
                }
                to_write.push(NodeRow { sid: node.sid.clone(), kind: node.kind.clone(), file: node.file.clone(), props });
            }
        }
        // Files whose only change is their outgoing edges: update the stored edge hash.
        let mut edge_hash_updates: Vec<Value> = Vec::new();
        for (path, _, hash) in &dirty {
            if !changed_set.contains(path.as_str()) {
                edge_hash_updates.push(json!({"sid": model::file_sid(path), "hash": hash}));
            }
        }
        to_write.sort_by_key(|n| match n.kind.as_str() {
            "param" => 2,
            "file" => 3,
            _ => 1,
        });
        report.nodes_written = to_write.len();
        statements += self.write_nodes(&to_write).await?;
        // Summaries of definitions whose node is not written otherwise.
        if !pending.is_empty() {
            let rows: Vec<Value> = pending
                .into_iter()
                .map(|(sid, (text, summary_hash))| {
                    summary_updates.push((summary::hash64(&sid), summary_hash));
                    json!({"sid": sid, "s": text, "h": summary_hash_value(summary_hash)})
                })
                .collect();
            for chunk in chunk_rows(rows) {
                self.run(&format!("{} UNWIND $rows AS r MATCH (n:Sym {{sid: r.sid}}) SET n.summary = r.s, n.summary_hash = r.h", layer::OBSERVED_WRITE), params! { "rows" => chunk }).await?;
                statements += 1;
            }
        }
        if let Some(stored) = self.summary_hashes.lock().await.as_mut() {
            for sid in removed.iter().chain(renames.iter().map(|(old_sid, _)| old_sid)) {
                stored.remove(&summary::hash64(sid));
            }
            for (key, summary_hash) in summary_updates {
                if summary_hash == 0 {
                    stored.remove(&key);
                } else {
                    stored.insert(key, summary_hash);
                }
            }
        }
        // Embedding probes: the last symbol of every label written (one statement group each).
        {
            let mut probes = self.last_written.lock().await;
            let mut seen_labels: HashSet<&str> = HashSet::new();
            for node in to_write.iter().rev() {
                if model::SYMBOL_KINDS.contains(&node.kind.as_str()) && seen_labels.insert(node.kind.as_str())
                    && let Some(text) = probe_text(&node.props)
                {
                    probes.retain(|(sid, _)| *sid != node.sid);
                    probes.push((node.sid.clone(), text));
                }
            }
        }
        if !edge_hash_updates.is_empty() {
            for chunk in chunk_rows(edge_hash_updates) {
                self.run(&format!("{} UNWIND $rows AS r MATCH (f:File {{sid: r.sid}}) SET f.edge_hash = r.hash", layer::OBSERVED_WRITE), params! { "rows" => chunk }).await?;
                statements += 1;
            }
        }
        // Shared nodes: dependencies and environment variables (merged; orphans removed below).
        {
            // Only shared nodes whose properties changed since they were last written.
            let mut written = self.shared_hashes.lock().await;
            let changed: Vec<NodeRow> = shared_nodes.iter().filter(|n| written.get(&n.sid) != Some(&props_hash(&n.props))).cloned().collect();
            statements += self.write_nodes(&changed).await?;
            for node in &changed {
                written.insert(node.sid.clone(), props_hash(&node.props));
            }
        }

        // 8. Directory structure (replaced when the set of files changed).
        let (dir_nodes, dir_edges) = structure;
        let dir_edge_count = dir_edges.len() as u64;
        let structure_key = edge_hash(&dir_edges);
        let structure_changed = *self.structure_hash.lock().await != structure_key;
        if structure_changed {
            let mut repo_nodes = dir_nodes.clone();
            for node in &mut repo_nodes {
                if node.kind == "repo" {
                    node.props.insert("structure_hash".into(), json!(structure_key));
                    node.props.insert("root".into(), json!(self.root.to_string_lossy()));
                }
            }
            statements += self.write_nodes(&repo_nodes).await?;
            self.run(&format!("{} MATCH (d:Code)-[r:CONTAINS]->() WHERE d:Directory OR d:Repo DELETE r", layer::OBSERVED_WRITE), Map::new()).await?;
            statements += 1;
            let live: Vec<&str> = dir_nodes.iter().map(|n| n.sid.as_str()).collect();
            let stale = self.run(&format!("{} MATCH (d:Directory) RETURN d.sid", layer::OBSERVED_READ), Map::new()).await?;
            let stale: Vec<String> = stale.column(0).filter_map(|v| v.as_str()).filter(|s| !live.contains(s)).map(str::to_owned).collect();
            if !stale.is_empty() {
                self.run(&format!("{} UNWIND $sids AS s MATCH (n:Code {{sid: s}}) DETACH DELETE n", layer::OBSERVED_WRITE), params! { "sids" => stale }).await?;
                statements += 1;
            }
        }

        // 9. Edges of dirty files (and the directory structure when it changed).
        let mut edges: Vec<&EdgeRow> = dirty.iter().flat_map(|(_, e, _)| e.iter()).collect();
        if structure_changed {
            edges.extend(dir_edges.iter());
        }
        report.edges_written = edges.len();
        statements += self.write_edges(&edges).await?;
        if structure_changed {
            *self.structure_hash.lock().await = structure_key;
        }

        // 10. Orphaned shared nodes.
        let live_shared: HashSet<&str> = shared_nodes.iter().map(|n| n.sid.as_str()).collect();
        let existing_shared = self.run(&format!("{} MATCH (n:Code) WHERE n:EnvVar OR n:Dependency RETURN n.sid", layer::OBSERVED_READ), Map::new()).await?;
        statements += 1;
        let orphans: Vec<String> = existing_shared.column(0).filter_map(|v| v.as_str()).filter(|s| !live_shared.contains(s)).map(str::to_owned).collect();
        if !orphans.is_empty() {
            self.run(&format!("{} UNWIND $sids AS s MATCH (n:Code {{sid: s}})-[:EXPORTS]->(x:ExternalSymbol) DETACH DELETE x", layer::OBSERVED_WRITE), params! { "sids" => orphans.clone() }).await?;
            self.run(&format!("{} UNWIND $sids AS s MATCH (n:Code {{sid: s}}) DETACH DELETE n", layer::OBSERVED_WRITE), params! { "sids" => orphans }).await?;
            statements += 2;
        }
        {
            let mut hashes = self.edge_hashes.lock().await;
            for path in &deleted {
                hashes.remove(path);
            }
            for (path, _, hash) in &dirty {
                hashes.insert(path.clone(), hash.clone());
            }
        }
        self.ensure_indexes().await?;
        report.statements = statements;
        report.write_ms = write_started.elapsed().as_millis() as u64;
        report.total_ms = started.elapsed().as_millis() as u64;
        *self.edge_total.lock().await = all_edges.iter().map(|(_, e, _)| e.len() as u64).sum::<u64>() + dir_edge_count;
        // Publish facts only after every fallible graph write has succeeded.
        *self.model.write().await = model;
        self.status.lock().await.packages = packages;
        self.update_status(report.total_ms).await;
        Ok(report)
    }

    /// The summaries to write, by sid: (text, its hash; none and 0 when nothing is said any more).
    /// The first sync of a tree loads the stored hashes and considers every definition.
    async fn summaries(
        &self,
        model: &Model,
        all_edges: &[(String, Vec<EdgeRow>, String)],
        changed_paths: &[String],
        dirty: &[&(String, Vec<EdgeRow>, String)],
        deleted: &[String],
    ) -> Result<HashMap<String, (Option<String>, u64)>, RpcError> {
        let mut stored_guard = self.summary_hashes.lock().await;
        let first = stored_guard.is_none();
        if first {
            *stored_guard = Some(self.stored_summary_hashes().await?);
        }
        let Some(stored) = stored_guard.as_ref() else { return Ok(HashMap::new()) };
        let (heard, reach) = summary::heard(model, all_edges);
        let mut pending: HashMap<String, (Option<String>, u64)> = HashMap::new();
        let mut seen: HashSet<String> = HashSet::new();
        let mut consider = |sid: String, symbol: &crate::facts::SymbolFact| {
            if !model::SYMBOL_KINDS.contains(&symbol.kind.as_str()) || !seen.insert(sid.clone()) {
                return;
            }
            let text = summary::compose(symbol, heard.get(sid.as_str()));
            let hash = text.as_deref().map(summary::hash64).unwrap_or(0);
            if stored.get(&summary::hash64(&sid)).copied().unwrap_or(0) != hash {
                pending.insert(sid, (text, hash));
            }
        };
        // Definitions of changed files (every file on the first sync), from their facts directly.
        let own: Vec<&str> = if first { model.files.keys().map(String::as_str).collect() } else { changed_paths.iter().map(String::as_str).collect() };
        for path in own {
            if let Some(file) = model.files.get(path) {
                for symbol in &file.symbols {
                    consider(model::symbol_sid(model::sid_prefix(file), &file.path, &symbol.qname), symbol);
                }
            }
        }
        // Definitions the words of re-resolved or deleted files reach, now or before.
        let mut reached = self.summary_reach.lock().await;
        let mut targets: Vec<String> = Vec::new();
        for path in dirty.iter().map(|(path, _, _)| path.as_str()).chain(deleted.iter().map(String::as_str)) {
            if let Some(before) = reached.remove(path) {
                targets.extend(before);
            }
            if let Some(now) = reach.get(path) {
                targets.extend(now.iter().map(|sid| sid.to_string()));
                reached.insert(path.to_owned(), now.iter().map(|sid| sid.to_string()).collect());
            }
        }
        if first {
            reached.clear();
            reached.extend(reach.iter().map(|(path, sids)| (path.to_string(), sids.iter().map(|sid| sid.to_string()).collect())));
        }
        for sid in targets {
            if let Some(symbol) = summary::lookup(model, &sid) {
                consider(sid, symbol);
            }
        }
        Ok(pending)
    }

    /// `summary_hash` of every definition that has one, by the hash of its sid.
    async fn stored_summary_hashes(&self) -> Result<HashMap<u64, u64>, RpcError> {
        let rows = self.run(&format!("{} MATCH (n:Sym) WHERE n.summary_hash IS NOT NULL RETURN n.sid, n.summary_hash", layer::OBSERVED_READ), Map::new()).await?;
        Ok(rows
            .rows
            .iter()
            .filter_map(|row| {
                let sid = row.first()?.as_str()?;
                let hash = u64::from_str_radix(row.get(1)?.as_str()?, 16).ok()?;
                Some((summary::hash64(sid), hash))
            })
            .collect())
    }

    async fn write_nodes(&self, nodes: &[NodeRow]) -> Result<usize, RpcError> {
        let mut statements = 0;
        // Group by label set, preserving the order of first appearance.
        let mut groups: Vec<(String, Vec<Value>)> = Vec::new();
        for node in nodes {
            let labels = labels_for(&node.kind);
            let row = json!({"sid": node.sid, "p": node.props});
            match groups.iter_mut().find(|(l, _)| *l == labels) {
                Some((_, rows)) => rows.push(row),
                None => groups.push((labels, vec![row])),
            }
        }
        for (labels, rows) in groups {
            for chunk in chunk_rows(rows) {
                self.run(&format!("{} UNWIND $rows AS r MERGE (n:{labels} {{sid: r.sid}}) SET n += r.p", layer::OBSERVED_WRITE), params! { "rows" => chunk }).await?;
                statements += 1;
                self.ensure_indexes().await?;
            }
        }
        Ok(statements)
    }

    async fn write_edges(&self, edges: &[&EdgeRow]) -> Result<usize, RpcError> {
        let mut statements = 0;
        let mut groups: BTreeMap<&str, Vec<Value>> = BTreeMap::new();
        for edge in edges {
            groups.entry(edge.rel).or_default().push(json!({"a": edge.from, "b": edge.to, "p": edge.props}));
        }
        for (rel, rows) in groups {
            for chunk in chunk_rows(rows) {
                self.run(
                    &format!("{} UNWIND $rows AS r MATCH (a:Code {{sid: r.a}}), (b:Code {{sid: r.b}}) CREATE (a)-[e:{rel}]->(b) SET e += r.p", layer::OBSERVED_WRITE),
                    params! { "rows" => chunk },
                )
                .await?;
                statements += 1;
            }
        }
        Ok(statements)
    }

    /// Equality indexes, declared once their property has been written (IronGraph practice).
    async fn ensure_indexes(&self) -> Result<(), RpcError> {
        let mut created = self.indexes_created.lock().await;
        if *created {
            return Ok(());
        }
        let has_sym = self.run(&format!("{} MATCH (n:Sym) RETURN n.sid LIMIT 1", layer::OBSERVED_READ), Map::new()).await?;
        let has_code = self.run(&format!("{} MATCH (n:Code) RETURN n.sid LIMIT 1", layer::OBSERVED_READ), Map::new()).await?;
        if has_code.rows.is_empty() {
            return Ok(());
        }
        self.graph.ensure_index(&self.ig_id, "code_sid", "Code", "sid").await?;
        self.graph.ensure_index(&self.ig_id, "code_path", "Code", "path").await?;
        if !has_sym.rows.is_empty() {
            self.graph.ensure_index(&self.ig_id, "sym_sid", "Sym", "sid").await?;
            self.graph.ensure_index(&self.ig_id, "sym_name", "Sym", "name").await?;
            self.graph.ensure_index(&self.ig_id, "sym_qname", "Sym", "qname").await?;
            *created = true;
        }
        Ok(())
    }

    /// Record what reaching the language plugins found (for the project status).
    async fn note_plugins(&self, snapshot: &Snapshot) {
        let mut problems = snapshot.problems.clone();
        problems.extend(snapshot.inactive.iter().map(|(category, state)| format!("{category} is {state}")));
        if !self.langs.has_host() {
            problems.push("not connected to the frame: no language plugin is reachable".into());
        }
        self.status.lock().await.plugin_problems = problems;
    }

    async fn update_status(&self, last_ms: u64) {
        let model = self.model.read().await;
        let mut status = self.status.lock().await;
        status.files = model.files.len() as u64;
        status.edges = *self.edge_total.lock().await;
        status.symbols = model.files.values().map(|f| f.symbols.len() as u64 + f.commands.len() as u64).sum();
        status.languages = model.files.values().map(|f| f.language.clone()).collect();
        status.language_plugins.clear();
        status.uncovered.clear();
        status.problems.clear();
        for file in model.files.values() {
            match &file.origin {
                Origin::Plugin { category, version } => {
                    let entry = status.language_plugins.entry(file.language.clone()).or_insert_with(|| (category.clone(), version.clone(), 0));
                    entry.2 += 1;
                }
                Origin::Uncovered => {
                    let name = file.path.rsplit('/').next().unwrap_or(&file.path);
                    let extension = name.rsplit_once('.').filter(|(stem, _)| !stem.is_empty()).map(|(_, e)| e.to_owned()).unwrap_or_default();
                    let entry = status.uncovered.entry((extension, file.language.clone())).or_insert_with(|| (0, file.path.clone()));
                    entry.0 += 1;
                }
                Origin::Failed { category, error } => status.problems.push((file.path.clone(), category.clone(), error.clone(), "text".into())),
                Origin::Kept { plugin, reason } => status.problems.push((file.path.clone(), plugin.clone(), reason.clone(), "kept".into())),
            }
        }
        status.last_index_ms = last_ms;
        status.state = "ready".into();
        status.error = None;
    }

    /// Remove third-party API nodes earlier versions wrote (only the project's own code is mapped).
    pub async fn drop_externals(&self) -> Result<(), RpcError> {
        self.run(&format!("{} MATCH (x:ExternalSymbol) DETACH DELETE x", layer::OBSERVED_WRITE), Map::new()).await?;
        Ok(())
    }

    /// Remove the code earlier versions stored on nodes: definitions' code and the leading text of
    /// files a language plugin parsed (code is read from the files).
    async fn drop_code(&self) -> Result<(), RpcError> {
        self.run(&format!("{} MATCH (n:Code) WHERE n.code IS NOT NULL REMOVE n.code", layer::OBSERVED_WRITE), Map::new()).await?;
        self.run(&format!("{} MATCH (f:File) WHERE f.extractor IS NOT NULL AND f.text IS NOT NULL REMOVE f.text", layer::OBSERVED_WRITE), Map::new()).await?;
        Ok(())
    }

    /// Move the `Description` nodes earlier versions wrote onto the symbols they describe
    /// (`description`, `described_hash`), then delete them.
    async fn move_descriptions(&self) -> Result<(), RpcError> {
        let rows = self.run(&format!("{} MATCH (d:Description)-[:DESCRIBES]->(s:Code) RETURN s.sid, d.text, d.body_hash, d.updated_ms", layer::CODE_READ), Map::new()).await?;
        let moved: Vec<Value> = rows.rows.iter().map(|r| json!({"sid": r.first(), "text": r.get(1), "hash": r.get(2), "ms": r.get(3)})).collect();
        if !moved.is_empty() {
            self.run(
                &format!("{} UNWIND $rows AS r MATCH (s:Code {{sid: r.sid}}) WHERE s.description IS NULL SET s.description = r.text, s.described_hash = r.hash, s.described_ms = r.ms", layer::OBSERVED_WRITE),
                params! { "rows" => moved },
            )
            .await?;
        }
        self.run(&format!("{} MATCH (d:Description) DETACH DELETE d", layer::KNOWLEDGE_WRITE), Map::new()).await?;
        Ok(())
    }
}

impl CodeTree {
    /// Read the git history of the tree (read-only `git log`; nothing is written to the repository).
    /// Each indexed file gets MODIFIED_IN edges to the commits that changed it.
    async fn read_history(&self) -> History {
        let root = self.root.clone();
        tokio::task::spawn_blocking(move || read_history(&root)).await.ok().flatten().unwrap_or_default()
    }

    /// Reload history after the repository's HEAD moved, and write the new edges.
    pub async fn refresh_history(&self) -> Result<SyncReport, RpcError> {
        let _guard = self.sync_lock.lock().await;
        if self.sync_pending.load(Ordering::Acquire) {
            return self.full_index_owned().await;
        }
        let mut model = self.model.read().await.clone();
        let (commits, history) = self.read_history().await;
        if model.commits.first().map(|c| &c.sha) == commits.first().map(|c| &c.sha) {
            return Ok(SyncReport::default());
        }
        model.commits = commits;
        model.history = history;
        self.sync_prepared(Vec::new(), Vec::new(), model).await
    }
}

/// `git log` of the tree, paths relative to it (newest first).
/// Commits (newest first) and, per path, the commits that modified it.
type History = (Vec<model::CommitFact>, HashMap<String, Vec<String>>);

fn read_history(root: &Path) -> Option<History> {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["log", "--relative", "--no-renames", "--name-only", "--format=%x1e%H%x1f%an%x1f%at%x1f%s"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let mut commits = Vec::new();
    let mut history: HashMap<String, Vec<String>> = HashMap::new();
    for record in text.split('\u{1e}').filter(|r| !r.trim().is_empty()) {
        let mut lines = record.lines();
        let header = lines.next().unwrap_or("");
        let fields: Vec<&str> = header.split('\u{1f}').collect();
        if fields.len() < 4 {
            continue;
        }
        let sha = fields[0].to_owned();
        commits.push(model::CommitFact { sha: sha.clone(), author: fields[1].to_owned(), time_ms: fields[2].parse::<u64>().unwrap_or(0) * 1000, subject: fields[3].to_owned() });
        for path in lines.map(str::trim).filter(|l| !l.is_empty()) {
            history.entry(path.to_owned()).or_default().push(sha.clone());
        }
    }
    Some((commits, history))
}

/// Whether a repository-relative path is kept by the tree's ignore rules.
pub fn included(root: &Path, rel: &str) -> bool {
    let mut current = root.to_path_buf();
    let segments: Vec<&str> = rel.split('/').collect();
    for seg in &segments[..segments.len().saturating_sub(1)] {
        if *seg == ".git" || *seg == "node_modules" {
            return false;
        }
        current.push(seg);
        if current.join("CACHEDIR.TAG").is_file() || current.join("pyvenv.cfg").is_file() {
            return false;
        }
    }
    let absolute = root.join(rel);
    let mut directories = vec![root.to_path_buf()];
    let mut dir = root.to_path_buf();
    for seg in &segments[..segments.len().saturating_sub(1)] {
        dir.push(seg);
        directories.push(dir.clone());
    }
    let load = |dir: &Path, name: &str| {
        let mut builder = ignore::gitignore::GitignoreBuilder::new(dir);
        let _ = builder.add(dir.join(name));
        builder.build().unwrap_or_else(|_| ignore::gitignore::Gitignore::empty())
    };
    // Each matcher has its own base, so a nested /pattern is relative to that directory.
    let rules: Vec<_> = [".ignore", ".gitignore"].iter().map(|name| directories.iter().map(|dir| load(dir, name)).collect::<Vec<_>>()).collect();
    let exclude = load(root, ".git/info/exclude");
    let (global, _) = ignore::gitignore::GitignoreBuilder::new(root).build_global();
    let mut path = root.to_path_buf();
    // Check parents before their descendants: a whitelist inside an ignored directory must
    // not make it searchable when the full scan cannot enter that directory.
    for (depth, segment) in segments.iter().enumerate() {
        path.push(segment);
        let is_dir = depth + 1 < segments.len() || absolute.is_dir();
        let mut matched = ignore::Match::None;
        // .ignore takes precedence over gitignore, then deeper files over shallower ones.
        for matchers in &rules {
            for matcher in matchers[..=depth.min(matchers.len() - 1)].iter().rev() {
                matched = matcher.matched(&path, is_dir);
                if !matched.is_none() {
                    break;
                }
            }
            if !matched.is_none() {
                break;
            }
        }
        if matched.is_none() {
            matched = exclude.matched(&path, is_dir);
        }
        if matched.is_none() {
            matched = global.matched(&path, is_dir);
        }
        if matched.is_ignore() {
            return false;
        }
    }
    true
}

/// A summary's hash as stored: hex, or null when there is no summary.
fn summary_hash_value(hash: u64) -> Value {
    if hash == 0 { Value::Null } else { json!(format!("{hash:016x}")) }
}

pub fn edge_hash(edges: &[EdgeRow]) -> String {
    let mut hasher = blake3::Hasher::new();
    for e in edges {
        hasher.update(e.rel.as_bytes());
        hasher.update(&[0]);
        hasher.update(e.from.as_bytes());
        hasher.update(&[0]);
        hasher.update(e.to.as_bytes());
        hasher.update(&[0]);
        hasher.update(serde_json::to_string(&e.props).unwrap_or_default().as_bytes());
        hasher.update(&[1]);
    }
    hasher.finalize().to_hex()[..32].to_owned()
}
