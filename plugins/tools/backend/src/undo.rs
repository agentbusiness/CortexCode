//! The step log: what every step changed in a workspace, kept since the last commit, for undo.
//!
//! ```text
//! (:Workspace {key: canonical path, head, baselined})-[:ON]->(:Machine)
//! (:Step {key: '<workspace hash>#<seq>', seq, ts_ms, actor, label, detail, undoes, session_id})
//!     -[:IN]->(:Workspace)   -[:BY]->(:Session)   -[:UNDOES]->(:Step)
//! (:StepFile {key: '<step key>#<path>', path, before, after, mode, added, removed, binary, bytes})-[:OF]->(:Step)
//! stream `steps.<workspace hash>`     every step as it was recorded
//! ```
//!
//! The nodes hold what undo still needs; the log of a workspace is read from them when it opens
//! (its standing steps and their files) and kept in memory while the plugin runs. Each step is
//! one put of its node, its files and their edges. Which steps are undone is derived when read
//! ([`active_set`]), so a step node never changes after it is written.
//!
//! Files that remain: `<plugin data>/undo/<workspace hash>/blobs/<sha256>.zst`, the contents
//! before and after each change, compressed and shared between steps (bytes, not records).
//!
//! Records come from three places. An edit made through this plugin (the edit tool, the editor's
//! save) is recorded with the text it had in hand. A command's side effects and edits made by hand
//! are found by comparing the tree with what the log knows ([`ProjectLog::reconcile`]): after every
//! command, after a quiet moment following a file-system event, before every timeline read and
//! when the log opens. git does the walking (`git status`), so a reconcile costs one status call
//! plus a read of each candidate file; a workspace that is not a repository is walked with the
//! ignore rules instead, and its files are recorded once when the log opens so there is something
//! to go back to.
//!
//! The commit boundary: on a new HEAD, every path whose content now equals HEAD leaves the log
//! (its step files are retired, a step left with nothing is retired, its blobs go when nothing
//! else needs them), so a full commit empties the log and a partial commit keeps what stays
//! uncommitted.
//!
//! Undo and redo are records too (`undoes` names the steps they reverted), so the log is linear
//! and append-only; which steps stand is derived ([`active_set`]). Nothing here holds a lock while
//! waiting: the one mutex guards the in-memory state and blob writes, which are short.
//!
//! Sequence numbers never repeat in a workspace: a log continues after the highest step ever
//! written, retired ones included. When the graph cannot be read at open, the log starts at the
//! clock's milliseconds, so its steps cannot overwrite stored ones.

use crate::graph::{self, Batch, Graph};
use crate::util;
use cc_protocol::contracts::now_ms;
use cc_protocol::contracts::tools::{
    TimelineDiffParams, TimelineDiffResult, TimelineFile, TimelineParams, TimelineRecord, TimelineResult, UndoParams, UndoResult, topics,
};
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

/// Files larger than this are stored but not diffed line by line.
const TEXT_DIFF_LIMIT: usize = 2 * 1024 * 1024;
/// A file-system event followed by this much quiet is one edit by hand.
const HAND_SETTLE: Duration = Duration::from_millis(400);

/// What the `Workspace` node holds (and the `meta.json` of logs kept in files before).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct Meta {
    workspace: String,
    /// The commit the log stands on (the last HEAD seen), when a repository.
    #[serde(default)]
    head: Option<String>,
    /// A workspace that is not a repository had its files recorded once.
    #[serde(default)]
    baselined: bool,
}

/// How a step is attributed: who, which session, what for.
#[derive(Debug, Clone, Default)]
pub struct Attribution {
    pub actor: String,
    pub session_id: Option<String>,
    pub label: String,
    pub detail: String,
}

impl Attribution {
    pub fn session(session_id: &str, label: &str, detail: &str) -> Self {
        Self { actor: format!("session:{session_id}"), session_id: Some(session_id.to_owned()), label: label.to_owned(), detail: detail.to_owned() }
    }

    pub fn user(label: &str) -> Self {
        Self { actor: "user".to_owned(), session_id: None, label: label.to_owned(), detail: String::new() }
    }

    pub fn host(label: &str, detail: &str) -> Self {
        Self { actor: "host".to_owned(), session_id: None, label: label.to_owned(), detail: detail.to_owned() }
    }
}

pub trait Publisher: Send + Sync {
    fn publish(&self, topic: &str, data: Value);
}

struct State {
    records: Vec<TimelineRecord>,
    next_seq: u64,
    meta: Meta,
    /// Non-repository workspaces: the stat of every file the log knows (size, mtime ns).
    stats: HashMap<String, (u64, u128)>,
}

/// The log of one workspace.
pub struct ProjectLog {
    workspace: PathBuf,
    /// The workspace path's hash: the prefix of its step keys, its stream and its blob folder.
    hash: String,
    dir: PathBuf,
    graph: Arc<Graph>,
    git: Option<PathBuf>,
    state: Mutex<State>,
    /// Commands and edits of this plugin in flight (their effects are recorded when they end, not
    /// by the watcher).
    busy: Mutex<u32>,
    /// The newest file-system event not yet reconciled.
    pending: Mutex<Option<Instant>>,
    watcher: Mutex<Option<RecommendedWatcher>>,
    publisher: Mutex<Option<Arc<dyn Publisher>>>,
}

/// Every workspace's log.
pub struct StepLog {
    root: PathBuf,
    graph: Arc<Graph>,
    git: Option<PathBuf>,
    logs: Mutex<HashMap<PathBuf, Arc<ProjectLog>>>,
    publisher: Mutex<Option<Arc<dyn Publisher>>>,
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn find_git() -> Option<PathBuf> {
    for candidate in ["/opt/homebrew/bin/git", "/usr/local/bin/git", "/usr/bin/git"] {
        if Path::new(candidate).is_file() {
            return Some(PathBuf::from(candidate));
        }
    }
    std::env::var_os("PATH").and_then(|path| std::env::split_paths(&path).map(|dir| dir.join("git")).find(|git| git.is_file()))
}

fn hash_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(64);
    for byte in digest {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

fn is_binary(bytes: &[u8]) -> bool {
    bytes.iter().take(8192).any(|b| *b == 0)
}

/// Lines added and removed between two texts (0, 0 and `binary` for what is not text).
fn line_counts(before: Option<&[u8]>, after: Option<&[u8]>) -> (u32, u32, bool) {
    let binary = before.is_some_and(is_binary) || after.is_some_and(is_binary);
    let too_big = before.is_some_and(|b| b.len() > TEXT_DIFF_LIMIT) || after.is_some_and(|b| b.len() > TEXT_DIFF_LIMIT);
    if binary || too_big {
        return (0, 0, binary);
    }
    let old = before.map(|b| String::from_utf8_lossy(b).into_owned()).unwrap_or_default();
    let new = after.map(|b| String::from_utf8_lossy(b).into_owned()).unwrap_or_default();
    let diff = similar::TextDiff::from_lines(&old, &new);
    let (mut added, mut removed) = (0u32, 0u32);
    for change in diff.iter_all_changes() {
        match change.tag() {
            similar::ChangeTag::Insert => added += 1,
            similar::ChangeTag::Delete => removed += 1,
            similar::ChangeTag::Equal => {}
        }
    }
    (added, removed, false)
}

/// Which steps stand: a step is active unless a later active undo lists it; undoing an undo puts
/// what it had undone back (a redo), recursively.
pub fn active_set(records: &[TimelineRecord]) -> BTreeSet<u64> {
    let by_seq: BTreeMap<u64, &TimelineRecord> = records.iter().map(|record| (record.seq, record)).collect();
    fn deactivate(seq: u64, by_seq: &BTreeMap<u64, &TimelineRecord>, active: &mut BTreeSet<u64>) {
        active.remove(&seq);
        if let Some(record) = by_seq.get(&seq) {
            for &inner in &record.undoes {
                reactivate(inner, by_seq, active);
            }
        }
    }
    fn reactivate(seq: u64, by_seq: &BTreeMap<u64, &TimelineRecord>, active: &mut BTreeSet<u64>) {
        active.insert(seq);
        if let Some(record) = by_seq.get(&seq) {
            for &inner in &record.undoes {
                deactivate(inner, by_seq, active);
            }
        }
    }
    let mut active = BTreeSet::new();
    for record in records {
        active.insert(record.seq);
        for &undone in &record.undoes {
            deactivate(undone, &by_seq, &mut active);
        }
    }
    active
}

/// A change of one path found by comparing the tree with the log.
struct Found {
    path: String,
    before: Option<Vec<u8>>,
    after: Option<Vec<u8>>,
    mode: u32,
}

impl StepLog {
    pub fn new(data_dir: &Path, graph: Arc<Graph>) -> StepLog {
        StepLog { root: data_dir.join("undo"), graph, git: find_git(), logs: Mutex::new(HashMap::new()), publisher: Mutex::new(None) }
    }

    /// The git program the logs use, when one was found.
    pub fn git(&self) -> Option<&Path> {
        self.git.as_deref()
    }

    pub fn set_publisher(&self, publisher: Arc<dyn Publisher>) {
        *lock(&self.publisher) = Some(publisher.clone());
        for log in lock(&self.logs).values() {
            *lock(&log.publisher) = Some(publisher.clone());
        }
    }

    fn hash(workspace: &Path) -> String {
        let digest = Sha256::digest(workspace.as_os_str().as_encoded_bytes());
        digest.iter().take(12).map(|b| format!("{b:02x}")).collect()
    }

    /// The log of `workspace` when it is open already.
    pub fn opened(&self, workspace: &Path) -> Option<Arc<ProjectLog>> {
        lock(&self.logs).get(&util::canonical_lenient(workspace)).cloned()
    }

    /// The log of `workspace`, opened on first use: its standing steps are read from the graph,
    /// the commit boundary is applied when HEAD moved since the log last looked, and changes made
    /// while nobody looked are recorded as made by hand.
    pub async fn open(&self, workspace: &Path) -> Arc<ProjectLog> {
        let workspace = util::canonical_lenient(workspace);
        if let Some(log) = lock(&self.logs).get(&workspace) {
            return log.clone();
        }
        let hash = Self::hash(&workspace);
        let dir = self.root.join(&hash);
        let (meta, records, next_seq) = load(&self.graph, &workspace, &hash, &dir).await;
        if let Some(log) = lock(&self.logs).get(&workspace) {
            return log.clone();
        }
        let git = self.git.clone().filter(|_| workspace.join(".git").exists());
        let log = Arc::new(ProjectLog::new(workspace.clone(), hash, dir, self.graph.clone(), git, meta, records, next_seq));
        *lock(&log.publisher) = lock(&self.publisher).clone();
        let existing = lock(&self.logs).entry(workspace).or_insert_with(|| log.clone()).clone();
        if Arc::ptr_eq(&existing, &log) {
            log.start_watching();
            log.catch_up();
        }
        existing
    }

    /// Forget a workspace that no longer exists: its nodes are retired, its blob folder goes.
    pub fn forget(&self, workspace: &Path) {
        let workspace = util::canonical_lenient(workspace);
        lock(&self.logs).remove(&workspace);
        let hash = Self::hash(&workspace);
        let _ = std::fs::remove_dir_all(self.root.join(&hash));
        let key = workspace.to_string_lossy().into_owned();
        self.graph.retire_matching(
            "StepFile",
            "MATCH (f:StepFile)-[:OF]->(:Step)-[:IN]->(w:Workspace) WHERE w.key = $workspace AND f.ended_ms IS NULL RETURN f.key LIMIT 5000",
            json!({ "workspace": key }),
            "forgotten",
        );
        self.graph.retire_matching(
            "Step",
            "MATCH (s:Step)-[:IN]->(w:Workspace) WHERE w.key = $workspace AND s.ended_ms IS NULL RETURN s.key LIMIT 5000",
            json!({ "workspace": key }),
            "forgotten",
        );
        self.graph.retire(vec![graph::gref("Workspace", &key)], "forgotten");
    }
}

/// How many times opening a log asks the graph while the memory plugin does not serve yet.
const LOAD_ATTEMPTS: u32 = 20;

/// The log as the graph keeps it: the workspace's node, its standing steps with their files, and
/// the highest sequence number ever written.
async fn read_graph(graph: &Graph, workspace: &str) -> Result<(Option<Meta>, Vec<TimelineRecord>, u64), cc_protocol::RpcError> {
    let params = json!({ "workspace": workspace });
    let node = graph.query("MATCH (w:Workspace) WHERE w.key = $workspace AND w.ended_ms IS NULL RETURN w LIMIT 1", params.clone(), 1, LOAD_ATTEMPTS).await?;
    let meta = node.rows.first().and_then(|row| row.first()).and_then(graph::properties).map(|props| Meta {
        workspace: workspace.to_owned(),
        head: props.get("head").and_then(Value::as_str).map(str::to_owned),
        baselined: props.get("baselined").and_then(Value::as_bool).unwrap_or(false),
    });
    let highest = graph
        .query("MATCH (s:Step)-[:IN]->(w:Workspace) WHERE w.key = $workspace RETURN s.seq AS seq ORDER BY seq DESC LIMIT 1", params.clone(), 1, LOAD_ATTEMPTS)
        .await?
        .rows
        .first()
        .and_then(|row| row.first())
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let steps = graph
        .query_all(
            "MATCH (s:Step)-[:IN]->(w:Workspace) WHERE w.key = $workspace AND s.ended_ms IS NULL AND s.key > $after RETURN s, s.key AS k ORDER BY k LIMIT 5000",
            params.clone(),
            LOAD_ATTEMPTS,
        )
        .await?;
    let mut by_key: HashMap<String, TimelineRecord> = HashMap::new();
    for row in &steps {
        let (Some(props), Some(key)) = (row.first().and_then(graph::properties), row.get(1).and_then(Value::as_str)) else { continue };
        let session_id = props.get("session_id").and_then(Value::as_str).map(str::to_owned);
        let undoes = props.get("undoes").and_then(Value::as_array).map(|seqs| seqs.iter().filter_map(Value::as_u64).collect()).unwrap_or_default();
        let record = TimelineRecord {
            seq: graph::number(props, "seq"),
            ts_ms: graph::number(props, "ts_ms"),
            actor: graph::text(props, "actor"),
            session_id,
            label: graph::text(props, "label"),
            detail: graph::text(props, "detail"),
            files: Vec::new(),
            undoes,
            undone: false,
        };
        by_key.insert(key.to_owned(), record);
    }
    let files = graph
        .query_all(
            "MATCH (f:StepFile)-[:OF]->(s:Step)-[:IN]->(w:Workspace) WHERE w.key = $workspace AND f.ended_ms IS NULL AND s.ended_ms IS NULL AND f.key > $after \
             RETURN f, s.key, f.key AS k ORDER BY k LIMIT 5000",
            params,
            LOAD_ATTEMPTS,
        )
        .await?;
    for row in &files {
        let (Some(props), Some(step)) = (row.first().and_then(graph::properties), row.get(1).and_then(Value::as_str)) else { continue };
        let Some(record) = by_key.get_mut(step) else { continue };
        let hash = |name: &str| props.get(name).and_then(Value::as_str).map(str::to_owned);
        record.files.push(TimelineFile {
            path: graph::text(props, "path"),
            before: hash("before"),
            after: hash("after"),
            mode: graph::number(props, "mode") as u32,
            added: graph::number(props, "added") as u32,
            removed: graph::number(props, "removed") as u32,
            binary: props.get("binary").and_then(Value::as_bool).unwrap_or(false),
            bytes: graph::number(props, "bytes"),
        });
    }
    let mut records: Vec<TimelineRecord> = by_key.into_values().collect();
    for record in &mut records {
        record.files.sort_by(|a, b| a.path.cmp(&b.path));
    }
    records.sort_by_key(|record| record.seq);
    Ok((meta, records, highest))
}

/// A log kept in files by earlier versions (`log.jsonl`, `meta.json`), if there is one.
fn read_legacy(dir: &Path) -> Option<(Option<Meta>, Vec<TimelineRecord>)> {
    let meta: Option<Meta> = std::fs::read(dir.join("meta.json")).ok().and_then(|bytes| serde_json::from_slice(&bytes).ok());
    let text = std::fs::read_to_string(dir.join("log.jsonl")).ok();
    if meta.is_none() && text.is_none() {
        return None;
    }
    let records = text.iter().flat_map(|text| text.lines()).filter_map(|line| serde_json::from_str::<TimelineRecord>(line).ok()).collect();
    Some((meta, records))
}

/// The files of an imported log go once the graph holds its records (and the temporary siblings
/// of their atomic writes with them).
fn remove_legacy(dir: &Path) {
    let _ = std::fs::remove_file(dir.join("log.jsonl"));
    let _ = std::fs::remove_file(dir.join("meta.json"));
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with(".log.jsonl.") || name.starts_with(".meta.json.") {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// A workspace's log as it opens: from the graph, and once from the files an earlier version
/// kept (written to the graph, then removed).
async fn load(graph: &Arc<Graph>, workspace: &Path, hash: &str, dir: &Path) -> (Meta, Vec<TimelineRecord>, u64) {
    let key = workspace.to_string_lossy().into_owned();
    let stored = read_graph(graph, &key).await;
    let legacy = read_legacy(dir);
    let (mut meta, mut records, highest) = match stored {
        Ok((meta, records, highest)) => (meta, records, Some(highest)),
        Err(error) => {
            tracing::error!(workspace = %key, error = %error.message, "the step log could not be read from the graph; it starts empty");
            (None, Vec::new(), None)
        }
    };
    if let Some((legacy_meta, legacy_records)) = legacy {
        meta = meta.or(legacy_meta);
        let mut by_seq: BTreeMap<u64, TimelineRecord> = records.into_iter().map(|record| (record.seq, record)).collect();
        for record in &legacy_records {
            by_seq.insert(record.seq, record.clone());
        }
        records = by_seq.into_values().collect();
        let meta = Meta { workspace: key.clone(), ..meta.clone().unwrap_or_default() };
        let mut batches = vec![workspace_batch(&meta)];
        for record in &legacy_records {
            batches.extend(step_batches(hash, &key, record));
        }
        let graph = graph.clone();
        let dir = dir.to_path_buf();
        tokio::spawn(async move {
            match graph.put_wait(batches).await {
                Ok(()) => remove_legacy(&dir),
                Err(error) => tracing::error!(dir = %dir.display(), error = %error.message, "the step log files were not imported; they are kept"),
            }
        });
    }
    let meta = Meta { workspace: key, ..meta.unwrap_or_default() };
    let kept_max = records.iter().map(|record| record.seq).max().unwrap_or(0);
    let next_seq = match highest {
        Some(highest) => highest.max(kept_max) + 1,
        None => now_ms().max(kept_max + 1),
    };
    (meta, records, next_seq)
}

/// The workspace's node.
fn workspace_batch(meta: &Meta) -> Batch {
    // `head` is null in a repository without a commit: the property goes.
    let mut props = serde_json::Map::new();
    props.insert("head".to_owned(), json!(meta.head));
    props.insert("baselined".to_owned(), json!(meta.baselined));
    let node = graph::node("Workspace", &meta.workspace, props);
    let me = graph::gref("Workspace", &meta.workspace);
    Batch::observed(vec![node], vec![graph::edge(&me, "ON", &graph::machine())], Vec::new())
}

fn step_key(hash: &str, seq: u64) -> String {
    format!("{hash}#{seq}")
}

fn file_key(hash: &str, seq: u64, path: &str) -> String {
    format!("{hash}#{seq}#{path}")
}

/// A step's node with its edges, then each of its files with its edge (packed into puts by the writer).
fn step_batches(hash: &str, workspace: &str, record: &TimelineRecord) -> Vec<Batch> {
    let key = step_key(hash, record.seq);
    let me = graph::gref("Step", &key);
    let undoes = (!record.undoes.is_empty()).then(|| record.undoes.clone());
    let node = graph::node(
        "Step",
        &key,
        graph::props(json!({
            "seq": record.seq,
            "ts_ms": record.ts_ms,
            "actor": record.actor,
            "label": record.label,
            "detail": record.detail,
            "undoes": undoes,
            "session_id": record.session_id,
        })),
    );
    let mut soft: Vec<_> = record.undoes.iter().map(|seq| graph::edge(&me, "UNDOES", &graph::gref("Step", &step_key(hash, *seq)))).collect();
    if let Some(session) = &record.session_id {
        soft.push(graph::edge(&me, "BY", &graph::gref("Session", session)));
    }
    let mut batches = vec![Batch::observed(vec![node], vec![graph::edge(&me, "IN", &graph::gref("Workspace", workspace))], soft)];
    for file in &record.files {
        let file_key = file_key(hash, record.seq, &file.path);
        let node = graph::node(
            "StepFile",
            &file_key,
            graph::props(json!({
                "path": file.path,
                "before": file.before,
                "after": file.after,
                "mode": file.mode,
                "added": file.added,
                "removed": file.removed,
                "binary": file.binary,
                "bytes": file.bytes,
            })),
        );
        batches.push(Batch::observed(vec![node], vec![graph::edge(&graph::gref("StepFile", &file_key), "OF", &me)], Vec::new()));
    }
    batches
}

impl ProjectLog {
    #[allow(clippy::too_many_arguments)]
    fn new(workspace: PathBuf, hash: String, dir: PathBuf, graph: Arc<Graph>, git: Option<PathBuf>, meta: Meta, records: Vec<TimelineRecord>, next_seq: u64) -> ProjectLog {
        let _ = std::fs::create_dir_all(dir.join("blobs"));
        // The node exists before the first step's edge to it.
        graph.put(workspace_batch(&meta));
        ProjectLog {
            workspace,
            hash,
            dir,
            graph,
            git,
            state: Mutex::new(State { records, next_seq, meta, stats: HashMap::new() }),
            busy: Mutex::new(0),
            pending: Mutex::new(None),
            watcher: Mutex::new(None),
            publisher: Mutex::new(None),
        }
    }

    pub fn workspace(&self) -> &Path {
        &self.workspace
    }

    fn publish(&self, seq: u64) {
        let publisher = lock(&self.publisher).clone();
        if let Some(publisher) = publisher {
            publisher.publish(topics::TIMELINE, json!({ "workspace": self.workspace.to_string_lossy(), "seq": seq }));
        }
    }

    // --- storage ---------------------------------------------------------------------------

    fn blob_path(&self, hash: &str) -> PathBuf {
        self.dir.join("blobs").join(format!("{hash}.zst"))
    }

    /// Keep `bytes`; returns its hash. A content already kept is not written again.
    fn keep(&self, bytes: &[u8]) -> std::io::Result<String> {
        let hash = hash_hex(bytes);
        let path = self.blob_path(&hash);
        if !path.exists() {
            let compressed = zstd::encode_all(bytes, 3)?;
            let tmp = path.with_extension(format!("zst.{}", std::process::id()));
            std::fs::write(&tmp, compressed)?;
            std::fs::rename(&tmp, &path)?;
        }
        Ok(hash)
    }

    /// The kept content of `hash`.
    pub fn content(&self, hash: &str) -> std::io::Result<Vec<u8>> {
        let compressed = std::fs::read(self.blob_path(hash))?;
        zstd::decode_all(&compressed[..])
    }

    fn write_meta(&self, meta: &Meta) {
        self.graph.put(workspace_batch(meta));
    }

    /// A new step: its nodes in one put, the record on the workspace's stream.
    fn write_step(&self, record: &TimelineRecord) {
        let workspace = self.workspace.to_string_lossy();
        self.graph.put_many(step_batches(&self.hash, &workspace, record));
        if let Ok(value) = serde_json::to_value(record) {
            self.graph.append(&format!("steps.{}", self.hash), vec![value]);
        }
    }

    /// Drop blobs no record names.
    fn collect_blobs(&self, records: &[TimelineRecord]) {
        let used: BTreeSet<&str> = records.iter().flat_map(|record| record.files.iter().flat_map(|file| [file.before.as_deref(), file.after.as_deref()])).flatten().collect();
        let Ok(entries) = std::fs::read_dir(self.dir.join("blobs")) else { return };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if let Some(hash) = name.strip_suffix(".zst")
                && !used.contains(hash)
            {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }

    // --- git -------------------------------------------------------------------------------

    fn git(&self, args: &[&str]) -> Option<Vec<u8>> {
        let git = self.git.as_ref()?;
        let output = std::process::Command::new(git).arg("-C").arg(&self.workspace).args(args).env("GIT_OPTIONAL_LOCKS", "0").output().ok()?;
        output.status.success().then_some(output.stdout)
    }

    fn head(&self) -> Option<String> {
        self.git(&["rev-parse", "HEAD"]).map(|out| String::from_utf8_lossy(&out).trim().to_owned()).filter(|head| !head.is_empty())
    }

    /// Paths that differ from HEAD or are untracked (not ignored), by git.
    fn dirty(&self) -> Option<BTreeSet<String>> {
        let out = self.git(&["status", "--porcelain=v1", "-z", "--untracked-files=all", "--no-renames"])?;
        let mut paths = BTreeSet::new();
        for entry in out.split(|b| *b == 0) {
            if entry.len() > 3 {
                paths.insert(String::from_utf8_lossy(&entry[3..]).into_owned());
            }
        }
        Some(paths)
    }

    /// The content of `path` in HEAD (None: not in HEAD).
    fn head_content(&self, path: &str) -> Option<Vec<u8>> {
        self.git(&["show", &format!("HEAD:{path}")])
    }

    // --- reading the tree -----------------------------------------------------------------

    /// The file's bytes and mode, None when it does not exist (a directory or a link counts as none).
    fn read(&self, path: &str) -> Option<(Vec<u8>, u32)> {
        let absolute = self.workspace.join(path);
        let meta = std::fs::symlink_metadata(&absolute).ok()?;
        if !meta.is_file() {
            return None;
        }
        use std::os::unix::fs::PermissionsExt;
        let mode = meta.permissions().mode() & 0o777;
        std::fs::read(&absolute).ok().map(|bytes| (bytes, mode))
    }

    /// The newest active record's view of `path`: its hash after that step (None: deleted), or
    /// `Err` when no active step touched it.
    fn last_known(records: &[TimelineRecord], active: &BTreeSet<u64>, path: &str) -> Result<Option<String>, ()> {
        for record in records.iter().rev() {
            if !active.contains(&record.seq) {
                continue;
            }
            if let Some(file) = record.files.iter().find(|file| file.path == path) {
                return Ok(file.after.clone());
            }
        }
        Err(())
    }

    // --- recording -------------------------------------------------------------------------

    fn file_change(&self, found: &Found) -> Option<TimelineFile> {
        let before = match &found.before {
            Some(bytes) => Some(self.keep(bytes).ok()?),
            None => None,
        };
        let after = match &found.after {
            Some(bytes) => Some(self.keep(bytes).ok()?),
            None => None,
        };
        if before == after {
            return None;
        }
        let (added, removed, binary) = line_counts(found.before.as_deref(), found.after.as_deref());
        Some(TimelineFile {
            path: found.path.clone(),
            before,
            after,
            mode: if found.after.is_some() { found.mode } else { 0 },
            added,
            removed,
            binary,
            bytes: found.after.as_ref().map(|b| b.len() as u64).unwrap_or(0),
        })
    }

    fn append(&self, state: &mut State, who: &Attribution, files: Vec<TimelineFile>, undoes: Vec<u64>) -> TimelineRecord {
        let record = TimelineRecord {
            seq: state.next_seq,
            ts_ms: now_ms(),
            actor: who.actor.clone(),
            session_id: who.session_id.clone(),
            label: who.label.clone(),
            detail: who.detail.clone(),
            files,
            undoes,
            undone: false,
        };
        state.next_seq += 1;
        self.write_step(&record);
        state.records.push(record.clone());
        record
    }

    /// Record an edit this plugin made, with the contents it had in hand. Nothing is recorded when
    /// the log already knows `after` (the watcher saw it first).
    pub fn record_edit(&self, who: &Attribution, path: &str, before: Option<&[u8]>, after: Option<&[u8]>, mode: u32) -> Option<TimelineRecord> {
        let mut state = lock(&self.state);
        let active = active_set(&state.records);
        if let Ok(known) = Self::last_known(&state.records, &active, path)
            && known == after.map(hash_hex)
        {
            return None;
        }
        let found = Found { path: path.to_owned(), before: before.map(<[u8]>::to_vec), after: after.map(<[u8]>::to_vec), mode };
        let file = self.file_change(&found)?;
        if let Some(after) = &found.after {
            state.stats.insert(path.to_owned(), (after.len() as u64, 0));
        }
        let record = self.append(&mut state, who, vec![file], Vec::new());
        drop(state);
        self.publish(record.seq);
        Some(record)
    }

    /// Record what changed on disk since the log last looked, attributed to `who`.
    pub fn reconcile(&self, who: &Attribution) -> Option<TimelineRecord> {
        let mut state = lock(&self.state);
        let found = if self.git.is_some() { self.find_git_changes(&mut state) } else { self.find_walk_changes(&mut state) };
        if found.is_empty() {
            return None;
        }
        let files: Vec<TimelineFile> = found.iter().filter_map(|change| self.file_change(change)).collect();
        if files.is_empty() {
            return None;
        }
        let record = self.append(&mut state, who, files, Vec::new());
        drop(state);
        self.publish(record.seq);
        Some(record)
    }

    fn find_git_changes(&self, state: &mut State) -> Vec<Found> {
        let Some(dirty) = self.dirty() else { return Vec::new() };
        self.apply_boundary(state, &dirty);
        let active = active_set(&state.records);
        let mut candidates: BTreeSet<String> = dirty.clone();
        for record in &state.records {
            if active.contains(&record.seq) {
                candidates.extend(record.files.iter().map(|file| file.path.clone()));
            }
        }
        let mut found = Vec::new();
        for path in candidates {
            if path.starts_with(".git/") || path == ".git" {
                continue;
            }
            let current = self.read(&path);
            let current_hash = current.as_ref().map(|(bytes, _)| hash_hex(bytes));
            let known = match Self::last_known(&state.records, &active, &path) {
                Ok(hash) => hash,
                // Never seen: unchanged unless git says it differs from HEAD.
                Err(()) => {
                    if !dirty.contains(&path) {
                        continue;
                    }
                    self.head_content(&path).map(|bytes| hash_hex(&bytes))
                }
            };
            if known == current_hash {
                continue;
            }
            let before = match &known {
                Some(hash) => self.content(hash).ok().or_else(|| self.head_content(&path)),
                None => None,
            };
            let (after, mode) = match current {
                Some((bytes, mode)) => (Some(bytes), mode),
                None => (None, 0),
            };
            found.push(Found { path, before, after, mode });
        }
        found
    }

    /// Without git: every file the ignore rules admit, compared by size and mtime with what the log
    /// saw, then by content.
    fn find_walk_changes(&self, state: &mut State) -> Vec<Found> {
        let mut seen: HashMap<String, (u64, u128)> = HashMap::new();
        for entry in ignore::WalkBuilder::new(&self.workspace).hidden(false).parents(true).build().flatten() {
            let path = entry.path();
            if path == self.workspace || path.components().any(|part| part.as_os_str() == ".git") {
                continue;
            }
            let Ok(meta) = std::fs::symlink_metadata(path) else { continue };
            if !meta.is_file() {
                continue;
            }
            let relative = util::display_rel(path, &self.workspace);
            let mtime = meta.modified().ok().and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok()).map(|d| d.as_nanos()).unwrap_or(0);
            seen.insert(relative, (meta.len(), mtime));
        }
        let active = active_set(&state.records);
        let first = !state.meta.baselined;
        let mut found = Vec::new();
        let mut candidates: BTreeSet<String> = seen.keys().cloned().collect();
        candidates.extend(state.stats.keys().cloned());
        for path in candidates {
            let unchanged_stat = seen.get(&path).is_some_and(|stat| state.stats.get(&path) == Some(stat));
            if unchanged_stat && !first {
                continue;
            }
            let current = self.read(&path);
            let current_hash = current.as_ref().map(|(bytes, _)| hash_hex(bytes));
            let known = Self::last_known(&state.records, &active, &path).unwrap_or(None);
            if known == current_hash && !(first && known.is_none() && current.is_some()) {
                continue;
            }
            let before = known.as_ref().and_then(|hash| self.content(hash).ok());
            let (after, mode) = match current {
                Some((bytes, mode)) => (Some(bytes), mode),
                None => (None, 0),
            };
            found.push(Found { path, before, after, mode });
        }
        state.stats = seen;
        if first {
            state.meta.baselined = true;
            self.write_meta(&state.meta);
        }
        found
    }

    // --- the commit boundary ----------------------------------------------------------------

    /// The commit boundary, applied with every look at the tree: a path git no longer reports as
    /// different from HEAD is committed (or gone, or ignored now), so nothing is left to go back to
    /// and its entries leave the log; blobs nobody names go with them. HEAD is noted for the
    /// timeline. Idempotent: running it between a commit's steps or after it ends the same.
    fn apply_boundary(&self, state: &mut State, dirty: &BTreeSet<String>) {
        let head = self.head();
        let head_moved = head != state.meta.head;
        if head_moved {
            state.meta.head = head;
            self.write_meta(&state.meta);
        }
        let before = state.records.len();
        let mut touched = false;
        let mut retired = Vec::new();
        for record in &mut state.records {
            let kept = record.files.len();
            for file in record.files.iter().filter(|file| !dirty.contains(&file.path)) {
                retired.push(graph::gref("StepFile", &file_key(&self.hash, record.seq, &file.path)));
            }
            record.files.retain(|file| dirty.contains(&file.path));
            touched |= kept != record.files.len();
        }
        if !touched {
            return;
        }
        let all = std::mem::take(&mut state.records);
        let mut kept: Vec<TimelineRecord> = all.iter().filter(|record| !record.files.is_empty() || !record.undoes.is_empty()).cloned().collect();
        // An undo whose every target left the log is bookkeeping with nothing to undo; it goes
        // too, and so does an undo of it, until nothing more goes.
        loop {
            let seqs: BTreeSet<u64> = kept.iter().map(|record| record.seq).collect();
            let before = kept.len();
            kept.retain(|record| !record.files.is_empty() || record.undoes.iter().any(|seq| seqs.contains(seq)));
            if kept.len() == before {
                break;
            }
        }
        let seqs: BTreeSet<u64> = kept.iter().map(|record| record.seq).collect();
        for record in &all {
            if !seqs.contains(&record.seq) {
                retired.extend(record.files.iter().map(|file| graph::gref("StepFile", &file_key(&self.hash, record.seq, &file.path))));
                retired.push(graph::gref("Step", &step_key(&self.hash, record.seq)));
            }
        }
        self.graph.retire(retired, "committed");
        self.collect_blobs(&kept);
        let changed = kept.len() != before;
        state.records = kept;
        if changed || touched {
            let seq = state.next_seq - 1;
            self.publish(seq);
        }
    }

    /// A commit, a checkout, a reset: look at the tree (the boundary is part of it).
    pub fn on_head_change(&self) {
        self.reconcile(&Attribution::user("hand"));
    }

    /// At open: the boundary when HEAD moved while nobody looked, then changes made meanwhile.
    fn catch_up(&self) {
        self.reconcile(&Attribution::user("hand"));
    }

    // --- in flight and the watcher ---------------------------------------------------------

    /// A command or edit of this plugin starts: the watcher leaves its effects to [`Self::end`].
    pub fn begin(&self) {
        *lock(&self.busy) += 1;
    }

    /// It ended: its effects are recorded as `who`'s.
    pub fn end(&self, who: &Attribution) -> Option<TimelineRecord> {
        let record = self.reconcile(who);
        let mut busy = lock(&self.busy);
        *busy = busy.saturating_sub(1);
        record
    }

    fn start_watching(self: &Arc<Self>) {
        let Ok(runtime) = tokio::runtime::Handle::try_current() else { return };
        let weak: Weak<ProjectLog> = Arc::downgrade(self);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<()>();
        let watcher = notify::recommended_watcher(move |event: Result<notify::Event, notify::Error>| {
            // Files of the tree, and the repository's own HEAD, index and refs (a commit, a
            // checkout, a reset: the boundary); git's lock files and objects say nothing.
            if let Ok(event) = event
                && event.paths.iter().any(|path| {
                    let inside_git = path.components().any(|part| part.as_os_str() == ".git");
                    if !inside_git {
                        return true;
                    }
                    let name = path.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default();
                    let under_refs = path.components().any(|part| part.as_os_str() == "refs");
                    (name == "HEAD" || name == "index" || name == "packed-refs" || name == "ORIG_HEAD" || under_refs) && !name.ends_with(".lock")
                })
            {
                let _ = tx.send(());
            }
        });
        let Ok(mut watcher) = watcher else { return };
        if watcher.watch(&self.workspace, RecursiveMode::Recursive).is_err() {
            return;
        }
        *lock(&self.watcher) = Some(watcher);
        runtime.spawn(async move {
            while rx.recv().await.is_some() {
                let Some(log) = weak.upgrade() else { return };
                *lock(&log.pending) = Some(Instant::now());
                drop(log);
                // One edit by hand is a burst of events followed by quiet; the first event of the
                // burst waits it out, the rest find the moment already taken.
                loop {
                    tokio::time::sleep(HAND_SETTLE).await;
                    let Some(log) = weak.upgrade() else { return };
                    let settled = lock(&log.pending).is_some_and(|last| last.elapsed() >= HAND_SETTLE);
                    if !settled {
                        continue;
                    }
                    *lock(&log.pending) = None;
                    if *lock(&log.busy) == 0 {
                        let log = log.clone();
                        let _ = tokio::task::spawn_blocking(move || log.reconcile(&Attribution::user("hand"))).await;
                    }
                    break;
                }
                // Events that arrived while reconciling are drained: the reconcile saw the tree.
                while rx.try_recv().is_ok() {}
            }
        });
    }

    // --- reading ---------------------------------------------------------------------------

    /// The newest standing step that touched `path` (relative): its session and label.
    pub fn last_step(&self, path: &str) -> Option<(Option<String>, String)> {
        let state = lock(&self.state);
        let active = active_set(&state.records);
        state
            .records
            .iter()
            .rev()
            .filter(|record| active.contains(&record.seq))
            .find(|record| record.files.iter().any(|file| file.path == path))
            .map(|record| (record.session_id.clone(), record.label.clone()))
    }

    pub fn timeline(&self, params: &TimelineParams) -> TimelineResult {
        let state = lock(&self.state);
        let active = active_set(&state.records);
        let mut records: Vec<TimelineRecord> = state
            .records
            .iter()
            .filter(|record| params.since_seq.is_none_or(|since| record.seq > since))
            .filter(|record| params.session_id.as_deref().is_none_or(|session| record.session_id.as_deref() == Some(session)))
            .filter(|record| params.since_ms.is_none_or(|since| record.ts_ms >= since))
            .cloned()
            .map(|mut record| {
                record.undone = !active.contains(&record.seq);
                record
            })
            .collect();
        records.reverse();
        if let Some(limit) = params.limit {
            records.truncate(limit);
        }
        let mut paths = BTreeSet::new();
        let (mut added, mut removed) = (0u32, 0u32);
        for record in state.records.iter().filter(|record| active.contains(&record.seq) && record.undoes.is_empty()) {
            for file in &record.files {
                paths.insert(file.path.clone());
                added += file.added;
                removed += file.removed;
            }
        }
        TimelineResult {
            records,
            head_seq: state.next_seq - 1,
            head: state.meta.head.clone(),
            files: paths.len() as u32,
            added,
            removed,
        }
    }

    pub fn diff(&self, params: &TimelineDiffParams) -> TimelineDiffResult {
        let state = lock(&self.state);
        let to = params.to_seq.unwrap_or(params.from_seq);
        let mut before: Option<Option<String>> = None;
        let mut after: Option<Option<String>> = None;
        for record in &state.records {
            if record.seq < params.from_seq || record.seq > to {
                continue;
            }
            if let Some(file) = record.files.iter().find(|file| file.path == params.path) {
                if before.is_none() {
                    before = Some(file.before.clone());
                }
                after = Some(file.after.clone());
            }
        }
        drop(state);
        let (Some(before), Some(after)) = (before, after) else {
            return TimelineDiffResult { path: params.path.clone(), diff: String::new(), binary: false, status: "unchanged".to_owned() };
        };
        let old = before.as_deref().and_then(|hash| self.content(hash).ok());
        let new = after.as_deref().and_then(|hash| self.content(hash).ok());
        let status = match (&old, &new) {
            (None, Some(_)) => "added",
            (Some(_), None) => "deleted",
            (Some(a), Some(b)) if a == b => "unchanged",
            _ => "modified",
        };
        let binary = old.as_deref().is_some_and(is_binary) || new.as_deref().is_some_and(is_binary);
        if binary || status == "unchanged" {
            return TimelineDiffResult { path: params.path.clone(), diff: String::new(), binary, status: status.to_owned() };
        }
        let old_text = old.map(|b| String::from_utf8_lossy(&b).into_owned()).unwrap_or_default();
        let new_text = new.map(|b| String::from_utf8_lossy(&b).into_owned()).unwrap_or_default();
        let diff = similar::TextDiff::from_lines(&old_text, &new_text)
            .unified_diff()
            .context_radius(3)
            .header(&format!("a/{}", params.path), &format!("b/{}", params.path))
            .to_string();
        TimelineDiffResult { path: params.path.clone(), diff, binary: false, status: status.to_owned() }
    }

    // --- undo and redo ---------------------------------------------------------------------

    /// Undo steps. `allowed` says which paths may be written (the sandbox's rule for the workspace).
    pub fn undo(&self, params: &UndoParams, allowed: &dyn Fn(&Path) -> bool) -> Result<UndoResult, String> {
        let who = Attribution {
            actor: params.actor.clone().unwrap_or_else(|| params.session_id.as_deref().map(|s| format!("session:{s}")).unwrap_or_else(|| "user".to_owned())),
            session_id: params.session_id.clone(),
            label: "undo".to_owned(),
            detail: String::new(),
        };
        // What stands on disk now is the starting point: a change nobody recorded yet becomes a
        // step first, so the undo never overwrites it unseen.
        self.reconcile(&Attribution::user("hand"));
        let mut state = lock(&self.state);
        let active = active_set(&state.records);
        let mut wanted: BTreeSet<u64> = BTreeSet::new();
        if let Some(to) = params.to_seq {
            wanted.extend(state.records.iter().filter(|record| record.seq > to && active.contains(&record.seq)).map(|record| record.seq));
        }
        for seq in &params.seqs {
            if active.contains(seq) {
                wanted.insert(*seq);
            }
        }
        if wanted.is_empty() {
            return Ok(UndoResult::default());
        }
        self.revert(&mut state, who, wanted, params.cascade || params.to_seq.is_some(), allowed)
    }

    /// Undo the newest active undo (which puts back what it had undone).
    pub fn redo(&self, session_id: Option<&str>, actor: Option<&str>, allowed: &dyn Fn(&Path) -> bool) -> Result<UndoResult, String> {
        self.reconcile(&Attribution::user("hand"));
        let mut state = lock(&self.state);
        let active = active_set(&state.records);
        let Some(latest) = state.records.iter().rev().find(|record| active.contains(&record.seq) && !record.undoes.is_empty()).map(|record| record.seq) else {
            return Ok(UndoResult::default());
        };
        let who = Attribution {
            actor: actor.map(str::to_owned).unwrap_or_else(|| session_id.map(|s| format!("session:{s}")).unwrap_or_else(|| "user".to_owned())),
            session_id: session_id.map(str::to_owned),
            label: "redo".to_owned(),
            detail: String::new(),
        };
        self.revert(&mut state, who, BTreeSet::from([latest]), true, allowed)
    }

    /// Revert the active steps `wanted` (and, with `cascade`, every later active step that shares
    /// a file with them, transitively): each path goes back to what it was before the earliest of
    /// those steps touched it.
    fn revert(&self, state: &mut State, who: Attribution, wanted: BTreeSet<u64>, cascade: bool, allowed: &dyn Fn(&Path) -> bool) -> Result<UndoResult, String> {
        let active = active_set(&state.records);
        let by_seq: BTreeMap<u64, TimelineRecord> = state.records.iter().filter(|record| active.contains(&record.seq)).cloned().map(|record| (record.seq, record)).collect();
        let paths_of = |seq: u64| -> BTreeSet<String> { by_seq.get(&seq).map(|record| record.files.iter().map(|file| file.path.clone()).collect()).unwrap_or_default() };
        let mut closure = wanted.clone();
        let mut paths: BTreeSet<String> = closure.iter().flat_map(|seq| paths_of(*seq)).collect();
        loop {
            let earliest = *closure.iter().next().unwrap_or(&0);
            let more: BTreeSet<u64> = by_seq.keys().copied().filter(|seq| *seq > earliest && !closure.contains(seq) && !paths_of(*seq).is_disjoint(&paths)).collect();
            if more.is_empty() {
                break;
            }
            if !cascade {
                return Ok(UndoResult { record: None, blocked_by: more.into_iter().collect(), restored: Vec::new(), skipped: Vec::new() });
            }
            for seq in more {
                paths.extend(paths_of(seq));
                closure.insert(seq);
            }
        }
        // Per path: the content before the earliest step of the closure that touched it, and what
        // stands now (the newest active step's after), which must be what is on disk.
        let mut targets: BTreeMap<String, (Option<String>, u32)> = BTreeMap::new();
        let mut current: BTreeMap<String, Option<String>> = BTreeMap::new();
        for seq in &closure {
            let Some(record) = by_seq.get(seq) else { continue };
            for file in &record.files {
                targets.entry(file.path.clone()).or_insert((file.before.clone(), file.mode));
            }
        }
        for path in targets.keys() {
            current.insert(path.clone(), Self::last_known(&state.records, &active, path).unwrap_or(None));
        }
        let mut restored = Vec::new();
        let mut skipped = Vec::new();
        let mut files = Vec::new();
        for (path, (target, _)) in &targets {
            let absolute = self.workspace.join(path);
            if !util::is_inside(&absolute, &self.workspace) || !allowed(&absolute) {
                skipped.push(format!("{path}: not writable here"));
                continue;
            }
            let on_disk = self.read(path);
            let on_disk_hash = on_disk.as_ref().map(|(bytes, _)| hash_hex(bytes));
            if &on_disk_hash != current.get(path).unwrap_or(&None) {
                skipped.push(format!("{path}: changed on disk since the log last looked"));
                continue;
            }
            let before_bytes = on_disk.as_ref().map(|(bytes, _)| bytes.clone());
            let mode = on_disk.as_ref().map(|(_, mode)| *mode).unwrap_or(0o644);
            let after_bytes = match target {
                Some(hash) => match self.content(hash) {
                    Ok(bytes) => Some(bytes),
                    Err(error) => {
                        skipped.push(format!("{path}: its earlier content is no longer kept ({error})"));
                        continue;
                    }
                },
                None => None,
            };
            let outcome = match &after_bytes {
                Some(bytes) => {
                    if let Some(parent) = absolute.parent() {
                        let _ = std::fs::create_dir_all(parent);
                    }
                    util::atomic_write_checked(&absolute, bytes, allowed).and_then(|()| {
                        use std::os::unix::fs::PermissionsExt;
                        let keep_mode = targets.get(path).map(|(_, mode)| *mode).filter(|mode| *mode != 0).unwrap_or(mode);
                        std::fs::set_permissions(&absolute, std::fs::Permissions::from_mode(keep_mode))
                    })
                }
                None => match std::fs::remove_file(&absolute) {
                    Ok(()) => Ok(()),
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
                    Err(error) => Err(error),
                },
            };
            match outcome {
                Ok(()) => {
                    restored.push(path.clone());
                    let found = Found { path: path.clone(), before: before_bytes, after: after_bytes, mode };
                    if let Some(file) = self.file_change(&found) {
                        files.push(file);
                    }
                }
                Err(error) => skipped.push(format!("{path}: {error}")),
            }
        }
        if restored.is_empty() {
            return Err(if skipped.is_empty() { "nothing to undo".to_owned() } else { skipped.join("; ") });
        }
        let record = self.append(state, &who, files, closure.into_iter().collect());
        self.publish(record.seq);
        Ok(UndoResult { record: Some(record), blocked_by: Vec::new(), restored, skipped })
    }
}

impl ProjectLog {
    /// An edit of this plugin ended; it recorded itself, so nothing is looked for.
    pub fn end_quiet(&self) {
        let mut busy = lock(&self.busy);
        *busy = busy.saturating_sub(1);
    }
}
