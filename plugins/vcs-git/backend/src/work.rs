//! A runner's repositories (`runner@1`): a bare mirror of a source, one working tree per job, and
//! what landing a job needs (rebase, squash by request, fast-forward), plus reads of a revision
//! without a checkout. Host code calls these; none is a command of the model's shells.
//!
//! The mirror is a bare repository whose `origin` is the source. The source's branches are kept as
//! `refs/remotes/origin/*` and the runner's own job branches as `refs/heads/*`: with git's own
//! `--mirror` layout (`+refs/*:refs/*`) a pruning fetch deletes every job branch the source does
//! not have, a fetch into a branch a working tree has checked out is refused, and a push of one
//! branch is refused (`--mirror can't be combined with refspecs`).

use crate::git::{Git, command_error, path_text, split_nul};
use crate::github::Github;
use crate::ops::conflicted_paths;
use crate::remote;
use crate::repo::Repo;
use cc_protocol::contracts::vcs::{GrepHit, GrepResult, MirrorResult, RebaseResult, RevResult, SquashGroup, SquashResult, TreeResult, WorktreeResult};
use cc_protocol::{RpcError, codes};
use std::path::{Path, PathBuf};

/// Largest file `vcs.show` answers whole.
const SHOW_LIMIT: usize = 2 * 1024 * 1024;
const GREP_DEFAULT: usize = 100;
const GREP_MAX: usize = 2000;

/// An existing repository folder (a working tree or a bare repository), canonical.
pub fn repo_dir(path: &str) -> Result<PathBuf, RpcError> {
    crate::repo::canonical_root(path)
}

fn absolute(path: &str, what: &str) -> Result<PathBuf, RpcError> {
    let candidate = Path::new(path);
    if path.is_empty() || !candidate.is_absolute() {
        return Err(RpcError::invalid_params(format!("`{what}` must be an absolute path, got `{path}`")));
    }
    Ok(candidate.to_path_buf())
}

/// A revision or name handed to git as one argument: never an option, never a second argument.
fn checked<'a>(value: &'a str, what: &str) -> Result<&'a str, RpcError> {
    if value.is_empty() || value.starts_with('-') || value.chars().any(|c| c.is_control() || c == ' ') {
        return Err(RpcError::invalid_params(format!("invalid {what} `{value}`")));
    }
    Ok(value)
}

/// The commit `rev` names in the repository at `dir`, or None.
pub async fn rev(git: &Git, dir: &Path, rev: &str) -> Result<RevResult, RpcError> {
    let spec = format!("{}^{{commit}}", checked(rev, "revision")?);
    Ok(RevResult { sha: git.try_text(dir, ["rev-parse", "-q", "--verify", spec.as_str()]).await? })
}

async fn commit_of(git: &Git, dir: &Path, name: &str) -> Result<String, RpcError> {
    rev(git, dir, name).await?.sha.ok_or_else(|| RpcError::not_found(format!("`{name}` is not a commit")))
}

/// `vcs.mirror`: the bare repository of `url` at `path`, made when absent, then fetched with prune.
pub async fn mirror(git: &Git, github: &Github, url: &str, path: &str) -> Result<MirrorResult, RpcError> {
    let url = url.trim();
    if url.is_empty() || url.starts_with('-') {
        return Err(RpcError::invalid_params(format!("invalid source address `{url}`")));
    }
    let dest = absolute(path, "path")?;
    let created = !dest.join("HEAD").is_file();
    if created {
        let occupied = dest.exists() && (!dest.is_dir() || std::fs::read_dir(&dest).map(|mut entries| entries.next().is_some()).unwrap_or(true));
        if occupied {
            return Err(RpcError::invalid_params(format!("{} exists and is not a git repository", dest.display())));
        }
        std::fs::create_dir_all(&dest).map_err(|error| RpcError::invalid_params(format!("cannot create {}: {error}", dest.display())))?;
        git.run(&dest, ["init", "--quiet", "--bare"]).await?;
        git.run(&dest, ["remote", "add", "origin", url]).await?;
    } else if git.try_text(&dest, ["remote", "get-url", "origin"]).await?.as_deref() != Some(url) {
        // The source moved (spec: leaving a host means changing the address, nothing else).
        if git.try_text(&dest, ["remote", "get-url", "origin"]).await?.is_some() {
            git.run(&dest, ["remote", "set-url", "origin", url]).await?;
        } else {
            git.run(&dest, ["remote", "add", "origin", url]).await?;
        }
    }
    let dest = std::fs::canonicalize(&dest).unwrap_or(dest);
    let fetched = remote::fetch(git, github, &dest, Some("origin"), true).await?;
    Ok(MirrorResult { path: dest.to_string_lossy().into_owned(), created, ok: fetched.ok, available: fetched.available, detail: fetched.detail })
}

/// The working trees of the repository at `dir`: `(path, branch ref)`.
async fn worktrees(git: &Git, dir: &Path) -> Result<Vec<(PathBuf, String)>, RpcError> {
    let out = git.run(dir, ["worktree", "list", "--porcelain", "-z"]).await?;
    let mut trees = Vec::new();
    let mut current: Option<PathBuf> = None;
    for record in split_nul(&out) {
        let line = path_text(record);
        if let Some(path) = line.strip_prefix("worktree ") {
            current = Some(PathBuf::from(path));
            trees.push((PathBuf::from(path), String::new()));
        } else if let Some(branch) = line.strip_prefix("branch ")
            && current.is_some()
            && let Some(last) = trees.last_mut()
        {
            last.1 = branch.to_owned();
        }
    }
    Ok(trees)
}

/// `vcs.worktree.add`: a working tree of `repo` at `path` on `branch`. The tree that is already
/// there on that branch is answered as it is, so a repeated call changes nothing.
pub async fn worktree_add(git: &Git, repo: &Path, path: &str, branch: &str, from: &str) -> Result<WorktreeResult, RpcError> {
    let dest = absolute(path, "path")?;
    let branch = checked(branch, "branch")?;
    let reference = format!("refs/heads/{branch}");
    git.run(repo, ["worktree", "prune"]).await?;
    let canonical = std::fs::canonicalize(&dest).unwrap_or_else(|_| dest.clone());
    if let Some((_, checked_out)) = worktrees(git, repo).await?.into_iter().find(|(tree, _)| *tree == canonical || *tree == dest) {
        if checked_out != reference {
            return Err(RpcError::new(codes::CONFLICT, format!("{} is a working tree on `{}`, not on `{branch}`", dest.display(), checked_out.trim_start_matches("refs/heads/"))));
        }
        let head = commit_of(git, &canonical, "HEAD").await?;
        return Ok(WorktreeResult { path: canonical.to_string_lossy().into_owned(), branch: branch.to_owned(), head });
    }
    if dest.exists() && std::fs::read_dir(&dest).map(|mut entries| entries.next().is_some()).unwrap_or(true) {
        return Err(RpcError::invalid_params(format!("{} already exists and is not empty", dest.display())));
    }
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent).map_err(|error| RpcError::invalid_params(format!("cannot create {}: {error}", parent.display())))?;
    }
    let exists = git.try_text(repo, ["rev-parse", "-q", "--verify", reference.as_str()]).await?.is_some();
    let dest_text = dest.to_string_lossy().into_owned();
    if exists {
        git.run(repo, ["worktree", "add", "--quiet", dest_text.as_str(), branch]).await?;
    } else {
        let start = commit_of(git, repo, from).await?;
        // No upstream: the job branch must never be taken for the main branch by a later push.
        git.run(repo, ["worktree", "add", "--quiet", "--no-track", "-b", branch, dest_text.as_str(), start.as_str()]).await?;
    }
    let canonical = std::fs::canonicalize(&dest).unwrap_or(dest);
    let head = commit_of(git, &canonical, "HEAD").await?;
    Ok(WorktreeResult { path: canonical.to_string_lossy().into_owned(), branch: branch.to_owned(), head })
}

/// `vcs.worktree.remove`: the working tree goes, its branch stays. A tree that is not there is
/// done already.
pub async fn worktree_remove(git: &Git, repo: &Path, path: &str, force: bool) -> Result<bool, RpcError> {
    let dest = absolute(path, "path")?;
    let canonical = std::fs::canonicalize(&dest).unwrap_or_else(|_| dest.clone());
    let known = worktrees(git, repo).await?.into_iter().any(|(tree, _)| tree == canonical || tree == dest);
    if known {
        let dest_text = canonical.to_string_lossy().into_owned();
        let mut args = vec!["worktree", "remove"];
        if force {
            args.push("--force");
        }
        args.push(dest_text.as_str());
        git.run(repo, args).await?;
    }
    git.run(repo, ["worktree", "prune"]).await?;
    Ok(known)
}

/// `vcs.rebase`: the current branch onto `onto`. Tracked files must be clean, so an abort puts
/// back exactly what was there.
pub async fn rebase(git: &Git, repo: &Repo, onto: &str) -> Result<RebaseResult, RpcError> {
    let target = commit_of(git, &repo.top, onto).await?;
    let dirty = git.run(&repo.top, ["status", "--porcelain=v2", "-z", "--untracked-files=no"]).await?;
    if split_nul(&dirty).iter().any(|record| !record.starts_with(b"#")) {
        return Err(RpcError::new(codes::CONFLICT, "the working tree has uncommitted changes to tracked files; commit them before the rebase"));
    }
    let output = git.cmd(&repo.top).args(["rebase", "--quiet", target.as_str()]).output().await?;
    if output.success() {
        let head = commit_of(git, &repo.top, "HEAD").await?;
        return Ok(RebaseResult { ok: true, conflicts: Vec::new(), head });
    }
    let conflicts = conflicted_paths(git, repo).await.unwrap_or_default();
    let in_progress = repo.git_dir.join("rebase-merge").exists() || repo.git_dir.join("rebase-apply").exists();
    if in_progress {
        git.run(&repo.top, ["rebase", "--abort"]).await?;
    }
    if conflicts.is_empty() {
        // Not a conflict: git's own reason is the answer.
        return Err(command_error("git rebase", &output));
    }
    let head = commit_of(git, &repo.top, "HEAD").await?;
    Ok(RebaseResult { ok: false, conflicts, head })
}

/// `vcs.reset`: index and working tree back to `to`; with `clean`, untracked files go too
/// (ignored files, build output among them, stay).
pub async fn reset(git: &Git, repo: &Repo, to: &str, clean: bool) -> Result<RevResult, RpcError> {
    let target = commit_of(git, &repo.top, to).await?;
    git.run(&repo.top, ["reset", "--quiet", "--hard", target.as_str()]).await?;
    if clean {
        git.run(&repo.top, ["clean", "--quiet", "-f", "-d"]).await?;
    }
    Ok(RevResult { sha: Some(target) })
}

/// The values of trailer `name` in a commit message (`Request: CT-0142`).
fn trailer_values(message: &str, name: &str) -> Vec<String> {
    message
        .lines()
        .filter_map(|line| line.split_once(':'))
        .filter(|(key, _)| key.trim().eq_ignore_ascii_case(name))
        .map(|(_, value)| value.trim().to_owned())
        .filter(|value| !value.is_empty())
        .collect()
}

/// `vcs.squash` (see [`cc_protocol::contracts::vcs::SquashParams`]). The last new commit has
/// HEAD's tree, so the working tree and index are as they were.
pub async fn squash(git: &Git, repo: &Repo, base: &str, trailer: &str, groups: &[SquashGroup]) -> Result<SquashResult, RpcError> {
    if trailer.trim().is_empty() || groups.is_empty() {
        return Err(RpcError::invalid_params("a squash needs the trailer's name and at least one group"));
    }
    let base = commit_of(git, &repo.top, base).await?;
    let head = commit_of(git, &repo.top, "HEAD").await?;
    if git.cmd(&repo.top).args(["merge-base", "--is-ancestor", base.as_str(), head.as_str()]).output().await?.code != Some(0) {
        return Err(RpcError::new(codes::CONFLICT, "the base is not an ancestor of the branch; rebase it first"));
    }
    let range = format!("{base}..{head}");
    let listed = git.text(&repo.top, ["rev-list", "--reverse", "--first-parent", range.as_str()]).await?;
    let commits: Vec<&str> = listed.lines().filter(|line| !line.is_empty()).collect();
    if commits.is_empty() {
        return Ok(SquashResult { head, commits: Vec::new() });
    }
    // Where each group ends: the position of the last commit that carries its value.
    let mut ends: Vec<(usize, &SquashGroup)> = Vec::new();
    let mut messages = Vec::with_capacity(commits.len());
    for sha in &commits {
        messages.push(git.text(&repo.top, ["show", "-s", "--format=%B", *sha]).await?);
    }
    for group in groups {
        let last = messages.iter().rposition(|message| trailer_values(message, trailer).contains(&group.value));
        if let Some(at) = last {
            ends.push((at, group));
        }
    }
    if ends.is_empty() {
        return Err(RpcError::new(codes::CONFLICT, format!("no commit after the base carries a `{trailer}:` line of these groups")));
    }
    ends.sort_by_key(|(at, _)| *at);
    if let Some(last) = ends.last_mut() {
        last.0 = commits.len() - 1;
    }
    let mut parent = base.clone();
    let mut made = Vec::new();
    for (at, group) in ends {
        if group.message.trim().is_empty() {
            return Err(RpcError::invalid_params(format!("the commit for `{}` has no message", group.value)));
        }
        let (tree_spec, parent_spec) = (format!("{}^{{tree}}", commits[at]), format!("{parent}^{{tree}}"));
        let tree = git.text(&repo.top, ["rev-parse", tree_spec.as_str()]).await?;
        let parent_tree = git.text(&repo.top, ["rev-parse", parent_spec.as_str()]).await?;
        if tree == parent_tree {
            continue;
        }
        let out = git.cmd(&repo.top).args(["commit-tree", tree.as_str(), "-p", parent.as_str(), "-F", "-"]).stdin(group.message.as_bytes().to_vec()).ok().await?;
        parent = String::from_utf8_lossy(&out).trim().to_owned();
        made.push(parent.clone());
    }
    if made.is_empty() {
        return Ok(SquashResult { head, commits: Vec::new() });
    }
    // Same tree as before: only the branch moves.
    git.run(&repo.top, ["reset", "--quiet", "--soft", parent.as_str()]).await?;
    Ok(SquashResult { head: parent, commits: made })
}

/// `vcs.branch.advance`: `refs/heads/<branch>` forward to `to`. A branch that does not exist yet
/// starts from the source's branch of that name (`refs/remotes/origin/<branch>`).
pub async fn advance(git: &Git, dir: &Path, branch: &str, to: &str) -> Result<RevResult, RpcError> {
    let branch = checked(branch, "branch")?;
    let reference = format!("refs/heads/{branch}");
    let target = commit_of(git, dir, to).await?;
    if worktrees(git, dir).await?.iter().any(|(_, checked_out)| *checked_out == reference) {
        return Err(RpcError::new(codes::CONFLICT, format!("`{branch}` is checked out in a working tree; it is not moved from outside")));
    }
    let local = git.try_text(dir, ["rev-parse", "-q", "--verify", reference.as_str()]).await?;
    let tracked = format!("refs/remotes/origin/{branch}");
    let current = match &local {
        Some(local) => Some(local.clone()),
        None => git.try_text(dir, ["rev-parse", "-q", "--verify", tracked.as_str()]).await?,
    };
    if let Some(current) = &current
        && git.cmd(dir).args(["merge-base", "--is-ancestor", current.as_str(), target.as_str()]).output().await?.code != Some(0)
    {
        return Err(RpcError::new(codes::CONFLICT, format!("`{branch}` cannot fast-forward to {target}: it has commits that one does not")));
    }
    let mut args = vec!["update-ref", "-m", "runner: land", reference.as_str(), target.as_str()];
    if let Some(local) = &local {
        args.push(local.as_str());
    }
    git.run(dir, args).await?;
    Ok(RevResult { sha: Some(target) })
}

/// `vcs.show`: the file at `path` as revision `rev` has it.
pub async fn show(git: &Git, dir: &Path, rev: &str, path: &str) -> Result<String, RpcError> {
    let commit = commit_of(git, dir, rev).await?;
    let path = path.trim().trim_start_matches("./").trim_matches('/');
    if path.is_empty() || path.split('/').any(|part| part == "..") {
        return Err(RpcError::invalid_params(format!("invalid path `{path}`")));
    }
    let spec = format!("{commit}:{path}");
    let kind = git.try_text(dir, ["cat-file", "-t", spec.as_str()]).await?;
    match kind.as_deref() {
        Some("blob") => {}
        Some(_) => return Err(RpcError::invalid_params(format!("`{path}` is a folder; list it with vcs.tree"))),
        None => return Err(RpcError::not_found(format!("no file `{path}` at that revision"))),
    }
    let bytes = git.run(dir, ["cat-file", "blob", spec.as_str()]).await?;
    if bytes.len() > SHOW_LIMIT {
        return Err(RpcError::invalid_params(format!("`{path}` is {} bytes; too large to read whole", bytes.len())));
    }
    if bytes.contains(&0) {
        return Err(RpcError::invalid_params(format!("`{path}` is not a text file")));
    }
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

/// `vcs.grep`: lines of the revision's text files that contain `text`.
pub async fn grep(git: &Git, dir: &Path, rev: &str, text: &str, under: &str, limit: Option<u32>) -> Result<GrepResult, RpcError> {
    if text.is_empty() {
        return Err(RpcError::invalid_params("nothing to search for"));
    }
    let commit = commit_of(git, dir, rev).await?;
    let limit = limit.map(|limit| limit as usize).unwrap_or(GREP_DEFAULT).clamp(1, GREP_MAX);
    let mut cmd = git.cmd(dir).args(["grep", "-n", "-I", "-F", "--full-name", "-z", "-e", text, commit.as_str()]);
    let under = under.trim().trim_start_matches("./").trim_matches('/');
    if !under.is_empty() {
        if under.split('/').any(|part| part == "..") {
            return Err(RpcError::invalid_params(format!("invalid path `{under}`")));
        }
        cmd = cmd.args(["--", under]);
    }
    let output = cmd.output().await?;
    // 1: nothing matched.
    if output.code == Some(1) {
        return Ok(GrepResult::default());
    }
    if !output.success() {
        return Err(command_error("git grep", &output));
    }
    let prefix = format!("{commit}:");
    let mut hits = Vec::new();
    let mut truncated = false;
    // `<rev>:<path>\0<line>\0<text>\n` per hit.
    for record in output.stdout.split(|byte| *byte == b'\n').filter(|record| !record.is_empty()) {
        let mut parts = record.splitn(3, |byte| *byte == 0);
        let (Some(name), Some(line), Some(text)) = (parts.next(), parts.next(), parts.next()) else { continue };
        if hits.len() >= limit {
            truncated = true;
            break;
        }
        let name = path_text(name);
        let text: String = String::from_utf8_lossy(text).chars().take(400).collect();
        hits.push(GrepHit { path: name.strip_prefix(&prefix).unwrap_or(&name).to_owned(), line: path_text(line).parse().unwrap_or(0), text });
    }
    Ok(GrepResult { hits, truncated })
}

/// `vcs.tree`: the entries of a folder of the revision, folders first marked with `/`.
pub async fn tree(git: &Git, dir: &Path, rev: &str, path: &str) -> Result<TreeResult, RpcError> {
    let commit = commit_of(git, dir, rev).await?;
    let path = path.trim().trim_start_matches("./").trim_matches('/');
    if path.split('/').any(|part| part == "..") {
        return Err(RpcError::invalid_params(format!("invalid path `{path}`")));
    }
    let spec = if path.is_empty() { commit } else { format!("{commit}:{path}") };
    let output = git.cmd(dir).args(["ls-tree", "-z", spec.as_str()]).output().await?;
    if !output.success() {
        return Err(RpcError::not_found(format!("no folder `{path}` at that revision")));
    }
    let mut folders = Vec::new();
    let mut files = Vec::new();
    // `<mode> <type> <sha>\t<name>` per entry.
    for record in split_nul(&output.stdout) {
        let line = path_text(record);
        let Some((meta, name)) = line.split_once('\t') else { continue };
        if meta.split(' ').nth(1) == Some("tree") {
            folders.push(format!("{name}/"));
        } else {
            files.push(name.to_owned());
        }
    }
    folders.extend(files);
    Ok(TreeResult { entries: folders })
}
