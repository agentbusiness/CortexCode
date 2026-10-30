//! Checkpoints of a working tree in a shadow repository.
//!
//! The shadow repository lives at `<plugin data>/shadow/<sha256(root)>` and is used with an
//! explicit `--git-dir` (the shadow) and `--work-tree` (the root). It has its own index, objects
//! and refs, so snapshotting and restoring never reads or writes anything in the user's `.git`
//! except its `info/exclude` (read only, so the shadow ignores exactly what the user's repo ignores).
//! git never stores or writes a path component named `.git`, so the user's repository directory is
//! outside everything the shadow tracks.
//!
//! A checkpoint is a commit in the shadow (chained to the previous one) kept reachable by
//! `refs/checkpoints/<id>`. What it is (label, time, commit) lives in the app's graph:
//!
//! ```text
//! (:Checkpoint {key: id, label, ts_ms, commit, tree, root})-[:OF]->(:Workspace {key: canonical root})
//! (:Checkpoint)-[:AFTER]->(:Checkpoint)          the one made before it in the same root
//! (:Workspace)-[:ON]->(:Machine)
//! stream `checkpoints`: {id, root, label, ts_ms, commit, tree}, one record per checkpoint
//! ```
//!
//! The nodes are in the `workspace` layer and written through the plugin's queue; the checkpoints
//! this process made are also kept in memory, so they list and restore while the graph is not
//! serving. Roots inside the plugin's own data folder (the probe's scratch repositories) stay in
//! memory only. The `checkpoints.json` earlier versions kept is moved into the graph once and
//! removed.
//!
//! Files that stay: the shadow repository itself (git objects, refs, index: bytes) and
//! `cortex.lock`, an OS lock that serializes operations on one root across plugin instances
//! (hot-swap).

use crate::git::{Cmd, Git};
use crate::graph::{Graph, Wait, edge, machine_ref, node, node_ref, workspace_batch};
use crate::repo::{canonical_root, discover};
use cc_protocol::contracts::now_ms;
use cc_protocol::contracts::vcs::{Checkpoint, CheckpointsResult, RestoreResult};
use cc_protocol::{RpcError, codes};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

const IDENTITY_NAME: &str = "CortexCode checkpoint";
const IDENTITY_EMAIL: &str = "checkpoint@cortexcode.invalid";
/// The file earlier versions kept the checkpoints in, inside each shadow repository.
const OLD_META: &str = "checkpoints.json";
const TOPIC: &str = "checkpoints";

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Meta {
    id: String,
    label: String,
    ts_ms: u64,
    commit: String,
    tree: String,
}

/// `checkpoints.json` of earlier versions.
#[derive(Debug, Default, Deserialize)]
struct MetaFile {
    #[serde(default)]
    root: String,
    #[serde(default)]
    checkpoints: Vec<Meta>,
}

impl Meta {
    fn from_props(props: &Map<String, Value>) -> Option<Self> {
        let text = |key: &str| props.get(key).and_then(Value::as_str).map(str::to_owned);
        let ts_ms = props.get("ts_ms").and_then(|v| v.as_u64().or_else(|| v.as_f64().map(|f| f.max(0.0) as u64)))?;
        Some(Self { id: text("key")?, label: text("label").unwrap_or_default(), ts_ms, commit: text("commit")?, tree: text("tree").unwrap_or_default() })
    }

    fn record(&self, root: &str) -> Value {
        json!({ "id": self.id, "root": root, "label": self.label, "ts_ms": self.ts_ms, "commit": self.commit, "tree": self.tree })
    }
}

/// The batch that writes `checkpoints` of `root` (oldest first), each after `previous` and then
/// after the one before it.
fn batch(machine: &str, root: &str, previous: Option<&str>, checkpoints: &[Meta]) -> cc_protocol::contracts::memory::GraphPutParams {
    let workspace = node_ref("Workspace", root);
    let mut nodes = vec![node("Workspace", root, Map::new())];
    let mut edges = vec![edge(workspace.clone(), "ON", machine_ref(machine))];
    let mut before = previous.map(str::to_owned);
    for meta in checkpoints {
        let mut props = Map::new();
        props.insert("label".into(), json!(meta.label));
        props.insert("ts_ms".into(), json!(meta.ts_ms));
        props.insert("commit".into(), json!(meta.commit));
        props.insert("tree".into(), json!(meta.tree));
        props.insert("root".into(), json!(root));
        nodes.push(node("Checkpoint", &meta.id, props));
        let this = node_ref("Checkpoint", &meta.id);
        edges.push(edge(this.clone(), "OF", workspace.clone()));
        if let Some(before) = &before {
            edges.push(edge(this, "AFTER", node_ref("Checkpoint", before)));
        }
        before = Some(meta.id.clone());
    }
    workspace_batch(nodes, edges)
}

/// Checkpoint records: the graph, plus what this process made (by canonical root).
pub struct Checkpoints {
    graph: Arc<Graph>,
    data_dir: PathBuf,
    made: Mutex<HashMap<String, Vec<Meta>>>,
}

impl Checkpoints {
    pub fn new(graph: Arc<Graph>, data_dir: &Path) -> Self {
        Self { graph, data_dir: data_dir.to_path_buf(), made: Mutex::new(HashMap::new()) }
    }

    fn made(&self) -> std::sync::MutexGuard<'_, HashMap<String, Vec<Meta>>> {
        self.made.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// A root inside the plugin's own data folder: a scratch repository, never written to the graph.
    fn scratch(&self, root: &Path) -> bool {
        root.starts_with(&self.data_dir)
    }

    /// The checkpoints of `root`, oldest first.
    async fn of(&self, root: &str, scratch: bool) -> Result<Vec<Meta>, RpcError> {
        let mut all: Vec<Meta> = self.made().get(root).cloned().unwrap_or_default();
        if !scratch {
            let cypher = "MATCH (c:Checkpoint)-[:OF]->(:Workspace {key: $root}) WHERE c.ended_ms IS NULL RETURN c ORDER BY c.ts_ms DESC LIMIT 5000";
            match self.graph.nodes(cypher, json!({ "root": root }), Wait::Bounded).await {
                Ok(found) => {
                    for meta in found.iter().filter_map(Meta::from_props) {
                        if !all.iter().any(|known| known.id == meta.id) {
                            all.push(meta);
                        }
                    }
                }
                Err(error) if error.code == codes::UNAVAILABLE => {
                    tracing::warn!(error = %error.message, root, "memory is not serving; listing only this run's checkpoints");
                }
                Err(error) => return Err(error),
            }
        }
        all.sort_by(|a, b| a.ts_ms.cmp(&b.ts_ms).then_with(|| a.id.cmp(&b.id)));
        Ok(all)
    }

    /// Keep a new checkpoint: in memory now, in the graph and the stream through the queue.
    fn keep(&self, root: &str, scratch: bool, previous: Option<&str>, meta: &Meta) {
        self.made().entry(root.to_owned()).or_default().push(meta.clone());
        if scratch {
            return;
        }
        let (root_key, previous, entry) = (root.to_owned(), previous.map(str::to_owned), meta.clone());
        self.graph.put_later(move |machine| batch(machine, &root_key, previous.as_deref(), std::slice::from_ref(&entry)));
        self.graph.append_later(TOPIC, vec![meta.record(root)]);
    }

    /// Move one shadow repository's `checkpoints.json` into the graph, then remove it. The caller
    /// holds the shadow's lock.
    async fn migrate(&self, dir: &Path, wait: Wait) -> Result<(), RpcError> {
        let path = dir.join(OLD_META);
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(io("cannot read the old checkpoints file", error)),
        };
        let old: MetaFile = serde_json::from_slice(&bytes)
            .map_err(|e| RpcError::internal(format!("{} is not valid JSON ({e}); move or remove it", path.display())))?;
        if !old.root.is_empty() && !old.checkpoints.is_empty() && !self.scratch(Path::new(&old.root)) {
            let root = old.root.clone();
            let checkpoints = old.checkpoints.clone();
            for chunk in checkpoints.chunks(1000) {
                let previous = checkpoints.iter().take_while(|m| m.id != chunk[0].id).last().map(|m| m.id.clone());
                self.graph.put(|machine| batch(machine, &root, previous.as_deref(), chunk), wait).await?;
            }
            let ids: Vec<&str> = checkpoints.iter().map(|m| m.id.as_str()).collect();
            let rows = self.graph.query("MATCH (c:Checkpoint) WHERE c.key IN $ids RETURN count(c)", json!({ "ids": ids }), wait).await?;
            let stored = rows.first().and_then(|row| row.first()).and_then(Value::as_u64).unwrap_or(0);
            if stored < ids.len() as u64 {
                return Err(RpcError::internal(format!("memory kept {stored} of {} checkpoints from {}; the file stays", ids.len(), path.display())));
            }
            for chunk in checkpoints.chunks(1000) {
                self.graph.append(TOPIC, chunk.iter().map(|m| m.record(&root)).collect(), wait).await?;
            }
            tracing::info!(root, count = ids.len(), "checkpoints moved into the graph");
        }
        remove_old_meta(dir);
        Ok(())
    }

    /// Move every shadow repository's `checkpoints.json` into the graph (at start).
    pub async fn migrate_all(&self) {
        let Ok(entries) = std::fs::read_dir(self.data_dir.join("shadow")) else { return };
        for entry in entries.flatten() {
            let dir = entry.path();
            if !dir.join(OLD_META).exists() {
                continue;
            }
            let outcome = async {
                let _lock = lock_dir(&dir).await?;
                self.migrate(&dir, Wait::Until).await
            }
            .await;
            if let Err(error) = outcome {
                tracing::error!(dir = %dir.display(), error = %error.message, "the old checkpoints file could not be moved into the graph");
            }
        }
    }
}

/// Remove `checkpoints.json` and the temporary files its writes left.
fn remove_old_meta(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name == OLD_META || name.starts_with("checkpoints.json.") {
            if let Err(error) = std::fs::remove_file(entry.path()) {
                tracing::warn!(file = %entry.path().display(), %error, "cannot remove the old checkpoints file");
            }
        }
    }
}

/// Exclusive lock over a shadow repository for the duration of one operation.
async fn lock_dir(dir: &Path) -> Result<std::fs::File, RpcError> {
    std::fs::create_dir_all(dir).map_err(|e| io("cannot create the shadow repository", e))?;
    let path = dir.join("cortex.lock");
    tokio::task::spawn_blocking(move || {
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
            .map_err(|e| io("cannot open the checkpoint lock", e))?;
        file.lock().map_err(|e| io("cannot lock the shadow repository", e))?;
        Ok(file)
    })
    .await
    .map_err(|e| RpcError::internal(format!("lock task failed: {e}")))?
}

/// The shadow repository of one root.
pub struct Shadow {
    git: Git,
    store: Arc<Checkpoints>,
    root: PathBuf,
    /// The canonical root: the `Workspace` key.
    root_key: String,
    root_text: String,
    dir: PathBuf,
    scratch: bool,
}

/// Directory of the shadow repository for a canonical root.
pub fn shadow_dir(data_dir: &Path, root: &Path) -> PathBuf {
    let digest = Sha256::digest(root.as_os_str().as_encoded_bytes());
    let hex: String = digest.iter().take(12).map(|b| format!("{b:02x}")).collect();
    data_dir.join("shadow").join(hex)
}

fn io(context: &str, error: std::io::Error) -> RpcError {
    RpcError::internal(format!("{context}: {error}"))
}

impl Shadow {
    pub fn new(git: &Git, store: &Arc<Checkpoints>, root: &str) -> Result<Self, RpcError> {
        let canonical = canonical_root(root)?;
        let dir = shadow_dir(&store.data_dir, &canonical);
        let scratch = store.scratch(&canonical);
        let root_key = canonical.to_string_lossy().into_owned();
        Ok(Self { git: git.clone(), store: store.clone(), root: canonical, root_key, root_text: root.to_owned(), dir, scratch })
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// A git command against the shadow repository and the root's working tree.
    fn cmd(&self) -> Cmd {
        let mut git_dir = std::ffi::OsString::from("--git-dir=");
        git_dir.push(&self.dir);
        let mut work_tree = std::ffi::OsString::from("--work-tree=");
        work_tree.push(&self.root);
        self.git
            .cmd(&self.root)
            .arg(git_dir)
            .arg(work_tree)
            .args(["-c", "core.hooksPath=/dev/null", "-c", "core.autocrlf=false", "-c", "core.safecrlf=false"])
            .args(["-c", "core.fsmonitor=false", "-c", "advice.addEmbeddedRepo=false", "-c", "gc.autoDetach=false"])
            .args(["-c", "commit.gpgSign=false", "-c", "core.symlinks=true"])
            .env("GIT_AUTHOR_NAME", IDENTITY_NAME)
            .env("GIT_AUTHOR_EMAIL", IDENTITY_EMAIL)
            .env("GIT_COMMITTER_NAME", IDENTITY_NAME)
            .env("GIT_COMMITTER_EMAIL", IDENTITY_EMAIL)
    }

    async fn ensure(&self) -> Result<(), RpcError> {
        if self.dir.join("HEAD").exists() {
            return Ok(());
        }
        std::fs::create_dir_all(&self.dir).map_err(|e| io("cannot create the shadow repository", e))?;
        self.git
            .cmd(&self.dir)
            .args(["init", "--quiet", "--bare"])
            .arg(&self.dir)
            .env("GIT_TEMPLATE_DIR", "")
            .ok()
            .await?;
        for (key, value) in [("core.bare", "false"), ("gc.auto", "6700"), ("core.logAllRefUpdates", "false")] {
            self.git.cmd(&self.dir).arg(format!("--git-dir={}", self.dir.display())).args(["config", key, value]).ok().await?;
        }
        Ok(())
    }

    /// Exclusive lock over this shadow repository for the duration of one operation; the
    /// checkpoints file of an earlier version is moved into the graph first.
    async fn lock(&self) -> Result<std::fs::File, RpcError> {
        let file = lock_dir(&self.dir).await?;
        if self.dir.join(OLD_META).exists()
            && let Err(error) = self.store.migrate(&self.dir, Wait::Bounded).await
        {
            tracing::warn!(dir = %self.dir.display(), error = %error.message, "the old checkpoints file stays until memory takes it");
        }
        Ok(file)
    }

    /// Mirror the user's `info/exclude` so the shadow ignores what the user's repository ignores
    /// (`.gitignore` files and the global excludes file apply to both automatically).
    async fn sync_excludes(&self) -> Result<(), RpcError> {
        let mut content = Vec::new();
        if let Some(repo) = discover(&self.git, &self.root).await? {
            if let Ok(bytes) = std::fs::read(repo.common_dir.join("info").join("exclude")) {
                content = bytes;
            }
            if repo.top != self.root {
                // The root is a subdirectory of a working tree: patterns are relative to the
                // user's top level, which this shadow cannot express; skip rather than mis-apply.
                content.clear();
            }
        }
        let info = self.dir.join("info");
        std::fs::create_dir_all(&info).map_err(|e| io("cannot write shadow excludes", e))?;
        let target = info.join("exclude");
        if std::fs::read(&target).ok().as_deref() != Some(content.as_slice()) {
            std::fs::write(&target, content).map_err(|e| io("cannot write shadow excludes", e))?;
        }
        Ok(())
    }

    /// Record the current working tree in the shadow index and return its tree id.
    async fn snapshot_tree(&self) -> Result<String, RpcError> {
        self.sync_excludes().await?;
        // --ignore-errors: an unreadable file (permissions, a socket) must not prevent checkpoints
        // of everything else; whatever could be read is recorded.
        let output = self.cmd().args(["add", "--all", "--ignore-errors", "--", "."]).output().await?;
        if !output.success() {
            tracing::warn!(stderr = %output.stderr_text(), root = %self.root.display(), "checkpoint skipped unreadable files");
        }
        let tree = self.cmd().args(["write-tree"]).ok().await?;
        Ok(String::from_utf8_lossy(&tree).trim().to_owned())
    }

    async fn record(&self, label: &str) -> Result<Meta, RpcError> {
        let tree = self.snapshot_tree().await?;
        let known = self.store.of(&self.root_key, self.scratch).await?;
        let previous = known.last();
        let ts_ms = now_ms();
        let id = format!("cp-{ts_ms}-{}", &uuid::Uuid::new_v4().simple().to_string()[..8]);
        let mut cmd = self.cmd().args(["commit-tree", tree.as_str()]);
        if let Some(previous) = previous {
            cmd = cmd.args(["-p", previous.commit.as_str()]);
        }
        let message = format!("{label}\n\nCortexCode-Checkpoint: {id}\n");
        let commit = cmd.args(["-F", "-"]).stdin(message.into_bytes()).ok().await?;
        let commit = String::from_utf8_lossy(&commit).trim().to_owned();
        let reference = format!("refs/checkpoints/{id}");
        self.cmd().args(["update-ref", reference.as_str(), commit.as_str()]).ok().await?;
        let entry = Meta { id, label: label.to_owned(), ts_ms, commit, tree };
        self.store.keep(&self.root_key, self.scratch, previous.map(|p| p.id.as_str()), &entry);
        // Keep the shadow compact; runs only past git's loose-object threshold.
        let _ = self.cmd().args(["gc", "--auto", "--quiet"]).output().await;
        Ok(entry)
    }

    fn contract(&self, meta: &Meta) -> Checkpoint {
        Checkpoint { id: meta.id.clone(), root: self.root_text.clone(), label: meta.label.clone(), ts_ms: meta.ts_ms }
    }

    pub async fn checkpoint(&self, label: &str) -> Result<Checkpoint, RpcError> {
        let _lock = self.lock().await?;
        self.ensure().await?;
        let meta = self.record(label).await?;
        Ok(self.contract(&meta))
    }

    /// Newest first.
    pub async fn list(&self) -> Result<CheckpointsResult, RpcError> {
        if self.dir.join(OLD_META).exists() {
            drop(self.lock().await?);
        }
        let known = self.store.of(&self.root_key, self.scratch).await?;
        let mut checkpoints: Vec<Checkpoint> = known.iter().map(|m| self.contract(m)).collect();
        checkpoints.reverse();
        Ok(CheckpointsResult { checkpoints })
    }

    /// Put the working tree back to checkpoint `id`: files it had are written back, files created
    /// since (and not ignored) are removed, ignored files are left alone. The state just before is
    /// checkpointed first, so a restore can itself be undone.
    pub async fn restore(&self, id: &str) -> Result<RestoreResult, RpcError> {
        let _lock = self.lock().await?;
        self.ensure().await?;
        let known = self.store.of(&self.root_key, self.scratch).await?;
        let target = known
            .iter()
            .find(|m| m.id == id)
            .cloned()
            .ok_or_else(|| RpcError::new(codes::NOT_FOUND, format!("no checkpoint `{id}` for {}", self.root_text)))?;
        // The backup leaves the shadow index equal to the current working tree, which is what
        // `read-tree -u` needs to know which files exist now and must go.
        let backup = self.record(&format!("before restoring \"{}\"", target.label)).await?;
        self.cmd().args(["read-tree", "-u", "--reset", target.commit.as_str()]).ok().await?;
        Ok(RestoreResult { restored: self.contract(&target), backup: self.contract(&backup) })
    }
}
