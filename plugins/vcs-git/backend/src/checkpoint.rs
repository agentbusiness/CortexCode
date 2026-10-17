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
//! `refs/checkpoints/<id>`; labels and times live in `checkpoints.json`. Operations on one root are
//! serialized with an exclusive file lock, so concurrent plugin instances (hot-swap) cannot interleave.

use crate::git::{Cmd, Git};
use crate::repo::{canonical_root, discover};
use cc_protocol::contracts::now_ms;
use cc_protocol::contracts::vcs::{Checkpoint, CheckpointsResult, RestoreResult};
use cc_protocol::{RpcError, codes};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

const IDENTITY_NAME: &str = "CortexCode checkpoint";
const IDENTITY_EMAIL: &str = "checkpoint@cortexcode.invalid";

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Meta {
    id: String,
    label: String,
    ts_ms: u64,
    commit: String,
    tree: String,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct MetaFile {
    root: String,
    checkpoints: Vec<Meta>,
}

/// The shadow repository of one root.
pub struct Shadow {
    git: Git,
    root: PathBuf,
    root_text: String,
    dir: PathBuf,
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
    pub fn new(git: &Git, data_dir: &Path, root: &str) -> Result<Self, RpcError> {
        let canonical = canonical_root(root)?;
        let dir = shadow_dir(data_dir, &canonical);
        Ok(Self { git: git.clone(), root: canonical, root_text: root.to_owned(), dir })
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
        let meta = MetaFile { root: self.root.to_string_lossy().into_owned(), checkpoints: Vec::new() };
        self.write_meta(&meta)?;
        Ok(())
    }

    /// Exclusive lock over this shadow repository for the duration of one operation.
    async fn lock(&self) -> Result<std::fs::File, RpcError> {
        std::fs::create_dir_all(&self.dir).map_err(|e| io("cannot create the shadow repository", e))?;
        let path = self.dir.join("cortex.lock");
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

    fn meta_path(&self) -> PathBuf {
        self.dir.join("checkpoints.json")
    }

    fn read_meta(&self) -> Result<MetaFile, RpcError> {
        match std::fs::read(self.meta_path()) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map_err(|e| RpcError::internal(format!("corrupt {}: {e}", self.meta_path().display()))),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(MetaFile::default()),
            Err(error) => Err(io("cannot read checkpoints", error)),
        }
    }

    fn write_meta(&self, meta: &MetaFile) -> Result<(), RpcError> {
        let bytes = serde_json::to_vec_pretty(meta).map_err(|e| RpcError::internal(e.to_string()))?;
        let tmp = self.dir.join(format!("checkpoints.json.{}", uuid::Uuid::new_v4().simple()));
        std::fs::write(&tmp, bytes).map_err(|e| io("cannot write checkpoints", e))?;
        std::fs::rename(&tmp, self.meta_path()).map_err(|e| io("cannot write checkpoints", e))
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
        let mut meta = self.read_meta()?;
        meta.root = self.root.to_string_lossy().into_owned();
        let ts_ms = now_ms();
        let id = format!("cp-{ts_ms}-{}", &uuid::Uuid::new_v4().simple().to_string()[..8]);
        let mut cmd = self.cmd().args(["commit-tree", tree.as_str()]);
        if let Some(previous) = meta.checkpoints.last() {
            cmd = cmd.args(["-p", previous.commit.as_str()]);
        }
        let message = format!("{label}\n\nCortexCode-Checkpoint: {id}\n");
        let commit = cmd.args(["-F", "-"]).stdin(message.into_bytes()).ok().await?;
        let commit = String::from_utf8_lossy(&commit).trim().to_owned();
        let reference = format!("refs/checkpoints/{id}");
        self.cmd().args(["update-ref", reference.as_str(), commit.as_str()]).ok().await?;
        let entry = Meta { id, label: label.to_owned(), ts_ms, commit, tree };
        meta.checkpoints.push(entry.clone());
        self.write_meta(&meta)?;
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
        let meta = self.read_meta()?;
        let mut checkpoints: Vec<Checkpoint> = meta.checkpoints.iter().map(|m| self.contract(m)).collect();
        checkpoints.reverse();
        Ok(CheckpointsResult { checkpoints })
    }

    /// Put the working tree back to checkpoint `id`: files it had are written back, files created
    /// since (and not ignored) are removed, ignored files are left alone. The state just before is
    /// checkpointed first, so a restore can itself be undone.
    pub async fn restore(&self, id: &str) -> Result<RestoreResult, RpcError> {
        let _lock = self.lock().await?;
        self.ensure().await?;
        let meta = self.read_meta()?;
        let target = meta
            .checkpoints
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
