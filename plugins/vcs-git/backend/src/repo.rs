//! Locating a repository from a directory.

use crate::git::Git;
use cc_protocol::{RpcError, codes};
use std::path::{Path, PathBuf};

/// A git working tree.
#[derive(Debug, Clone)]
pub struct Repo {
    /// The directory the caller named (canonicalized).
    pub root: PathBuf,
    /// Top level of the working tree containing `root`; every git command runs here and every path
    /// in results is relative to it.
    pub top: PathBuf,
    /// This working tree's git directory (holds HEAD and index).
    pub git_dir: PathBuf,
    /// The common git directory (refs, objects; differs from `git_dir` in linked worktrees).
    pub common_dir: PathBuf,
}

/// Canonicalize a caller-supplied root; it must be an existing directory.
pub fn canonical_root(root: &str) -> Result<PathBuf, RpcError> {
    if root.is_empty() {
        return Err(RpcError::invalid_params("`root` is empty"));
    }
    let path = Path::new(root);
    if !path.is_absolute() {
        return Err(RpcError::invalid_params(format!("`root` must be an absolute path, got `{root}`")));
    }
    let canonical = std::fs::canonicalize(path)
        .map_err(|error| RpcError::new(codes::NOT_FOUND, format!("cannot open `{root}`: {error}")))?;
    if !canonical.is_dir() {
        return Err(RpcError::invalid_params(format!("`{root}` is not a directory")));
    }
    Ok(canonical)
}

/// The repository whose working tree contains `root`, or `None` when `root` is not inside one
/// (bare repositories have no working tree and count as none).
pub async fn discover(git: &Git, root: &Path) -> Result<Option<Repo>, RpcError> {
    let output = git
        .cmd(root)
        .args(["rev-parse", "--path-format=absolute", "--show-toplevel", "--absolute-git-dir", "--git-common-dir"])
        .output()
        .await?;
    if !output.success() {
        return Ok(None);
    }
    let text = String::from_utf8_lossy(&output.stdout).into_owned();
    let mut lines = text.lines();
    let (Some(top), Some(git_dir), Some(common)) = (lines.next(), lines.next(), lines.next()) else {
        return Ok(None);
    };
    if top.is_empty() {
        return Ok(None);
    }
    Ok(Some(Repo {
        root: root.to_path_buf(),
        top: PathBuf::from(top),
        git_dir: PathBuf::from(git_dir),
        common_dir: PathBuf::from(common),
    }))
}

/// Like [`discover`], but a missing repository is an error.
pub async fn require(git: &Git, root: &str) -> Result<Repo, RpcError> {
    let canonical = canonical_root(root)?;
    discover(git, &canonical)
        .await?
        .ok_or_else(|| RpcError::new(codes::NOT_FOUND, format!("`{root}` is not inside a git working tree")))
}

impl Repo {
    /// Resolve a path relative to the working tree top (as results report them) to an absolute one.
    pub fn abs(&self, relative: &str) -> PathBuf {
        self.top.join(relative)
    }

    /// The commit HEAD points at, or `None` on an unborn branch.
    pub async fn head(&self, git: &Git) -> Result<Option<String>, RpcError> {
        git.try_text(&self.top, ["rev-parse", "-q", "--verify", "HEAD^{commit}"]).await
    }

    /// The current branch name, or `None` when HEAD is detached.
    pub async fn branch(&self, git: &Git) -> Result<Option<String>, RpcError> {
        git.try_text(&self.top, ["symbolic-ref", "-q", "--short", "HEAD"]).await
    }

    /// Object id of the empty tree in this repository's hash format.
    pub async fn empty_tree(&self, git: &Git) -> Result<String, RpcError> {
        let out = git.cmd(&self.top).args(["hash-object", "-t", "tree", "--stdin"]).stdin(Vec::new()).ok().await?;
        Ok(String::from_utf8_lossy(&out).trim().to_owned())
    }

    /// HEAD, or the empty tree on an unborn branch: the base for "everything not committed".
    pub async fn head_or_empty(&self, git: &Git) -> Result<String, RpcError> {
        match self.head(git).await? {
            Some(head) => Ok(head),
            None => self.empty_tree(git).await,
        }
    }

    /// A merge is in progress (MERGE_HEAD exists).
    pub fn merging(&self) -> bool {
        self.git_dir.join("MERGE_HEAD").exists()
    }
}
