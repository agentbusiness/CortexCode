//! Borrowing hands: a member runs a session's commands in a copy of the tree it keeps in step
//! (`docs/MESH-IMPLEMENTATION-PLAN.md` §7). The code lives at home; edits and reads stay local.
//!
//! - `mesh.runner` (asked by the tools plugin once per session): the member that runs the
//!   session's commands, chosen from the cards (reachable, `hands` granted, the same tools plugin
//!   at the same version with the same live data, nobody working there, on mains), or none. Before
//!   answering, the member is asked to make its copy ready (`mesh.tree.ready`), which pulls the
//!   tree's files from here (`mesh.tree.files`, streamed) the first time.
//! - Then every step of the home step log for that tree (`tools.timeline` events) is applied to
//!   the copy (`mesh.tree.apply`), and every step the member's commands make in the copy comes
//!   back the same way, into the home tree, as a step attributed to the member.
//!
//! The copy lives under `<home>/mesh/work/<owner>/<project>/`; a `.cortex-copy.json` beside it
//! names the owner and its workspace, so changes find their way back.

use crate::service::Mesh;
use base64::Engine as _;
use cc_protocol::contracts::mesh::{Runner, RunnerParams, TreeApplyParams, TreeFile, TreeFilesParams};
use cc_protocol::contracts::tools::{TimelineApplyFile, TimelineApplyParams, TimelineParams, TimelineResult, methods as tools_methods};
use cc_protocol::{ItemSink, RpcError};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// A copy of a member's tree kept here.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Copy {
    pub key: String,
    pub owner: String,
    pub owner_workspace: String,
    pub path: String,
}

/// A tree of this node that a member runs commands for.
#[derive(Debug, Clone)]
pub struct Lent {
    pub node: String,
    pub key: String,
    pub remote_workspace: String,
    /// The last step of the home log sent to the member.
    pub sent_through: u64,
}

#[derive(Default)]
pub struct Trees {
    /// By home workspace path: the member that runs its commands.
    pub lent: Mutex<HashMap<PathBuf, Lent>>,
    /// By key: copies kept here for members.
    pub copies: Mutex<HashMap<String, Copy>>,
    /// Copies by their path, for the step-log events of this node.
    pub copy_paths: Mutex<HashMap<PathBuf, String>>,
    /// Members this node is about to send a tree to (their `mesh.tree.files` is allowed).
    pub expecting: Mutex<HashMap<String, PathBuf>>,
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn git_program() -> Option<PathBuf> {
    for candidate in ["/opt/homebrew/bin/git", "/usr/local/bin/git", "/usr/bin/git"] {
        if Path::new(candidate).is_file() {
            return Some(PathBuf::from(candidate));
        }
    }
    None
}

/// The files of a tree: what git tracks or would track (ignored files stay home), else every
/// file the ignore rules admit.
fn tree_files(workspace: &Path) -> Vec<PathBuf> {
    if let Some(git) = git_program()
        && workspace.join(".git").exists()
        && let Ok(output) = std::process::Command::new(git).arg("-C").arg(workspace).args(["ls-files", "-co", "--exclude-standard", "-z"]).output()
        && output.status.success()
    {
        return output.stdout.split(|b| *b == 0).filter(|entry| !entry.is_empty()).map(|entry| PathBuf::from(String::from_utf8_lossy(entry).into_owned())).collect();
    }
    let mut files = Vec::new();
    for entry in ignore::WalkBuilder::new(workspace).hidden(false).parents(true).build().flatten() {
        let path = entry.path();
        if path.components().any(|part| part.as_os_str() == ".git") {
            continue;
        }
        if path.is_file()
            && let Ok(relative) = path.strip_prefix(workspace)
        {
            files.push(relative.to_path_buf());
        }
    }
    files
}

fn mode_of(path: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).map(|meta| meta.permissions().mode() & 0o777).unwrap_or(0o644)
}

fn write_file(root: &Path, file: &TreeFile) -> std::io::Result<()> {
    let path = root.join(&file.path);
    if path.strip_prefix(root).is_err() || file.path.contains("..") {
        return Err(std::io::Error::other("a path outside the tree"));
    }
    match &file.data {
        Some(data) => {
            let bytes = base64::engine::general_purpose::STANDARD.decode(data).map_err(std::io::Error::other)?;
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let tmp = path.with_extension(format!("{}.cortex-tmp", path.extension().map(|e| e.to_string_lossy().into_owned()).unwrap_or_default()));
            std::fs::write(&tmp, bytes)?;
            std::fs::rename(&tmp, &path)?;
            if file.mode != 0 {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(file.mode));
            }
            Ok(())
        }
        None => match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        },
    }
}

impl Mesh {
    pub fn work_dir(&self) -> PathBuf {
        self.home.join("mesh").join("work")
    }

    /// Whether a member's tree request is one this node expects or serves.
    pub fn tree_request_allowed(&self, node: &str, method: &str, params: &Value) -> bool {
        match method {
            "mesh.tree.files" => {
                let workspace = params.get("workspace").and_then(Value::as_str).map(PathBuf::from);
                lock(&self.trees.expecting).get(node).is_some_and(|expected| Some(expected) == workspace.as_ref())
            }
            // Any member may ask this node to hold a copy of its tree; running commands in it is
            // what this node's `hands` grant to that member means, and the copy is the member's own.
            "mesh.tree.ready" => true,
            "mesh.tree.apply" => {
                let target = params.get("target").and_then(Value::as_str).unwrap_or("");
                if let Some(key) = target.strip_prefix("copy:") {
                    // Into a copy kept here: only its owner.
                    lock(&self.trees.copies).get(key).is_some_and(|copy| copy.owner == node)
                } else {
                    // Into a tree here: only the member that runs its commands.
                    lock(&self.trees.lent).get(Path::new(target)).is_some_and(|lent| lent.node == node)
                }
            }
            _ => false,
        }
    }

    // --- the borrower's side ---------------------------------------------------------------

    /// Who runs a session's commands: the project's own computer, always (§12: execution stays
    /// where the project, its files and its memory are; the model is what other computers lend).
    pub async fn runner(self: &Arc<Self>, _params: RunnerParams) -> Result<Runner, RpcError> {
        Ok(Runner { node: None, name: self.name(), remote_workspace: String::new(), reason: "commands run on the project's own computer".to_owned() })
    }

    /// The tree's files, streamed to a member making its copy.
    pub async fn tree_files(&self, params: TreeFilesParams, items: ItemSink) -> Result<Value, RpcError> {
        let workspace = PathBuf::from(&params.workspace);
        let files = tokio::task::spawn_blocking({
            let workspace = workspace.clone();
            move || tree_files(&workspace)
        })
        .await
        .map_err(|e| RpcError::internal(e.to_string()))?;
        let mut count = 0u64;
        for relative in files {
            let path = workspace.join(&relative);
            let Ok(bytes) = std::fs::read(&path) else { continue };
            let file = TreeFile { path: relative.to_string_lossy().into_owned(), mode: mode_of(&path), data: Some(base64::engine::general_purpose::STANDARD.encode(&bytes)) };
            if !items.send(serde_json::to_value(file).unwrap_or(Value::Null)) {
                break;
            }
            count += 1;
        }
        Ok(json!({"files": count}))
    }

    /// A step of a home tree a member runs commands for: its files go to the member's copy.
    pub async fn forward_steps(self: &Arc<Self>, workspace: &Path, seq: u64) {
        let Some(lent) = lock(&self.trees.lent).get(workspace).cloned() else { return };
        if seq <= lent.sent_through {
            return;
        }
        let Ok(ctx) = self.ctx() else { return };
        let params = TimelineParams { workspace: workspace.to_string_lossy().into_owned(), since_seq: Some(lent.sent_through), ..TimelineParams::default() };
        let Ok(value) = ctx.call(tools_methods::TIMELINE, serde_json::to_value(params).unwrap_or(Value::Null)).await else { return };
        let Ok(timeline) = serde_json::from_value::<TimelineResult>(value) else { return };
        let member_actor = format!("node:{}", lent.node);
        let mut files: Vec<TreeFile> = Vec::new();
        let mut newest = lent.sent_through;
        for record in timeline.records.iter().rev() {
            newest = newest.max(record.seq);
            // What came from the member is already there.
            if record.actor == member_actor {
                continue;
            }
            for file in &record.files {
                let path = workspace.join(&file.path);
                let data = std::fs::read(&path).ok().map(|bytes| base64::engine::general_purpose::STANDARD.encode(bytes));
                files.retain(|known| known.path != file.path);
                files.push(TreeFile { path: file.path.clone(), mode: if data.is_some() { mode_of(&path) } else { 0 }, data });
            }
        }
        if !files.is_empty() {
            let params = TreeApplyParams { target: format!("copy:{}", lent.key), files, actor: format!("node:{}", self.me.hex()), detail: "from the tree at home".to_owned() };
            if let Err(error) = self.call_member(&lent.node, "mesh.tree.apply", serde_json::to_value(params).unwrap_or(Value::Null), None, CancellationToken::new()).await {
                tracing::warn!(%error, "the member's copy of the tree could not take the latest step");
                return;
            }
        }
        if let Some(lent) = lock(&self.trees.lent).get_mut(workspace) {
            lent.sent_through = newest;
        }
    }

    // --- the lender's side -----------------------------------------------------------------

    /// Make (or find) the copy of a member's tree for `key`, pulling its files the first time.
    pub async fn tree_ready(self: &Arc<Self>, node: &str, params: Value) -> Result<Value, RpcError> {
        let key = params.get("key").and_then(Value::as_str).unwrap_or("").to_owned();
        let owner_workspace = params.get("owner_workspace").and_then(Value::as_str).unwrap_or("").to_owned();
        if key.is_empty() || owner_workspace.is_empty() {
            return Err(RpcError::invalid_params("a key and the owner's workspace are needed"));
        }
        if let Some(copy) = lock(&self.trees.copies).get(&key).cloned()
            && Path::new(&copy.path).is_dir()
        {
            return Ok(json!({"path": copy.path}));
        }
        let path = self.work_dir().join(key.replace(':', "_"));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).map_err(|e| RpcError::internal(e.to_string()))?;
        let (tx, mut rx) = mpsc::unbounded_channel::<Value>();
        let writer = {
            let root = path.clone();
            tokio::spawn(async move {
                let mut written = 0u64;
                while let Some(item) = rx.recv().await {
                    if let Ok(file) = serde_json::from_value::<TreeFile>(item) {
                        if let Err(error) = write_file(&root, &file) {
                            tracing::warn!(%error, path = %file.path, "a file of the copy could not be written");
                        } else {
                            written += 1;
                        }
                    }
                }
                written
            })
        };
        let pulled = self.call_member(node, "mesh.tree.files", json!({"workspace": owner_workspace}), Some(tx), CancellationToken::new()).await;
        let written = writer.await.unwrap_or(0);
        pulled?;
        let copy = Copy { key: key.clone(), owner: node.to_owned(), owner_workspace, path: path.to_string_lossy().into_owned() };
        let _ = std::fs::write(path.join(".cortex-copy.json"), serde_json::to_vec_pretty(&copy).unwrap_or_default());
        lock(&self.trees.copies).insert(key, copy.clone());
        lock(&self.trees.copy_paths).insert(path.clone(), copy.key.clone());
        tracing::info!(files = written, path = %path.display(), "a member's tree was copied here for its commands");
        Ok(json!({"path": copy.path}))
    }

    /// Files from a member: into its copy here, or into the tree here its commands changed.
    pub async fn tree_apply(&self, node: &str, params: TreeApplyParams) -> Result<Value, RpcError> {
        let (workspace, label, detail) = if let Some(key) = params.target.strip_prefix("copy:") {
            let copy = lock(&self.trees.copies).get(key).cloned().ok_or_else(|| RpcError::not_found("no such copy here"))?;
            (copy.path, "sync".to_owned(), params.detail.clone())
        } else {
            let name = self.member(node).map(|member| member.name).unwrap_or_else(|| node[..node.len().min(10)].to_owned());
            (params.target.clone(), "run".to_owned(), format!("{} on {name}", params.detail))
        };
        let files: Vec<TimelineApplyFile> = params.files.into_iter().map(|file| TimelineApplyFile { path: file.path, mode: file.mode, data: file.data }).collect();
        let apply = TimelineApplyParams { workspace, files, actor: format!("node:{node}"), session_id: None, label, detail };
        self.ctx()?.call(tools_methods::TIMELINE_APPLY, serde_json::to_value(apply).unwrap_or(Value::Null)).await?;
        Ok(json!({}))
    }

    /// A step in a copy kept here (a member's command changed files): back to the member's tree.
    pub async fn return_steps(self: &Arc<Self>, copy_path: &Path, seq: u64) {
        let Some(key) = lock(&self.trees.copy_paths).get(copy_path).cloned() else { return };
        let Some(copy) = lock(&self.trees.copies).get(&key).cloned() else { return };
        let Ok(ctx) = self.ctx() else { return };
        let params = TimelineParams { workspace: copy.path.clone(), since_seq: Some(seq.saturating_sub(1)), ..TimelineParams::default() };
        let Ok(value) = ctx.call(tools_methods::TIMELINE, serde_json::to_value(params).unwrap_or(Value::Null)).await else { return };
        let Ok(timeline) = serde_json::from_value::<TimelineResult>(value) else { return };
        let owner_actor = format!("node:{}", copy.owner);
        for record in timeline.records.iter().rev() {
            // What came from the owner, and the copy's first look at itself (every file, as a
            // baseline), is the owner's already; only what commands changed goes back.
            if record.seq != seq || record.actor == owner_actor || record.label == "hand" {
                continue;
            }
            let files: Vec<TreeFile> = record
                .files
                .iter()
                .map(|file| {
                    let path = copy_path.join(&file.path);
                    let data = std::fs::read(&path).ok().map(|bytes| base64::engine::general_purpose::STANDARD.encode(bytes));
                    TreeFile { path: file.path.clone(), mode: if data.is_some() { mode_of(&path) } else { 0 }, data }
                })
                .collect();
            if files.is_empty() {
                continue;
            }
            let params = TreeApplyParams { target: copy.owner_workspace.clone(), files, actor: format!("node:{}", self.me.hex()), detail: record.detail.clone() };
            if let Err(error) = self.call_member(&copy.owner, "mesh.tree.apply", serde_json::to_value(params).unwrap_or(Value::Null), None, CancellationToken::new()).await {
                tracing::warn!(%error, "a command's changes could not be sent back to the member's tree");
            }
        }
    }

    /// Copies recorded on disk from an earlier run of this plugin.
    pub fn load_copies(&self) {
        let Ok(entries) = std::fs::read_dir(self.work_dir()) else { return };
        for entry in entries.flatten() {
            let marker = entry.path().join(".cortex-copy.json");
            if let Ok(bytes) = std::fs::read(&marker)
                && let Ok(copy) = serde_json::from_slice::<Copy>(&bytes)
            {
                lock(&self.trees.copy_paths).insert(PathBuf::from(&copy.path), copy.key.clone());
                lock(&self.trees.copies).insert(copy.key.clone(), copy);
            }
        }
    }
}
