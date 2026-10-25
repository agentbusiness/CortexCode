//! `vcs.status`: `git status --porcelain=v2 -z` plus line counts from `git diff --numstat -z`.

use crate::git::{Git, path_text, split_nul};
use crate::repo::{Repo, canonical_root, discover};
use cc_protocol::RpcError;
use cc_protocol::contracts::vcs::{FileStatus, Status};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Branch header fields of porcelain v2 (`# branch.*`).
#[derive(Debug, Default, Clone, PartialEq)]
pub struct BranchInfo {
    pub oid: Option<String>,
    pub head: Option<String>,
    pub upstream: Option<String>,
    pub ahead: u32,
    pub behind: u32,
}

/// One entry of porcelain v2 output.
#[derive(Debug, Clone, PartialEq)]
pub enum Entry {
    /// `1`/`2`: tracked change; `x` index status, `y` work-tree status (`.` = unchanged).
    Changed { x: u8, y: u8, path: Vec<u8>, orig: Option<Vec<u8>> },
    /// `u`: unmerged.
    Unmerged { path: Vec<u8> },
    /// `?`
    Untracked { path: Vec<u8> },
}

/// Parse `git status --porcelain=v2 --branch -z`. Purely structural (the documented format):
/// fields are space-separated up to the path, which is the rest of the record; renames carry the
/// original path in the next NUL-terminated record.
pub fn parse_porcelain_v2(data: &[u8]) -> (BranchInfo, Vec<Entry>) {
    let records = split_nul(data);
    let mut branch = BranchInfo::default();
    let mut entries = Vec::new();
    let mut i = 0;
    while i < records.len() {
        let record = records[i];
        i += 1;
        if let Some(rest) = record.strip_prefix(b"# ") {
            let text = String::from_utf8_lossy(rest);
            if let Some(value) = text.strip_prefix("branch.oid ") {
                branch.oid = (value != "(initial)").then(|| value.to_owned());
            } else if let Some(value) = text.strip_prefix("branch.head ") {
                branch.head = (value != "(detached)").then(|| value.to_owned());
            } else if let Some(value) = text.strip_prefix("branch.upstream ") {
                branch.upstream = Some(value.to_owned());
            } else if let Some(value) = text.strip_prefix("branch.ab ") {
                for part in value.split(' ') {
                    if let Some(n) = part.strip_prefix('+') {
                        branch.ahead = n.parse().unwrap_or(0);
                    } else if let Some(n) = part.strip_prefix('-') {
                        branch.behind = n.parse().unwrap_or(0);
                    }
                }
            }
            continue;
        }
        let kind = record.first().copied();
        match kind {
            Some(b'1') => {
                // 1 XY sub mH mI mW hH hI path
                let fields: Vec<&[u8]> = record.splitn(9, |b| *b == b' ').collect();
                if let (Some(xy), Some(path)) = (fields.get(1), fields.get(8)) {
                    entries.push(Entry::Changed { x: xy[0], y: xy[1], path: path.to_vec(), orig: None });
                }
            }
            Some(b'2') => {
                // 2 XY sub mH mI mW hH hI Xscore path NUL origPath
                let fields: Vec<&[u8]> = record.splitn(10, |b| *b == b' ').collect();
                let orig = records.get(i).map(|r| r.to_vec());
                i += 1;
                if let (Some(xy), Some(path)) = (fields.get(1), fields.get(9)) {
                    entries.push(Entry::Changed { x: xy[0], y: xy[1], path: path.to_vec(), orig });
                }
            }
            Some(b'u') => {
                // u XY sub m1 m2 m3 mW h1 h2 h3 path
                let fields: Vec<&[u8]> = record.splitn(11, |b| *b == b' ').collect();
                if let Some(path) = fields.get(10) {
                    entries.push(Entry::Unmerged { path: path.to_vec() });
                }
            }
            Some(b'?') => {
                if let Some(path) = record.get(2..) {
                    entries.push(Entry::Untracked { path: path.to_vec() });
                }
            }
            _ => {}
        }
    }
    (branch, entries)
}

/// Parse `git diff --numstat -z`: `added\tremoved\tpath\0`, or for renames
/// `added\tremoved\t\0src\0dst\0`. Binary files report `-` (counted as 0). Keyed by (new) path.
pub fn parse_numstat(data: &[u8]) -> HashMap<Vec<u8>, (u32, u32)> {
    let records = split_nul(data);
    let mut out = HashMap::new();
    let mut i = 0;
    while i < records.len() {
        let record = records[i];
        i += 1;
        let mut parts = record.splitn(3, |b| *b == b'\t');
        let (Some(added), Some(removed), Some(path)) = (parts.next(), parts.next(), parts.next()) else {
            continue;
        };
        let count = |field: &[u8]| std::str::from_utf8(field).ok().and_then(|s| s.parse::<u32>().ok()).unwrap_or(0);
        let counts = (count(added), count(removed));
        let path = if path.is_empty() {
            // Rename: source then destination follow as separate records.
            let dst = records.get(i + 1).map(|r| r.to_vec());
            i += 2;
            match dst {
                Some(dst) => dst,
                None => continue,
            }
        } else {
            path.to_vec()
        };
        let entry = out.entry(path).or_insert((0, 0));
        entry.0 += counts.0;
        entry.1 += counts.1;
    }
    out
}

fn status_word(code: u8) -> &'static str {
    match code {
        b'A' | b'C' => "added",
        b'D' => "deleted",
        b'R' => "renamed",
        b'U' => "conflicted",
        _ => "modified",
    }
}

/// A copy of the repository's index with every given untracked path added as intent-to-add, so
/// `git diff` (with `GIT_INDEX_FILE` pointing here) shows those files as new files exactly as git
/// computes them (attributes, binary detection) without touching the user's index.
pub struct IntentIndex {
    pub path: PathBuf,
    objects: PathBuf,
    alternates: String,
}

impl IntentIndex {
    pub fn command(&self, git: &Git, repo: &Repo) -> crate::git::Cmd {
        git.cmd(&repo.top)
            .env("GIT_INDEX_FILE", &self.path)
            .env("GIT_OBJECT_DIRECTORY", &self.objects)
            .env("GIT_ALTERNATE_OBJECT_DIRECTORIES", &self.alternates)
    }
}

impl Drop for IntentIndex {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
        let mut lock = self.path.clone().into_os_string();
        lock.push(".lock");
        let _ = std::fs::remove_file(PathBuf::from(lock));
        let _ = std::fs::remove_dir_all(&self.objects);
    }
}

pub async fn intent_index(git: &Git, repo: &Repo, scratch: &Path, untracked: &[Vec<u8>]) -> Result<IntentIndex, RpcError> {
    std::fs::create_dir_all(scratch)
        .map_err(|error| RpcError::internal(format!("cannot create {}: {error}", scratch.display())))?;
    let id = uuid::Uuid::new_v4().simple().to_string();
    let objects = scratch.join(format!("objects-{id}"));
    std::fs::create_dir_all(&objects).map_err(|error| RpcError::internal(format!("cannot create scratch objects: {error}")))?;
    // Git's C-quoted alternate syntax preserves spaces, colons and backslashes in repository paths.
    let alternates = serde_json::to_string(&repo.common_dir.join("objects").to_string_lossy())
        .map_err(|error| RpcError::internal(error.to_string()))?;
    let index = IntentIndex { path: scratch.join(format!("index-{id}")), objects, alternates };
    let source = repo.git_dir.join("index");
    if source.exists() {
        std::fs::copy(&source, &index.path)
            .map_err(|error| RpcError::internal(format!("cannot copy the index: {error}")))?;
    }
    if !untracked.is_empty() {
        let mut list = Vec::new();
        for path in untracked {
            list.extend_from_slice(path);
            list.push(0);
        }
        index.command(&git.confined(scratch), repo)
            .args(["add", "--intent-to-add", "--pathspec-from-file=-", "--pathspec-file-nul"])
            .stdin(list)
            .ok()
            .await?;
    }
    Ok(index)
}

/// Untracked paths of the working tree (respecting ignore rules), from porcelain status.
pub async fn untracked_paths(git: &Git, repo: &Repo) -> Result<Vec<Vec<u8>>, RpcError> {
    let out = git.run(&repo.top, ["status", "--porcelain=v2", "-z", "--untracked-files=all"]).await?;
    let (_, entries) = parse_porcelain_v2(&out);
    Ok(entries
        .into_iter()
        .filter_map(|entry| match entry {
            Entry::Untracked { path } => Some(path),
            _ => None,
        })
        .collect())
}

pub async fn status(git: &Git, scratch: &Path, root: &str) -> Result<Status, RpcError> {
    Ok(status_with_repo(git, scratch, root).await?.0)
}

/// Status plus the repository it was computed for (`None` outside a working tree).
pub async fn status_with_repo(
    git: &Git,
    scratch: &Path,
    root: &str,
) -> Result<(Status, Option<Repo>), RpcError> {
    std::fs::create_dir_all(scratch).map_err(|error| {
        RpcError::internal(format!("cannot create Git inspection scratch: {error}"))
    })?;
    let confined = git.confined(scratch);
    let git = &confined;
    let canonical = canonical_root(root)?;
    let Some(repo) = discover(git, &canonical).await? else {
        return Ok((Status {
            root: root.to_owned(),
            is_repo: false,
            branch: String::new(),
            upstream: None,
            ahead: 0,
            behind: 0,
            files: Vec::new(),
            added_lines: 0,
            removed_lines: 0,
            head: None,
            detached: false,
        }, None));
    };
    // Independent reads run concurrently (each git process costs a few milliseconds to start).
    let staged_args = ["diff", "--cached", "--numstat", "-z", "--find-renames", "--no-ext-diff", "--no-textconv"];
    let (out, staged_out) = tokio::try_join!(
        git.run(&repo.top, ["status", "--porcelain=v2", "--branch", "-z", "--untracked-files=all", "--find-renames"]),
        git.run(&repo.top, staged_args),
    )?;
    let (branch, entries) = parse_porcelain_v2(&out);
    let untracked: Vec<Vec<u8>> = entries
        .iter()
        .filter_map(|entry| match entry {
            Entry::Untracked { path } => Some(path.clone()),
            _ => None,
        })
        .collect();

    let staged_counts = parse_numstat(&staged_out);
    let worktree_args = ["diff", "--numstat", "-z", "--no-renames", "--no-ext-diff", "--no-textconv"];
    let worktree_counts = if untracked.is_empty() {
        parse_numstat(&git.run(&repo.top, worktree_args).await?)
    } else {
        // Untracked files enter a scratch copy of the index as intent-to-add entries, so they are
        // counted exactly like git counts a new file (attributes, binary detection).
        let index = intent_index(git, &repo, scratch, &untracked).await?;
        parse_numstat(&index.command(git, &repo).args(worktree_args).ok().await?)
    };
    // A conflicted file is counted against HEAD (its index entries are the unmerged stages).
    let unmerged: Vec<Vec<u8>> = entries
        .iter()
        .filter_map(|entry| match entry {
            Entry::Unmerged { path } => Some(path.clone()),
            _ => None,
        })
        .collect();
    let conflict_counts = if unmerged.is_empty() {
        HashMap::new()
    } else {
        use std::os::unix::ffi::OsStrExt;
        let base = repo.head_or_empty(git).await?;
        parse_numstat(
            &git.cmd(&repo.top)
                .args(["diff", "--numstat", "-z", "--no-renames", "--no-ext-diff", "--no-textconv", base.as_str(), "--"])
                .args(unmerged.iter().map(|p| std::ffi::OsStr::from_bytes(p).to_os_string()))
                .ok()
                .await?,
        )
    };

    let mut files = Vec::new();
    for entry in entries {
        match entry {
            Entry::Changed { x, y, path, orig } => {
                if x != b'.' {
                    let (added, removed) = staged_counts.get(&path).copied().unwrap_or((0, 0));
                    files.push(FileStatus {
                        path: path_text(&path),
                        status: status_word(x).into(),
                        staged: true,
                        added,
                        removed,
                        old_path: if matches!(x, b'R' | b'C') { orig.as_deref().map(path_text) } else { None },
                    });
                }
                if y != b'.' {
                    let (mut added, mut removed) = worktree_counts.get(&path).copied().unwrap_or((0, 0));
                    let old_path = if matches!(y, b'R' | b'C') { orig.as_deref().map(path_text) } else { None };
                    if y == b'R'
                        && let Some(orig) = &orig
                        && let Some((a, r)) = worktree_counts.get(orig)
                    {
                        // Counted without rename detection: the removal side lives under the old path.
                        added += a;
                        removed += r;
                    }
                    files.push(FileStatus {
                        path: path_text(&path),
                        status: status_word(y).into(),
                        staged: false,
                        added,
                        removed,
                        old_path,
                    });
                }
            }
            Entry::Unmerged { path } => {
                let (added, removed) = conflict_counts.get(&path).copied().unwrap_or((0, 0));
                files.push(FileStatus {
                    path: path_text(&path),
                    status: "conflicted".into(),
                    staged: false,
                    added,
                    removed,
                    old_path: None,
                });
            }
            Entry::Untracked { path } => {
                let (added, removed) = worktree_counts.get(&path).copied().unwrap_or((0, 0));
                files.push(FileStatus {
                    path: path_text(&path),
                    status: "untracked".into(),
                    staged: false,
                    added,
                    removed,
                    old_path: None,
                });
            }
        }
    }
    let added_lines = files.iter().map(|f| f.added).sum();
    let removed_lines = files.iter().map(|f| f.removed).sum();
    let detached = branch.head.is_none();
    let status = Status {
        root: root.to_owned(),
        is_repo: true,
        branch: branch.head.clone().unwrap_or_else(|| "HEAD".into()),
        upstream: branch.upstream,
        ahead: branch.ahead,
        behind: branch.behind,
        files,
        added_lines,
        removed_lines,
        head: branch.oid,
        detached,
    };
    Ok((status, Some(repo)))
}
