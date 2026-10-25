//! Diff, history, branches, staging, commits and merges on top of the git CLI.

use crate::diff::{ParsedFile, parse_patch};
use crate::git::{Git, command_error, path_text, split_nul};
use crate::repo::Repo;
use crate::status::{intent_index, untracked_paths};
use cc_protocol::contracts::vcs::{
    BranchesResult, Commit, CommitResult, Diff, LogResult, MergeAbortResult, MergeResult,
};
use cc_protocol::{RpcError, codes};
use std::collections::{BTreeSet, HashSet};
use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// Flags that make patch output independent of user diff configuration we cannot parse or apply:
/// external diff drivers, textconv filters, custom/mnemonic/no prefixes, relative paths, colour.
const PATCH_FLAGS: &[&str] =
    &["--no-color", "--no-ext-diff", "--no-textconv", "--src-prefix=a/", "--dst-prefix=b/", "--no-relative", "-U3"];

/// A path from a caller, as git should see it (relative to the working-tree top).
pub fn rel_path(repo: &Repo, path: &str) -> Result<String, RpcError> {
    if path.is_empty() {
        return Err(RpcError::invalid_params("empty path"));
    }
    let candidate = Path::new(path);
    if candidate.is_absolute() {
        for base in [&repo.top, &repo.root] {
            if let Ok(rest) = candidate.strip_prefix(base) {
                return Ok(rest.to_string_lossy().into_owned());
            }
        }
        return Err(RpcError::invalid_params(format!("`{path}` is outside the working tree {}", repo.top.display())));
    }
    Ok(path.to_owned())
}

/// The diff of one scope as parsed files (with the exact bytes needed to re-apply hunks).
pub async fn diff_files(
    git: &Git,
    repo: &Repo,
    scratch: &Path,
    scope: &str,
    path: Option<&str>,
    base: Option<&str>,
) -> Result<Vec<ParsedFile>, RpcError> {
    // Diff reads may run repository clean/process filters even with external diff and textconv
    // disabled. Every caller, including automatic editor diffs and hunk previews, uses this bound.
    std::fs::create_dir_all(scratch).map_err(|error| RpcError::internal(format!("cannot create Git inspection scratch: {error}")))?;
    let confined = git.confined(scratch);
    let git = &confined;
    let mut args: Vec<OsString> = vec!["diff".into(), "-p".into(), "--find-renames".into()];
    args.extend(PATCH_FLAGS.iter().map(OsString::from));
    let mut index = None;
    let mut untracked_set: HashSet<String> = HashSet::new();
    match scope {
        "unstaged" => {}
        "staged" => args.push("--cached".into()),
        "uncommitted" => {
            let mut untracked = untracked_paths(git, repo).await?;
            if let Some(path) = path {
                // Only the untracked files the path filter can show need to enter the scratch index.
                let wanted = rel_path(repo, path)?;
                let dir = format!("{}/", wanted.trim_end_matches('/'));
                untracked.retain(|p| {
                    let text = path_text(p);
                    text == wanted || text.starts_with(&dir) || wanted == "."
                });
            }
            untracked_set = untracked.iter().map(|p| path_text(p)).collect();
            index = Some(intent_index(git, repo, scratch, &untracked).await?);
            args.push(repo.head_or_empty(git).await?.into());
        }
        "branch" => {
            let base = match base {
                Some(base) => base.to_owned(),
                None => default_base(git, repo).await?,
            };
            let merge_base = git
                .try_text(&repo.top, ["merge-base", base.as_str(), "HEAD"])
                .await?
                .ok_or_else(|| RpcError::invalid_params(format!("no merge base between `{base}` and HEAD")))?;
            args.push(merge_base.into());
            args.push("HEAD".into());
        }
        other => {
            let Some(rev) = other.strip_prefix("commit:") else {
                return Err(RpcError::invalid_params(format!(
                    "unknown diff scope `{other}` (uncommitted | unstaged | staged | branch | commit:<sha>)"
                )));
            };
            let sha = verify_commit(git, repo, rev).await?;
            let parents = git.text(&repo.top, ["rev-list", "--parents", "-n", "1", sha.as_str()]).await?;
            let first_parent = parents.split(' ').nth(1).map(str::to_owned);
            let parent = match first_parent {
                Some(parent) => parent,
                None => repo.empty_tree(git).await?,
            };
            args.push(parent.into());
            args.push(sha.into());
        }
    }
    if let Some(path) = path {
        args.push("--".into());
        args.push(rel_path(repo, path)?.into());
    }
    let cmd = match &index {
        Some(index) => index.command(git, repo),
        None => git.cmd(&repo.top),
    }.args(args);
    let out = cmd.ok().await?;
    let mut files = parse_patch(&out);
    if !untracked_set.is_empty() {
        for file in &mut files {
            if file.status == "added" && file.old_path.is_none() && untracked_set.contains(&file.path) {
                file.status = "untracked".into();
            }
        }
    }
    Ok(files)
}

pub async fn diff(git: &Git, repo: &Repo, scratch: &Path, scope: &str, path: Option<&str>, base: Option<&str>) -> Result<Diff, RpcError> {
    let files: Vec<_> = diff_files(git, repo, scratch, scope, path, base).await?.iter().map(ParsedFile::to_contract).collect();
    let added = files.iter().map(|f| f.added).sum();
    let removed = files.iter().map(|f| f.removed).sum();
    Ok(Diff { files, added, removed })
}

/// The base branch for the `branch` scope when the caller names none: the remote's default branch
/// (`origin/HEAD`), else the configured `init.defaultBranch`, `main` or `master`, whichever exists
/// first. A fixed precedence order, not a guess about meaning.
pub async fn default_base(git: &Git, repo: &Repo) -> Result<String, RpcError> {
    if let Some(remote_head) = git.try_text(&repo.top, ["symbolic-ref", "-q", "--short", "refs/remotes/origin/HEAD"]).await?
        && !remote_head.is_empty()
    {
        return Ok(remote_head);
    }
    let mut candidates = Vec::new();
    if let Some(configured) = git.try_text(&repo.top, ["config", "--get", "init.defaultBranch"]).await? {
        candidates.push(configured);
    }
    candidates.push("main".into());
    candidates.push("master".into());
    for candidate in candidates {
        let reference = format!("refs/heads/{candidate}");
        if git.try_text(&repo.top, ["rev-parse", "-q", "--verify", reference.as_str()]).await?.is_some() {
            return Ok(candidate);
        }
    }
    Err(RpcError::invalid_params("no base branch found; pass `base`"))
}

async fn verify_commit(git: &Git, repo: &Repo, rev: &str) -> Result<String, RpcError> {
    if rev.is_empty() || rev.starts_with('-') {
        return Err(RpcError::invalid_params(format!("invalid revision `{rev}`")));
    }
    let spec = format!("{rev}^{{commit}}");
    git.try_text(&repo.top, ["rev-parse", "-q", "--verify", spec.as_str()])
        .await?
        .ok_or_else(|| RpcError::not_found(format!("`{rev}` is not a commit")))
}

pub async fn log(git: &Git, repo: &Repo, limit: u32) -> Result<LogResult, RpcError> {
    if repo.head(git).await?.is_none() {
        return Ok(LogResult { commits: Vec::new() });
    }
    let mut args: Vec<OsString> = vec!["log".into(), "-z".into(), "--format=%H%x00%an%x00%at%x00%s".into()];
    // 0 means the whole history (no artificial cap).
    if limit > 0 {
        args.push(format!("--max-count={limit}").into());
    }
    args.push("HEAD".into());
    args.push("--".into());
    let out = git.run(&repo.top, args).await?;
    // Four NUL-separated fields per commit; -z also separates commits with NUL.
    let fields = split_nul(&out);
    let commits = fields
        .chunks(4)
        .filter(|chunk| chunk.len() == 4)
        .map(|chunk| {
            let text = |b: &[u8]| String::from_utf8_lossy(b).trim_start_matches('\n').to_owned();
            Commit {
                sha: text(chunk[0]),
                author: text(chunk[1]),
                ts_ms: text(chunk[2]).parse::<u64>().unwrap_or(0) * 1000,
                subject: text(chunk[3]),
            }
        })
        .collect();
    Ok(LogResult { commits })
}

pub async fn branches(git: &Git, repo: &Repo) -> Result<BranchesResult, RpcError> {
    // Ref names cannot contain newlines (git check-ref-format), so line splitting is exact.
    let out = git.text(&repo.top, ["for-each-ref", "--format=%(refname:short)", "refs/heads/"]).await?;
    let branches = out.lines().filter(|l| !l.is_empty()).map(str::to_owned).collect();
    let current = repo.branch(git).await?.unwrap_or_else(|| "HEAD".into());
    Ok(BranchesResult { current, branches })
}

async fn check_branch_name(git: &Git, repo: &Repo, name: &str) -> Result<(), RpcError> {
    if name.is_empty() || name.starts_with('-') {
        return Err(RpcError::invalid_params(format!("invalid branch name `{name}`")));
    }
    let output = git.cmd(&repo.top).args(["check-ref-format", "--branch", name]).output().await?;
    if output.success() { Ok(()) } else { Err(RpcError::invalid_params(format!("invalid branch name `{name}`"))) }
}

pub async fn branch_create(git: &Git, repo: &Repo, name: &str, from: Option<&str>) -> Result<BranchesResult, RpcError> {
    check_branch_name(git, repo, name).await?;
    let mut args: Vec<OsString> = vec!["branch".into(), "--no-track".into(), name.into()];
    if let Some(from) = from {
        args.push(verify_commit(git, repo, from).await?.into());
    }
    git.run(&repo.top, args).await?;
    branches(git, repo).await
}

pub async fn checkout(git: &Git, repo: &Repo, name: &str, from: Option<&str>) -> Result<BranchesResult, RpcError> {
    if let Some(from) = from {
        check_branch_name(git, repo, name).await?;
        let start = verify_commit(git, repo, from).await?;
        git.run(&repo.top, ["switch", "--quiet", "--no-track", "-c", name, start.as_str()]).await?;
        return branches(git, repo).await;
    }
    if name.is_empty() || name.starts_with('-') {
        return Err(RpcError::invalid_params(format!("invalid branch `{name}`")));
    }
    let local = format!("refs/heads/{name}");
    if git.try_text(&repo.top, ["rev-parse", "-q", "--verify", local.as_str()]).await?.is_some() {
        git.run(&repo.top, ["switch", "--quiet", "--no-guess", name]).await?;
    } else {
        let sha = verify_commit(git, repo, name).await?;
        git.run(&repo.top, ["switch", "--quiet", "--detach", sha.as_str()]).await?;
    }
    branches(git, repo).await
}

/// The patch of hunk `index` of `path` in `scope`.
async fn one_hunk_patch(git: &Git, repo: &Repo, scratch: &Path, scope: &str, path: &str, index: u32) -> Result<Vec<u8>, RpcError> {
    let files = diff_files(git, repo, scratch, scope, Some(path), None).await?;
    let wanted = rel_path(repo, path)?;
    let file = files
        .iter()
        .find(|f| f.path == wanted)
        .or_else(|| (files.len() == 1).then(|| &files[0]))
        .ok_or_else(|| RpcError::not_found(format!("`{path}` has no {scope} changes")))?;
    if file.binary {
        return Err(RpcError::invalid_params(format!("`{path}` is binary; stage or revert the whole file")));
    }
    file.hunk_patch(index as usize).ok_or_else(|| {
        RpcError::not_found(format!("`{path}` has {} {scope} hunk(s); hunk {index} does not exist", file.hunks.len()))
    })
}

async fn apply(git: &Git, repo: &Repo, patch: Vec<u8>, flags: &[&str]) -> Result<(), RpcError> {
    let mut args: Vec<&str> = vec!["apply", "--whitespace=nowarn"];
    args.extend_from_slice(flags);
    args.push("-");
    let display = format!("git {}", args.join(" "));
    let output = git.cmd(&repo.top).args(args).stdin(patch).output().await?;
    if output.success() { Ok(()) } else { Err(command_error(&display, &output)) }
}

fn single_path<'a>(paths: &'a [String], what: &str) -> Result<&'a str, RpcError> {
    match paths {
        [only] => Ok(only),
        _ => Err(RpcError::invalid_params(format!("{what} of one hunk needs exactly one path"))),
    }
}

fn rel_paths(repo: &Repo, paths: &[String]) -> Result<Vec<String>, RpcError> {
    if paths.is_empty() {
        return Err(RpcError::invalid_params("`paths` is empty"));
    }
    paths.iter().map(|p| rel_path(repo, p)).collect()
}

pub async fn stage(git: &Git, repo: &Repo, scratch: &Path, paths: &[String], hunk: Option<u32>) -> Result<(), RpcError> {
    match hunk {
        Some(index) => {
            let path = single_path(paths, "staging")?;
            let patch = one_hunk_patch(git, repo, scratch, "unstaged", path, index).await?;
            apply(git, repo, patch, &["--cached"]).await
        }
        None => {
            let mut args: Vec<String> = vec!["add".into(), "-A".into(), "--".into()];
            args.extend(rel_paths(repo, paths)?);
            git.run(&repo.top, args).await.map(|_| ())
        }
    }
}

pub async fn unstage(git: &Git, repo: &Repo, scratch: &Path, paths: &[String], hunk: Option<u32>) -> Result<(), RpcError> {
    match hunk {
        Some(index) => {
            let path = single_path(paths, "unstaging")?;
            let patch = one_hunk_patch(git, repo, scratch, "staged", path, index).await?;
            apply(git, repo, patch, &["--cached", "-R"]).await
        }
        None => {
            let rel = rel_paths(repo, paths)?;
            let mut args: Vec<String> = if repo.head(git).await?.is_some() {
                vec!["restore".into(), "--staged".into(), "--".into()]
            } else {
                // Unborn branch: nothing to restore from, so drop the entries from the index.
                vec!["rm".into(), "--cached".into(), "-r".into(), "-q".into(), "--ignore-unmatch".into(), "--".into()]
            };
            args.extend(rel);
            git.run(&repo.top, args).await.map(|_| ())
        }
    }
}

/// Discard working-tree changes. Without `include_staged`, tracked paths go back to their index
/// version; with it, to HEAD (index too). Untracked files named (or under a named directory) are
/// deleted; ignored files are never touched.
pub async fn revert(
    git: &Git,
    repo: &Repo,
    scratch: &Path,
    paths: &[String],
    hunk: Option<u32>,
    include_staged: bool,
) -> Result<(), RpcError> {
    if let Some(index) = hunk {
        let path = single_path(paths, "reverting")?;
        let patch = one_hunk_patch(git, repo, scratch, "unstaged", path, index).await?;
        return apply(git, repo, patch, &["-R"]).await;
    }
    let rel = rel_paths(repo, paths)?;
    let untracked = untracked_paths(git, repo).await?;
    let mut doomed: Vec<PathBuf> = Vec::new();
    for raw in &untracked {
        let text = path_text(raw);
        if rel.iter().any(|p| text == *p || text.starts_with(&format!("{}/", p.trim_end_matches('/')))) {
            doomed.push(repo.top.join(&text));
        }
    }
    let has_head = repo.head(git).await?.is_some();
    if include_staged && !has_head {
        // Unborn branch: "HEAD" is empty, so staged additions disappear entirely.
        let mut args: Vec<String> = vec!["ls-files".into(), "-z".into(), "--".into()];
        args.extend(rel.iter().cloned());
        let listed = git.run(&repo.top, args).await?;
        let staged: Vec<PathBuf> = split_nul(&listed).iter().map(|p| repo.top.join(path_text(p))).collect();
        let mut rm: Vec<String> = vec!["rm".into(), "--cached".into(), "-r".into(), "-q".into(), "--ignore-unmatch".into(), "--".into()];
        rm.extend(rel.iter().cloned());
        git.run(&repo.top, rm).await?;
        doomed.extend(staged);
    } else {
        // `git restore` fails on a pathspec that matches nothing tracked, so pass only the paths
        // that name tracked files (in the index, or in HEAD when staged changes are discarded too).
        let mut tracked: Vec<String> = Vec::new();
        let mut list: Vec<String> = vec!["ls-files".into(), "-z".into(), "--".into()];
        list.extend(rel.iter().cloned());
        tracked.extend(split_nul(&git.run(&repo.top, list).await?).iter().map(|p| path_text(p)));
        if include_staged {
            let mut list: Vec<String> = vec!["ls-tree".into(), "-r".into(), "-z".into(), "--name-only".into(), "HEAD".into(), "--".into()];
            list.extend(rel.iter().cloned());
            tracked.extend(split_nul(&git.run(&repo.top, list).await?).iter().map(|p| path_text(p)));
        }
        let restorable: Vec<String> = rel
            .iter()
            .filter(|p| {
                let dir = format!("{}/", p.trim_end_matches('/'));
                p.as_str() == "." || tracked.iter().any(|t| t == *p || t.starts_with(&dir))
            })
            .cloned()
            .collect();
        if !restorable.is_empty() {
            let mut args: Vec<String> = vec!["restore".into()];
            if include_staged {
                args.extend(["--source=HEAD".into(), "--staged".into(), "--worktree".into()]);
            } else {
                args.push("--worktree".into());
            }
            args.push("--".into());
            args.extend(restorable);
            git.run(&repo.top, args).await?;
        }
    }
    for path in doomed {
        match std::fs::symlink_metadata(&path) {
            Ok(meta) if meta.is_dir() => {
                // An untracked nested repository shows up as a directory entry.
                std::fs::remove_dir_all(&path)
                    .map_err(|error| RpcError::internal(format!("cannot remove {}: {error}", path.display())))?;
            }
            Ok(_) => std::fs::remove_file(&path)
                .map_err(|error| RpcError::internal(format!("cannot remove {}: {error}", path.display())))?,
            Err(_) => {}
        }
        remove_empty_parents(&repo.top, &path);
    }
    Ok(())
}

fn remove_empty_parents(top: &Path, path: &Path) {
    let mut current = path.parent();
    while let Some(dir) = current {
        if dir == top || !dir.starts_with(top) || std::fs::remove_dir(dir).is_err() {
            break;
        }
        current = dir.parent();
    }
}

pub async fn commit(git: &Git, repo: &Repo, message: &str, all: bool) -> Result<CommitResult, RpcError> {
    if message.trim().is_empty() {
        return Err(RpcError::invalid_params("commit message is empty"));
    }
    if all {
        git.run(&repo.top, ["add", "-A"]).await?;
    }
    git.cmd(&repo.top).args(["commit", "--quiet", "--file=-"]).stdin(message.as_bytes().to_vec()).ok().await?;
    let sha = repo.head(git).await?.ok_or_else(|| RpcError::internal("commit succeeded but HEAD is unborn"))?;
    Ok(CommitResult { sha })
}

/// Paths with unmerged index entries.
pub async fn conflicted_paths(git: &Git, repo: &Repo) -> Result<Vec<String>, RpcError> {
    let out = git.run(&repo.top, ["diff", "--name-only", "--diff-filter=U", "-z"]).await?;
    let mut paths: Vec<String> = split_nul(&out).iter().map(|p| path_text(p)).collect();
    paths.dedup();
    Ok(paths)
}

pub async fn merge(
    git: &Git,
    repo: &Repo,
    branch: &str,
    message: Option<&str>,
    abort_on_conflict: bool,
) -> Result<MergeResult, RpcError> {
    if repo.merging() {
        return Err(RpcError::new(codes::CONFLICT, "a merge is already in progress; resolve or abort it first"));
    }
    let sha = verify_commit(git, repo, branch).await?;
    // A clean tracked tree is required so that aborting restores exactly the pre-merge state
    // (git cannot always reconstruct uncommitted changes after `merge --abort`).
    let dirty = git.run(&repo.top, ["status", "--porcelain=v2", "-z", "--untracked-files=no"]).await?;
    if split_nul(&dirty).iter().any(|r| !r.starts_with(b"#")) {
        return Err(RpcError::new(
            codes::CONFLICT,
            "the working tree has uncommitted changes to tracked files; commit them (or create a checkpoint and revert) before merging",
        ));
    }
    let mut args: Vec<OsString> = vec!["merge".into(), "--no-edit".into(), "--no-stat".into()];
    if let Some(message) = message {
        args.push("-m".into());
        args.push(message.into());
    }
    args.push(sha.clone().into());
    let output = git.cmd(&repo.top).args(args).output().await?;
    if output.success() {
        return Ok(MergeResult { ok: true, conflicts: Vec::new(), sha: repo.head(git).await?, aborted: false, blocked_by_local: Vec::new() });
    }
    if !repo.merging() {
        // Refused before starting (e.g. untracked files would be overwritten): nothing changed.
        return Err(command_error(&format!("git merge {branch}"), &output));
    }
    let conflicts = conflicted_paths(git, repo).await?;
    if abort_on_conflict {
        git.run(&repo.top, ["merge", "--abort"]).await?;
    }
    Ok(MergeResult { ok: false, conflicts, sha: None, aborted: abort_on_conflict, blocked_by_local: Vec::new() })
}

/// `vcs.merge` with `keep_local_changes`: merge `branch` into HEAD without touching uncommitted
/// changes. The merge is computed apart from the working tree (`git merge-tree --write-tree`), so a
/// conflict changes nothing; the files the merge changes are checked against the uncommitted and
/// untracked files first (structurally: path sets, not git's messages); then the index and working
/// tree move to the merge commit with a two-way `read-tree -m -u`, which keeps every local change,
/// and HEAD's branch is updated (guarded by its old value).
pub async fn merge_keeping_local(git: &Git, repo: &Repo, branch: &str, message: Option<&str>, no_ff: bool) -> Result<MergeResult, RpcError> {
    if repo.merging() {
        return Err(RpcError::new(codes::CONFLICT, "a merge is already in progress; resolve or abort it first"));
    }
    let sha = verify_commit(git, repo, branch).await?;
    let head = repo.head(git).await?.ok_or_else(|| RpcError::new(codes::CONFLICT, "HEAD is unborn: nothing to merge into"))?;
    let done = |sha: String| MergeResult { ok: true, conflicts: Vec::new(), sha: Some(sha), aborted: false, blocked_by_local: Vec::new() };
    // Already contained: nothing to do.
    if git.cmd(&repo.top).args(["merge-base", "--is-ancestor", sha.as_str(), head.as_str()]).output().await?.success() {
        return Ok(done(head));
    }
    let target = if !no_ff && git.cmd(&repo.top).args(["merge-base", "--is-ancestor", head.as_str(), sha.as_str()]).output().await?.success() {
        // Fast-forward, as `git merge` does by default.
        sha.clone()
    } else {
        let output = git.cmd(&repo.top).args(["merge-tree", "--write-tree", "-z", "--name-only", head.as_str(), sha.as_str()]).output().await?;
        // Output: the tree, then (on conflict) the conflicted paths, an empty record, messages.
        let records = split_nul(&output.stdout);
        match output.code {
            Some(0) => {}
            Some(1) => {
                let mut conflicts: Vec<String> = records.iter().skip(1).take_while(|record| !record.is_empty()).map(|path| path_text(path)).collect();
                conflicts.dedup();
                return Ok(MergeResult { ok: false, conflicts, sha: None, aborted: true, blocked_by_local: Vec::new() });
            }
            _ => return Err(command_error(&format!("git merge-tree {branch}"), &output)),
        }
        let tree = records.first().map(|tree| path_text(tree)).filter(|tree| !tree.is_empty()).ok_or_else(|| RpcError::internal("git merge-tree printed no tree"))?;
        let message = message.map(str::to_owned).unwrap_or_else(|| format!("Merge {branch}"));
        git.cmd(&repo.top).args(["commit-tree", tree.as_str(), "-p", head.as_str(), "-p", sha.as_str(), "-F", "-"]).stdin(message.into_bytes()).ok().await.map(|out| path_text(out.trim_ascii()))?
    };
    // Files the merge changes that have local changes (staged or not) or exist untracked.
    let changed: BTreeSet<String> = split_nul(&git.run(&repo.top, ["diff", "--name-only", "--no-renames", "-z", head.as_str(), target.as_str()]).await?).iter().map(|path| path_text(path)).collect();
    let mut local: BTreeSet<String> = split_nul(&git.run(&repo.top, ["diff", "--name-only", "--no-renames", "-z", "HEAD"]).await?).iter().map(|path| path_text(path)).collect();
    local.extend(split_nul(&git.run(&repo.top, ["ls-files", "--others", "--exclude-standard", "-z"]).await?).iter().map(|path| path_text(path)));
    let blocked: Vec<String> = changed.intersection(&local).cloned().collect();
    if !blocked.is_empty() {
        return Ok(MergeResult { ok: false, conflicts: Vec::new(), sha: None, aborted: false, blocked_by_local: blocked });
    }
    git.run(&repo.top, ["read-tree", "-m", "-u", head.as_str(), target.as_str()]).await?;
    git.run(&repo.top, ["update-ref", "-m", &format!("merge {branch}"), "HEAD", target.as_str(), head.as_str()]).await?;
    Ok(done(target))
}

pub async fn merge_abort(git: &Git, repo: &Repo) -> Result<MergeAbortResult, RpcError> {
    if !repo.merging() {
        return Err(RpcError::new(codes::CONFLICT, "no merge is in progress"));
    }
    git.run(&repo.top, ["merge", "--abort"]).await?;
    Ok(MergeAbortResult { head: repo.head(git).await? })
}
