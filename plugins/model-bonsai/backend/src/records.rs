//! The plugin's records in the memory plugin's graph (`memory.graph.*`): the digest computed for
//! each model file, so a multi-gigabyte file is hashed once, not on every start. The uzu plugin
//! keeps its digests the same way, so the two share them.
//!
//! ```text
//! (:ModelFile {key: absolute path, bytes, modified_ns: decimal string, digest: "sha256:<hex>" | "gitsha1:<hex>"})
//!     -[:ON]->(:Machine)                                                   layer observed
//! ```
//!
//! A digest counts only while the file has the recorded size and modification time. The records
//! are read into memory once, when the memory plugin serves. The model plugin starts first, so a
//! load waits for that read up to [`WAIT`]; when memory does not answer in time, files without a
//! record are hashed, and the records are written once memory serves. Nothing is written to a file.
//! The model files (the vision projector among them) and the saved seeds are bytes: they stay
//! files.
//!
//! Earlier versions kept the digests in `<models_dir>/.cortexcode-verified.json`. It is moved into
//! the graph once and deleted.

use crate::variants;
use cc_protocol::contracts::memory::{GraphEdge, GraphLayer, GraphNode, GraphPutParams, GraphQueryParams, GraphQueryResult, GraphRef, GraphRetireParams, MachineInfo, methods};
use cc_protocol::{Ctx, RpcError, codes};
use serde::Deserialize;
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::Duration;
use tokio::sync::watch;

/// How long a load waits for the memory plugin to read the records.
const WAIT: Duration = Duration::from_secs(60);
/// How long one call to the memory plugin may take.
const CALL_LIMIT: Duration = Duration::from_secs(10);
/// Nodes per put (each node has its edge to the machine; a put takes 5000 items).
const PUT_NODES: usize = 2000;
/// The digest record of earlier versions, in the models directory.
const OLD_RECORD: &str = ".cortexcode-verified.json";
const FILE: &str = "ModelFile";

/// One digest: computed for a file at a size and modification time.
#[derive(Debug, Clone, PartialEq)]
struct Verified {
    bytes: u64,
    modified_ns: u128,
    /// `sha256:<hex>` or `gitsha1:<hex>`.
    digest: String,
}

/// Whether the memory plugin has served the records.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Memory {
    /// Not yet: the read is being retried.
    Waiting,
    /// The records are read; new ones are written through.
    Serving,
    /// It refused the records (an error retrying cannot fix): they stay in this process.
    Refused,
}

struct Link {
    ctx: Ctx,
    tokio: tokio::runtime::Handle,
}

struct Inner {
    models_dir: PathBuf,
    /// The app's old models folder, whose files are moved into `models_dir` at every start.
    moved_from: Option<PathBuf>,
    /// Records of files that were moved, still under their old path: retired once written anew.
    moved: Mutex<Vec<GraphRef>>,
    digests: Mutex<BTreeMap<String, Verified>>,
    /// Paths whose digest is not written yet.
    pending: Mutex<BTreeSet<String>>,
    link: OnceLock<Link>,
    machine: OnceLock<String>,
    memory: watch::Sender<Memory>,
    /// The bounded wait is over (served or timed out): later loads do not wait again.
    waited: watch::Sender<bool>,
}

/// The digest records, in memory and in the graph. Cheap to clone.
#[derive(Clone)]
pub struct VerifyCache {
    inner: Arc<Inner>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn modified_ns(path: &Path) -> Option<u128> {
    std::fs::metadata(path).ok()?.modified().ok()?.duration_since(std::time::UNIX_EPOCH).ok().map(|age| age.as_nanos())
}

/// The `kind:hex` form (`sha256:` is implied for the bare hex earlier versions wrote).
fn normalized(stored: &str) -> String {
    if stored.contains(':') { stored.to_owned() } else { format!("sha256:{stored}") }
}

/// An error that goes away once the memory plugin has started.
fn passing(error: &RpcError) -> bool {
    [codes::UNAVAILABLE, codes::CONNECTION_CLOSED, codes::DRAINING, codes::TIMEOUT].contains(&error.code.as_str())
}

impl VerifyCache {
    /// Records kept in this process until [`VerifyCache::attach`] links them to the graph (the
    /// command-line installer never does: its digests are computed again by the app).
    /// `moved_from`: the old models folder moved into `models_dir` at every start
    /// (`docs/SHARED-MODELS.md`); the records of its files follow them, so they are not hashed again.
    pub fn new(models_dir: &Path, moved_from: Option<&Path>) -> Self {
        let (memory, _) = watch::channel(Memory::Waiting);
        let (waited, _) = watch::channel(false);
        Self {
            inner: Arc::new(Inner {
                models_dir: models_dir.to_path_buf(),
                moved_from: moved_from.map(Path::to_path_buf),
                moved: Mutex::default(),
                digests: Mutex::default(),
                pending: Mutex::default(),
                link: OnceLock::new(),
                machine: OnceLock::new(),
                memory,
                waited,
            }),
        }
    }

    /// Read the records from the graph (retried until the memory plugin serves), move the old
    /// record files into it, and write new records through from then on.
    pub fn attach(&self, ctx: Ctx) {
        let Ok(tokio) = tokio::runtime::Handle::try_current() else { return };
        if self.inner.link.set(Link { ctx, tokio: tokio.clone() }).is_err() {
            return;
        }
        let cache = self.clone();
        tokio.spawn(async move { cache.serve().await });
    }

    /// Wait (once, at most [`WAIT`]) for the records to be read, so files with a record are not
    /// hashed again. Returns at once when not attached or already waited.
    pub async fn wait(&self) {
        if self.inner.link.get().is_none() || *self.inner.waited.borrow() {
            return;
        }
        let mut memory = self.inner.memory.subscribe();
        if *memory.borrow_and_update() == Memory::Waiting {
            tracing::info!("waiting up to {} s for the memory plugin to read the model file records", WAIT.as_secs());
            let served = tokio::time::timeout(WAIT, memory.wait_for(|memory| *memory != Memory::Waiting)).await;
            if served.is_err() {
                tracing::warn!(
                    "the memory plugin did not answer in {} s: model files without a record in this process are hashed, and their records are written when it serves",
                    WAIT.as_secs()
                );
            }
        }
        self.inner.waited.send_replace(true);
    }

    async fn call(&self, method: &str, params: Value) -> Result<Value, RpcError> {
        let link = self.inner.link.get().ok_or_else(|| RpcError::new(codes::UNAVAILABLE, "not attached"))?;
        match tokio::time::timeout(CALL_LIMIT, link.ctx.call(method, params)).await {
            Ok(answer) => answer,
            Err(_) => Err(RpcError::new(codes::TIMEOUT, format!("{method} did not answer in {} s", CALL_LIMIT.as_secs()))),
        }
    }

    /// One call, retried while the memory plugin is not serving yet (every second for the first
    /// minute, then every ten seconds).
    async fn call_until_served(&self, method: &str, params: Value) -> Result<Value, RpcError> {
        let started = std::time::Instant::now();
        let mut logged = false;
        loop {
            match self.call(method, params.clone()).await {
                Err(error) if passing(&error) => {
                    if !logged {
                        tracing::info!(error = %error.message, "the memory plugin is not serving yet; the model file records are read when it does");
                        logged = true;
                    }
                    let pause = if started.elapsed() < WAIT { 1 } else { 10 };
                    tokio::time::sleep(Duration::from_secs(pause)).await;
                }
                answer => return answer,
            }
        }
    }

    async fn serve(&self) {
        match self.read().await {
            Ok(()) => {
                self.inner.memory.send_replace(Memory::Serving);
                self.migrate().await;
                self.flush().await;
                self.retire_moved().await;
            }
            Err(error) => {
                tracing::warn!(error = %error.message, "the memory plugin refused the model file records: digests are kept in this process only (update the memory plugin, then restart the app)");
                self.inner.memory.send_replace(Memory::Refused);
            }
        }
    }

    /// The machine's key, then every record of this machine.
    async fn read(&self) -> Result<(), RpcError> {
        let machine: MachineInfo = serde_json::from_value(self.call_until_served(methods::GRAPH_MACHINE, json!({})).await?)
            .map_err(|error| RpcError::internal(format!("memory.graph.machine: {error}")))?;
        let _ = self.inner.machine.set(machine.key.clone());
        let files = self.query(&format!("MATCH (f:{FILE})-[:ON]->(m:Machine) WHERE m.key = $machine AND f.ended_ms IS NULL RETURN f"), &machine.key).await?;
        {
            let mut pending = lock(&self.inner.pending);
            let mut digests = lock(&self.inner.digests);
            for props in files {
                let (Some(key), Some(bytes), Some(modified), Some(digest)) = (
                    props.get("key").and_then(Value::as_str),
                    props.get("bytes").and_then(Value::as_u64),
                    props.get("modified_ns").and_then(Value::as_str).and_then(|text| text.parse::<u128>().ok()),
                    props.get("digest").and_then(Value::as_str),
                ) else {
                    continue;
                };
                let verified = Verified { bytes, modified_ns: modified, digest: digest.to_owned() };
                // A file moved from the old models folder keeps its record under its new path.
                if let Some(moved) = self.moved_path(key)
                    && std::fs::metadata(&moved).is_ok_and(|meta| meta.len() == bytes)
                    && modified_ns(&moved) == Some(modified)
                {
                    let moved = moved.display().to_string();
                    if !pending.contains(&moved) && !digests.contains_key(&moved) {
                        digests.insert(moved.clone(), verified.clone());
                        pending.insert(moved);
                    }
                    lock(&self.inner.moved).push(GraphRef { label: FILE.into(), key: key.to_owned() });
                    continue;
                }
                // A digest computed in this process meanwhile is newer.
                if !pending.contains(key) {
                    digests.insert(key.to_owned(), verified);
                }
            }
        }
        Ok(())
    }

    /// Where a file recorded under the old models folder is now, when this start moved it.
    fn moved_path(&self, key: &str) -> Option<PathBuf> {
        let rest = Path::new(key).strip_prefix(self.inner.moved_from.as_ref()?).ok()?;
        let now = self.inner.models_dir.join(rest);
        (!Path::new(key).exists() && now.exists()).then_some(now)
    }

    /// Retire the records left under the old paths of moved files, once their new ones are written.
    async fn retire_moved(&self) {
        let refs = std::mem::take(&mut *lock(&self.inner.moved));
        if refs.is_empty() || !lock(&self.inner.pending).is_empty() {
            lock(&self.inner.moved).extend(refs);
            return;
        }
        let params = GraphRetireParams { refs, reason: Some("moved".into()), at_ms: None };
        if let Err(error) = self.call(methods::GRAPH_RETIRE, serde_json::to_value(&params).unwrap_or(Value::Null)).await {
            tracing::warn!(error = %error.message, "the records of moved model files keep their old paths too; retired at the next start");
        }
    }

    /// The properties of the nodes a one-column query returns.
    async fn query(&self, cypher: &str, machine: &str) -> Result<Vec<Map<String, Value>>, RpcError> {
        let mut params = Map::new();
        params.insert("machine".into(), Value::String(machine.to_owned()));
        let query = GraphQueryParams { cypher: cypher.to_owned(), params, limit: Some(5000) };
        let answer = self.call_until_served(methods::GRAPH_QUERY, serde_json::to_value(&query).unwrap_or(Value::Null)).await?;
        let result: GraphQueryResult = serde_json::from_value(answer).map_err(|error| RpcError::internal(format!("memory.graph.query: {error}")))?;
        Ok(result
            .rows
            .into_iter()
            .filter_map(|row| row.into_iter().next())
            .filter_map(|node| node.get("properties").and_then(Value::as_object).cloned())
            .collect())
    }

    fn edge_to_machine(&self, label: &str, key: &str) -> Option<GraphEdge> {
        let machine = self.inner.machine.get()?;
        Some(GraphEdge {
            from: GraphRef { label: label.to_owned(), key: key.to_owned() },
            rel: "ON".into(),
            to: GraphRef { label: "Machine".into(), key: machine.clone() },
            ..GraphEdge::default()
        })
    }

    fn file_node(key: &str, verified: &Verified) -> GraphNode {
        let mut props = Map::new();
        props.insert("bytes".into(), json!(verified.bytes));
        props.insert("modified_ns".into(), json!(verified.modified_ns.to_string()));
        props.insert("digest".into(), json!(verified.digest));
        GraphNode { label: FILE.into(), key: key.to_owned(), props, ..GraphNode::default() }
    }

    /// Put nodes, each with its edge to the machine, in batches.
    async fn put(&self, nodes: Vec<GraphNode>) -> Result<(), RpcError> {
        for chunk in nodes.chunks(PUT_NODES) {
            let edges = chunk.iter().filter_map(|node| self.edge_to_machine(&node.label, &node.key)).collect::<Vec<_>>();
            if edges.len() != chunk.len() {
                return Err(RpcError::new(codes::UNAVAILABLE, "the machine's key is not known yet"));
            }
            let params = GraphPutParams { nodes: chunk.to_vec(), edges, layer: GraphLayer::Observed, at_ms: None };
            self.call(methods::GRAPH_PUT, serde_json::to_value(&params).unwrap_or(Value::Null)).await?;
        }
        Ok(())
    }

    /// Write the records not written yet; a failed write keeps them for the next.
    async fn flush(&self) {
        if *self.inner.memory.borrow() != Memory::Serving {
            return;
        }
        let keys = std::mem::take(&mut *lock(&self.inner.pending));
        if keys.is_empty() {
            return;
        }
        let nodes: Vec<GraphNode> = {
            let digests = lock(&self.inner.digests);
            keys.iter().filter_map(|key| digests.get(key).map(|verified| Self::file_node(key, verified))).collect()
        };
        if let Err(error) = self.put(nodes).await {
            tracing::warn!(error = %error.message, "model file records not written; they are written with the next record");
            lock(&self.inner.pending).extend(keys);
        }
    }

    /// Write through in the background (callers may be on a blocking thread).
    fn kick(&self) {
        if let Some(link) = self.inner.link.get() {
            let cache = self.clone();
            link.tokio.spawn(async move { cache.flush().await });
        }
    }

    /// Move the old record file into the graph, then delete it. Repeating it is harmless: the
    /// puts merge, so both model plugins may try.
    async fn migrate(&self) {
        let old = self.inner.models_dir.join(OLD_RECORD);
        if let Ok(bytes) = std::fs::read(&old) {
            #[derive(Deserialize)]
            struct OldVerified {
                bytes: u64,
                modified_ns: u128,
                sha256: String,
            }
            let records: BTreeMap<String, OldVerified> = serde_json::from_slice(&bytes).unwrap_or_default();
            // A record already in the graph is newer than the file's.
            let known: BTreeSet<String> = lock(&self.inner.digests).keys().cloned().collect();
            let records: BTreeMap<String, Verified> = records
                .into_iter()
                .filter(|(key, _)| !known.contains(key))
                .map(|(key, old)| (key, Verified { bytes: old.bytes, modified_ns: old.modified_ns, digest: normalized(&old.sha256) }))
                .collect();
            let nodes = records.iter().map(|(key, verified)| Self::file_node(key, verified)).collect();
            match self.put(nodes).await {
                Ok(()) => {
                    {
                        let mut digests = lock(&self.inner.digests);
                        for (key, verified) in records {
                            digests.entry(key).or_insert(verified);
                        }
                    }
                    let _ = std::fs::remove_file(&old);
                    remove_temporaries(&self.inner.models_dir);
                    tracing::info!(file = %old.display(), "model file digests moved into the graph");
                }
                Err(error) => tracing::warn!(error = %error.message, file = %old.display(), "model file digests not moved into the graph; tried again at the next start"),
            }
        }
    }

    /// The recorded digest (`kind:hex`) of `path`, if computed for the file as it is now.
    pub fn recorded(&self, path: &Path) -> Option<String> {
        let entry = lock(&self.inner.digests).get(&path.display().to_string()).cloned()?;
        let bytes = std::fs::metadata(path).ok()?.len();
        (entry.bytes == bytes && Some(entry.modified_ns) == modified_ns(path)).then_some(entry.digest)
    }

    /// Record a digest (`kind:hex`) computed for `path` as it is now.
    pub fn record_digest(&self, path: &Path, digest: &str) {
        let (Ok(metadata), Some(modified)) = (std::fs::metadata(path), modified_ns(path)) else { return };
        let key = path.display().to_string();
        lock(&self.inner.digests).insert(key.clone(), Verified { bytes: metadata.len(), modified_ns: modified, digest: digest.to_owned() });
        lock(&self.inner.pending).insert(key);
        self.kick();
    }

    /// Hash the file (or reuse the record) and compare with the catalog. `on_progress(done, total)`.
    pub fn verify(&self, file: &variants::FileSpec, path: &Path, on_progress: &mut dyn FnMut(u64, u64)) -> Result<String, String> {
        let expected = variants::expected_digest(file);
        let digest = match self.recorded(path).filter(|recorded| recorded.split(':').next() == expected.split(':').next()) {
            Some(digest) => digest,
            None => {
                let digest = variants::digest_file(path, file.sha256.is_some(), on_progress)?;
                self.record_digest(path, &digest);
                digest
            }
        };
        if digest == expected { Ok(digest) } else { Err(format!("{} has {digest}, expected {expected}", path.display())) }
    }
}

/// The `.cortexcode-verified.json.<pid>.tmp` files earlier versions left while writing.
fn remove_temporaries(models_dir: &Path) {
    let Ok(entries) = std::fs::read_dir(models_dir) else { return };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with(&format!("{OLD_RECORD}.")) && name.ends_with(".tmp") {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}
