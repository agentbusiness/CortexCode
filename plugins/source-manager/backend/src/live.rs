//! The live folder as one git repository, through git as a library (no git program needed). It
//! holds every installed plugin's data at its path in CortexCode's repository
//! (`plugins/<id>/data/...`, `plugins/<id>/backend/data/...`) and nothing else.
//!
//! - `refs/heads/main` is the live folder's branch: the packages' data plus the user's edits.
//! - `refs/cortexcode/shipped` is the packages' data the folder was last updated from. A shipped
//!   commit is a root commit with fixed metadata, so the same data gives the same commit on every
//!   Mac, and an update or a restore merges three trees with an explicit base (the shipped tree the
//!   edits were made on), never by shared history.
//!
//! Every commit carries trailers the history reads back: `Origin:` (shipped | update | edit |
//! revert | restore), `Plugins:`, `Shipped-Tree:` (shipped and update commits) and `Reverts:`.

use crate::packages::Package;
use crate::paths;
use cc_protocol::contracts::source::{Change, FileDiff};
use git2::{
    Cred, Delta, DiffFormat, FetchOptions, FileMode, Index, ObjectType, Oid, PushOptions, RemoteCallbacks, Repository,
    RepositoryInitOptions, Signature, Status, StatusOptions, Time, Tree, TreeWalkMode, TreeWalkResult, build::CheckoutBuilder,
};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};

pub const MAIN: &str = "refs/heads/main";
pub const SHIPPED: &str = "refs/cortexcode/shipped";
pub const BACKUP: &str = "refs/cortexcode/backup";
pub const UPSTREAM: &str = "refs/cortexcode/upstream";

type Result<T> = std::result::Result<T, String>;

pub(crate) fn err(error: git2::Error) -> String {
    error.message().to_owned()
}

/// Whether `dir` is a live folder this plugin manages (it has a shipped commit).
pub fn is_managed(dir: &Path) -> bool {
    Repository::open(dir).ok().is_some_and(|repo| repo.find_reference(SHIPPED).is_ok())
}

pub fn open(dir: &Path) -> Result<Repository> {
    Repository::open(dir).map_err(|e| format!("cannot open the live folder {}: {}", dir.display(), e.message()))
}

fn signature(at_ms: u64) -> Result<Signature<'static>> {
    Signature::new("CortexCode", "cortexcode@localhost", &Time::new((at_ms / 1000) as i64, 0)).map_err(err)
}

fn now_ms() -> u64 {
    cc_protocol::contracts::now_ms()
}

/// A commit message: the summary, then the trailers.
pub fn message(summary: &str, trailers: &[(&str, String)]) -> String {
    let mut text = summary.trim().to_owned();
    let lines: Vec<String> = trailers.iter().filter(|(_, value)| !value.is_empty()).map(|(key, value)| format!("{key}: {value}")).collect();
    if !lines.is_empty() {
        text.push_str("\n\n");
        text.push_str(&lines.join("\n"));
    }
    text.push('\n');
    text
}

/// The summary and trailers of a commit message.
pub fn parse_message(text: &str) -> (String, HashMap<String, String>) {
    let summary = text.lines().next().unwrap_or("").trim().to_owned();
    let mut trailers = HashMap::new();
    if let Some(last) = text.trim_end().rsplit("\n\n").next() {
        for line in last.lines() {
            if let Some((key, value)) = line.split_once(": ")
                && !key.is_empty()
                && key.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
            {
                trailers.insert(key.to_owned(), value.trim().to_owned());
            }
        }
    }
    (summary, trailers)
}

// ----- the packages' data -----

/// Every regular file under `root/dir`, as (path relative to `root`, full path).
fn files_under(root: &Path, dir: &str, out: &mut Vec<(String, PathBuf)>) -> Result<()> {
    let mut stack = vec![root.join(dir)];
    while let Some(current) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&current) else { continue };
        for entry in entries.flatten() {
            let Ok(kind) = entry.file_type() else { continue };
            let path = entry.path();
            if kind.is_dir() {
                stack.push(path);
            } else if kind.is_file() {
                let relative = path.strip_prefix(root).map_err(|e| e.to_string())?.to_string_lossy().into_owned();
                out.push((relative, path));
            }
        }
    }
    Ok(())
}

/// One package's data files, relative to the plugin folder (`data/skills/x.md`).
fn package_files(package: &Package) -> Result<Vec<(String, PathBuf)>> {
    let mut files = Vec::new();
    for dir in &package.dirs {
        files_under(&package.dir, dir.trim_end_matches('/'), &mut files)?;
    }
    files.sort();
    files.dedup_by(|a, b| a.0 == b.0);
    Ok(files)
}

enum Node {
    File(Oid, i32),
    Dir(BTreeMap<String, Node>),
}

fn write_tree(repo: &Repository, dir: &BTreeMap<String, Node>) -> Result<Oid> {
    let mut builder = repo.treebuilder(None).map_err(err)?;
    for (name, node) in dir {
        match node {
            Node::File(oid, mode) => builder.insert(name, *oid, *mode).map_err(err)?,
            Node::Dir(children) => builder.insert(name, write_tree(repo, children)?, i32::from(FileMode::Tree)).map_err(err)?,
        };
    }
    builder.write().map_err(err)
}

/// Write `files` (relative path, file on disk) into `repo` as a tree.
fn tree_of(repo: &Repository, files: &[(String, PathBuf)]) -> Result<Oid> {
    use std::os::unix::fs::PermissionsExt;
    let mut root: BTreeMap<String, Node> = BTreeMap::new();
    for (file, path) in files {
        let bytes = std::fs::read(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        let executable = std::fs::metadata(path).is_ok_and(|meta| meta.permissions().mode() & 0o111 != 0);
        let mode = i32::from(if executable { FileMode::BlobExecutable } else { FileMode::Blob });
        let oid = repo.blob(&bytes).map_err(err)?;
        let parts: Vec<&str> = file.split('/').collect();
        let mut dir = &mut root;
        for part in &parts[..parts.len() - 1] {
            let entry = dir.entry((*part).to_owned()).or_insert_with(|| Node::Dir(BTreeMap::new()));
            let Node::Dir(children) = entry else { return Err(format!("`{file}` is both a file and a folder")) };
            dir = children;
        }
        dir.insert(parts[parts.len() - 1].to_owned(), Node::File(oid, mode));
    }
    write_tree(repo, &root)
}

/// The data of every package as one tree, each plugin's under `plugins/<id>/`.
pub fn shipped_tree(repo: &Repository, packages: &[Package]) -> Result<Oid> {
    let mut files = Vec::new();
    for package in packages {
        files.extend(package_files(package)?.into_iter().map(|(file, path)| (format!("plugins/{}/{file}", package.id), path)));
    }
    tree_of(repo, &files)
}

/// One package's data as the tree of its plugin folder; None when it has no data.
fn plugin_tree(repo: &Repository, package: &Package) -> Result<Option<Oid>> {
    let files = package_files(package)?;
    if files.is_empty() { Ok(None) } else { tree_of(repo, &files).map(Some) }
}

/// The shipped commit for `tree`: a root commit with fixed metadata (the same on every Mac).
fn shipped_commit(repo: &Repository, tree: Oid) -> Result<Oid> {
    let tree = repo.find_tree(tree).map_err(err)?;
    let sig = signature(0)?;
    let text = message("CortexCode data", &[("Origin", "shipped".into()), ("Shipped-Tree", tree.id().to_string())]);
    repo.commit(None, &sig, &sig, &text, &tree, &[]).map_err(err)
}

/// Create the live folder from the packages' data.
pub fn populate(dir: &Path, packages: &[Package]) -> Result<()> {
    std::fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    let mut options = RepositoryInitOptions::new();
    options.initial_head("main");
    let repo = Repository::init_opts(dir, &options).map_err(err)?;
    let tree = shipped_tree(&repo, packages)?;
    let commit = shipped_commit(&repo, tree)?;
    repo.reference(MAIN, commit, true, "shipped data").map_err(err)?;
    repo.reference(SHIPPED, commit, true, "shipped data").map_err(err)?;
    repo.set_head(MAIN).map_err(err)?;
    repo.checkout_head(Some(CheckoutBuilder::new().force())).map_err(err)?;
    Ok(())
}

pub struct Updated {
    /// up_to_date | merged | conflicts
    pub state: &'static str,
    pub conflicts: Vec<String>,
}

/// Bring the live folder to the installed packages' data, keeping the user's edits.
pub fn update(repo: &Repository, packages: &[Package]) -> Result<Updated> {
    let tree = shipped_tree(repo, packages)?;
    merge_shipped(repo, tree, "Updated to the installed plugins")
}

/// The shipped tree with `plugins/<id>` replaced by the package's data.
fn released_tree(repo: &Repository, base: &Tree, package: &Package) -> Result<Oid> {
    let plugin = plugin_tree(repo, package)?;
    with_plugin(repo, base, &package.id, plugin)
}

/// `root` with `plugins/<id>` replaced by the tree `plugin` (removed when None).
fn with_plugin(repo: &Repository, root: &Tree, id: &str, plugin: Option<Oid>) -> Result<Oid> {
    let tree_mode = i32::from(FileMode::Tree);
    let plugins = match root.get_name("plugins") {
        Some(entry) => Some(repo.find_tree(entry.id()).map_err(err)?),
        None => None,
    };
    let mut builder = repo.treebuilder(plugins.as_ref()).map_err(err)?;
    match plugin {
        Some(plugin) => {
            builder.insert(id, plugin, tree_mode).map_err(err)?;
        }
        None if builder.get(id).map_err(err)?.is_some() => builder.remove(id).map_err(err)?,
        None => {}
    }
    let plugins = builder.write().map_err(err)?;
    let mut out = repo.treebuilder(Some(root)).map_err(err)?;
    if builder.is_empty() {
        if out.get("plugins").map_err(err)?.is_some() {
            out.remove("plugins").map_err(err)?;
        }
    } else {
        out.insert("plugins", plugins, tree_mode).map_err(err)?;
    }
    out.write().map_err(err)
}

/// Put one plugin's folder back to its package's data. The user's edits in it, committed or not,
/// are dropped; the rest of the live folder, including uncommitted edits elsewhere, stays. Returns
/// `reset` with the files put back, or `up_to_date`.
pub fn reset_plugin(repo: &Repository, package: &Package, version: &str) -> Result<(&'static str, Vec<String>)> {
    let id = package.id.as_str();
    let prefix = format!("plugins/{id}/");
    let base = repo.find_reference(SHIPPED).and_then(|r| r.peel_to_tree()).map_err(err)?;
    let new_shipped_id = released_tree(repo, &base, package)?;
    let new_shipped = repo.find_tree(new_shipped_id).map_err(err)?;
    let released = new_shipped.get_path(Path::new(&format!("plugins/{id}"))).ok().map(|entry| entry.id());
    let head = repo.find_reference(MAIN).and_then(|r| r.peel_to_commit()).map_err(err)?;
    let head_tree = head.tree().map_err(err)?;
    let result = repo.find_tree(with_plugin(repo, &head_tree, id, released)?).map_err(err)?;
    let mut files = changed_files(repo, Some(&head_tree), Some(&result))?;
    // Uncommitted changes in the folder: edits go back with the checkout, additions are removed.
    let uncommitted: Vec<String> = changed_paths(repo)?.into_iter().filter(|file| file.starts_with(&prefix)).collect();
    if files.is_empty() && uncommitted.is_empty() && base.id() == new_shipped_id {
        return Ok(("up_to_date", Vec::new()));
    }
    let work = repo.workdir().ok_or("the live folder has no work tree")?.to_path_buf();
    for file in &uncommitted {
        if result.get_path(Path::new(file)).is_err() {
            let _ = std::fs::remove_file(work.join(file));
        }
    }
    files.extend(uncommitted);
    files.sort();
    files.dedup();
    // The plugin's folder only: the user's uncommitted edits elsewhere are not touched.
    let mut builder = CheckoutBuilder::new();
    builder.force().path(format!("plugins/{id}"));
    repo.checkout_tree(result.as_object(), Some(&mut builder)).map_err(err)?;
    let shipped = shipped_commit(repo, new_shipped_id)?;
    let shipped_parent = repo.find_commit(shipped).map_err(err)?;
    let text = message(
        &format!("Reset {id} to its release {version}"),
        &[("Origin", "update".into()), ("Plugins", id.to_owned()), ("Shipped-Tree", new_shipped_id.to_string())],
    );
    let sig = signature(now_ms())?;
    let commit = repo.commit(None, &sig, &sig, &text, &result, &[&head, &shipped_parent]).map_err(err)?;
    repo.reference(MAIN, commit, true, "reset").map_err(err)?;
    repo.reference(SHIPPED, shipped, true, "reset").map_err(err)?;
    set_index(repo, &result)?;
    Ok(("reset", files))
}

/// Merge one package's data into the live folder, keeping the user's edits.
pub fn release(repo: &Repository, package: &Package, version: &str) -> Result<Updated> {
    let base = repo.find_reference(SHIPPED).and_then(|r| r.peel_to_tree()).map_err(err)?;
    let new_tree = released_tree(repo, &base, package)?;
    merge_shipped(repo, new_tree, &format!("Updated {} to {version}", package.id))
}

/// The files of `prefix` (`plugins/<id>/`) the user changed away from the packages' data,
/// committed or not.
pub fn edited(repo: &Repository, prefix: &str) -> Result<Vec<String>> {
    let shipped = repo.find_reference(SHIPPED).and_then(|r| r.peel_to_tree()).map_err(err)?;
    let head = repo.find_reference(MAIN).and_then(|r| r.peel_to_tree()).map_err(err)?;
    let mut files: Vec<String> = changed_files(repo, Some(&shipped), Some(&head))?.into_iter().filter(|file| file.starts_with(prefix)).collect();
    files.extend(changed_paths(repo)?.into_iter().filter(|file| file.starts_with(prefix)));
    files.sort();
    files.dedup();
    Ok(files)
}

/// Merge a new shipped tree into the live folder: three trees, the current shipped tree as base.
fn merge_shipped(repo: &Repository, new_tree_id: Oid, summary: &str) -> Result<Updated> {
    let base = repo.find_reference(SHIPPED).and_then(|r| r.peel_to_commit()).map_err(err)?;
    let base_tree = base.tree().map_err(err)?;
    if base_tree.id() == new_tree_id {
        return Ok(Updated { state: "up_to_date", conflicts: Vec::new() });
    }
    let new_tree = repo.find_tree(new_tree_id).map_err(err)?;
    let head = repo.find_reference(MAIN).and_then(|r| r.peel_to_commit()).map_err(err)?;
    let head_tree = head.tree().map_err(err)?;
    let result = if head_tree.id() == base_tree.id() {
        new_tree_id
    } else {
        let mut index = repo.merge_trees(&base_tree, &head_tree, &new_tree, None).map_err(err)?;
        if index.has_conflicts() {
            return Ok(Updated { state: "conflicts", conflicts: conflict_paths(&index) });
        }
        index.write_tree_to(repo).map_err(err)?
    };
    let result_tree = repo.find_tree(result).map_err(err)?;
    let blocked = checkout(repo, &result_tree, false)?;
    if !blocked.is_empty() {
        return Ok(Updated { state: "conflicts", conflicts: blocked });
    }
    let shipped = shipped_commit(repo, new_tree_id)?;
    let shipped_parent = repo.find_commit(shipped).map_err(err)?;
    let text = message(summary, &[("Origin", "update".into()), ("Shipped-Tree", new_tree_id.to_string())]);
    let sig = signature(now_ms())?;
    let merged = repo.commit(None, &sig, &sig, &text, &result_tree, &[&head, &shipped_parent]).map_err(err)?;
    repo.reference(MAIN, merged, true, "update").map_err(err)?;
    repo.reference(SHIPPED, shipped, true, "update").map_err(err)?;
    set_index(repo, &result_tree)?;
    Ok(Updated { state: "merged", conflicts: Vec::new() })
}

fn conflict_paths(index: &Index) -> Vec<String> {
    let mut out = Vec::new();
    if let Ok(conflicts) = index.conflicts() {
        for conflict in conflicts.flatten() {
            if let Some(entry) = conflict.our.or(conflict.their).or(conflict.ancestor) {
                out.push(String::from_utf8_lossy(&entry.path).into_owned());
            }
        }
    }
    out.sort();
    out.dedup();
    out
}

/// Check `tree` out over the work tree. Safe mode refuses to overwrite local edits and returns
/// the files that blocked it; `force` overwrites them.
fn checkout(repo: &Repository, tree: &Tree, force: bool) -> Result<Vec<String>> {
    let blocked = std::cell::RefCell::new(Vec::new());
    let outcome = {
        let mut builder = CheckoutBuilder::new();
        if force {
            builder.force().remove_untracked(false);
        } else {
            builder.safe().notify_on(git2::CheckoutNotificationType::CONFLICT).notify(|_, path, _, _, _| {
                if let Some(path) = path {
                    blocked.borrow_mut().push(path.to_string_lossy().into_owned());
                }
                true
            });
        }
        repo.checkout_tree(tree.as_object(), Some(&mut builder))
    };
    let blocked = blocked.into_inner();
    match outcome {
        Err(error) if blocked.is_empty() => Err(err(error)),
        _ => Ok(blocked),
    }
}

fn set_index(repo: &Repository, tree: &Tree) -> Result<()> {
    let mut index = repo.index().map_err(err)?;
    index.read_tree(tree).map_err(err)?;
    index.write().map_err(err)
}

// ----- working tree -----

/// Paths with uncommitted changes (modified, added, deleted), ignored files left out.
pub fn changed_paths(repo: &Repository) -> Result<Vec<String>> {
    let mut options = StatusOptions::new();
    options.include_untracked(true).recurse_untracked_dirs(true).include_ignored(false).exclude_submodules(true).renames_head_to_index(false);
    let statuses = repo.statuses(Some(&mut options)).map_err(err)?;
    let mut out: Vec<String> = statuses.iter().filter(|entry| entry.status() != Status::CURRENT && entry.status() != Status::IGNORED).filter_map(|entry| entry.path().ok().map(str::to_owned)).collect();
    out.sort();
    Ok(out)
}

/// Stage `files` as they are in the work tree (a missing file is a deletion) into `index`.
fn stage(repo: &Repository, index: &mut Index, files: &[String]) -> Result<()> {
    let work = repo.workdir().ok_or("the live folder has no work tree")?;
    for file in files {
        if work.join(file).exists() {
            index.add_path(Path::new(file)).map_err(err)?;
        } else {
            let _ = index.remove_path(Path::new(file));
        }
    }
    Ok(())
}

/// Commit `files` from the work tree on the branch.
pub fn commit_files(repo: &Repository, files: &[String], text: &str) -> Result<Oid> {
    let mut index = repo.index().map_err(err)?;
    stage(repo, &mut index, files)?;
    index.write().map_err(err)?;
    let tree = repo.find_tree(index.write_tree().map_err(err)?).map_err(err)?;
    let head = repo.find_reference(MAIN).and_then(|r| r.peel_to_commit()).map_err(err)?;
    let sig = signature(now_ms())?;
    repo.commit(Some(MAIN), &sig, &sig, text, &tree, &[&head]).map_err(err)
}

// ----- history -----

/// The files that differ between two trees.
pub fn changed_files(repo: &Repository, old: Option<&Tree>, new: Option<&Tree>) -> Result<Vec<String>> {
    let diff = repo.diff_tree_to_tree(old, new, None).map_err(err)?;
    let mut out: Vec<String> = diff
        .deltas()
        .filter_map(|delta| delta.new_file().path().or_else(|| delta.old_file().path()).map(|p| p.to_string_lossy().into_owned()))
        .collect();
    out.sort();
    out.dedup();
    Ok(out)
}

/// The branch's changes, newest first (first parents only), at most `limit`.
pub fn history(repo: &Repository, limit: usize) -> Result<Vec<Change>> {
    let mut changes: Vec<Change> = Vec::new();
    let mut reverted: HashMap<String, String> = HashMap::new();
    let mut commit = repo.find_reference(MAIN).and_then(|r| r.peel_to_commit()).map_err(err)?;
    loop {
        // A root commit is the shipped data itself: the starting point, not a change.
        let Ok(parent) = commit.parent(0) else { break };
        let (summary, trailers) = parse_message(commit.message().unwrap_or_default());
        let origin = trailers.get("Origin").cloned().unwrap_or_else(|| "edit".into());
        let parent_tree = parent.tree().ok();
        let tree = commit.tree().map_err(err)?;
        // What the commit changed against its first parent. An update or restore lists no files of
        // its own (they can be many).
        let changed = changed_files(repo, parent_tree.as_ref(), Some(&tree))?;
        let files = if matches!(origin.as_str(), "update" | "restore") { Vec::new() } else { changed };
        let id = commit.id().to_string();
        if let Some(target) = trailers.get("Reverts") {
            reverted.insert(target.clone(), id.clone());
        }
        let plugins = match trailers.get("Plugins") {
            Some(list) => list.split(", ").filter(|s| !s.is_empty()).map(str::to_owned).collect(),
            None => plugins_of(&files),
        };
        changes.push(Change {
            reverted_by: reverted.get(&id).cloned(),
            commit: id,
            ms: u64::try_from(commit.time().seconds()).unwrap_or(0) * 1000,
            origin,
            summary,
            plugins,
            files,
            shared: None,
        });
        if changes.len() >= limit {
            break;
        }
        commit = parent;
    }
    Ok(changes)
}

/// The plugins `files` touch.
pub fn plugins_of(files: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for file in files {
        if let Some(id) = paths::plugin_of(file)
            && !out.iter().any(|known| known == id)
        {
            out.push(id.to_owned());
        }
    }
    out
}

/// Revert one change of the branch with a new commit; returns it and the files it changed.
pub fn revert(repo: &Repository, commit: &str) -> Result<(Oid, Vec<String>)> {
    let target = repo.find_commit(Oid::from_str(commit).map_err(err)?).map_err(err)?;
    let (summary, trailers) = parse_message(target.message().unwrap_or_default());
    if target.parent_count() == 0 {
        return Err("the shipped data itself cannot be reverted".into());
    }
    let head = repo.find_reference(MAIN).and_then(|r| r.peel_to_commit()).map_err(err)?;
    let mainline = if target.parent_count() > 1 { 1 } else { 0 };
    let mut index = repo.revert_commit(&target, &head, mainline, None).map_err(err)?;
    if index.has_conflicts() {
        return Err(format!("later changes touch the same lines: {}", conflict_paths(&index).join(", ")));
    }
    let tree = repo.find_tree(index.write_tree_to(repo).map_err(err)?).map_err(err)?;
    let blocked = checkout(repo, &tree, false)?;
    if !blocked.is_empty() {
        return Err(format!("unsaved edits in {} would be overwritten", blocked.join(", ")));
    }
    let files = changed_files(repo, Some(&head.tree().map_err(err)?), Some(&tree))?;
    let text = message(
        &format!("Reverted: {summary}"),
        &[("Origin", "revert".into()), ("Plugins", trailers.get("Plugins").cloned().unwrap_or_else(|| plugins_of(&files).join(", "))), ("Reverts", target.id().to_string())],
    );
    let sig = signature(now_ms())?;
    let oid = repo.commit(Some(MAIN), &sig, &sig, &text, &tree, &[&head]).map_err(err)?;
    set_index(repo, &tree)?;
    Ok((oid, files))
}

// ----- diffs -----

/// Unified diffs per file between two trees.
pub fn diff_trees(repo: &Repository, old: Option<&Tree>, new: Option<&Tree>) -> Result<Vec<FileDiff>> {
    let diff = repo.diff_tree_to_tree(old, new, None).map_err(err)?;
    let mut files: Vec<FileDiff> = Vec::new();
    diff.print(DiffFormat::Patch, |delta, _, line| {
        let path = delta.new_file().path().or_else(|| delta.old_file().path()).map(|p| p.to_string_lossy().into_owned()).unwrap_or_default();
        if files.last().is_none_or(|last| last.path != path) {
            let status = match delta.status() {
                Delta::Added => "added",
                Delta::Deleted => "deleted",
                Delta::Renamed => "renamed",
                _ => "modified",
            };
            files.push(FileDiff { path: path.clone(), status: status.into(), patch: String::new() });
        }
        if let Some(file) = files.last_mut() {
            let origin = line.origin();
            if matches!(origin, '+' | '-' | ' ') {
                file.patch.push(origin);
            }
            file.patch.push_str(&String::from_utf8_lossy(line.content()));
        }
        true
    })
    .map_err(err)?;
    Ok(files)
}

/// A commit's change (against its first parent).
pub fn diff_commit(repo: &Repository, commit: Oid) -> Result<Vec<FileDiff>> {
    let commit = repo.find_commit(commit).map_err(err)?;
    let parent = commit.parent(0).map_err(|_| "the shipped data is the starting point, not a change; there is nothing to show against".to_owned())?;
    diff_trees(repo, Some(&parent.tree().map_err(err)?), Some(&commit.tree().map_err(err)?))
}

// ----- remotes -----

fn callbacks<'a>(login: &'a str, token: &'a str, rejected: &'a std::cell::RefCell<Option<String>>) -> RemoteCallbacks<'a> {
    let mut callbacks = RemoteCallbacks::new();
    callbacks.credentials(move |_, _, _| Cred::userpass_plaintext(login, token));
    callbacks.push_update_reference(move |reference, status| {
        if let Some(status) = status {
            *rejected.borrow_mut() = Some(format!("{reference}: {status}"));
        }
        Ok(())
    });
    callbacks
}

/// Push `refspec` to `url` with the GitHub connection's credentials.
pub fn push(repo: &Repository, url: &str, login: &str, token: &str, refspec: &str) -> Result<()> {
    let rejected = std::cell::RefCell::new(None);
    {
        let mut remote = repo.remote_anonymous(url).map_err(err)?;
        let mut options = PushOptions::new();
        options.remote_callbacks(callbacks(login, token, &rejected));
        remote.push(&[refspec], Some(&mut options)).map_err(err)?;
    }
    match rejected.into_inner() {
        Some(reason) => Err(format!("GitHub refused the push ({reason})")),
        None => Ok(()),
    }
}

/// Fetch `refspec` from `url` (credentials when given).
pub fn fetch(repo: &Repository, url: &str, credentials: Option<(&str, &str)>, refspec: &str) -> Result<()> {
    let rejected = std::cell::RefCell::new(None);
    let mut remote = repo.remote_anonymous(url).map_err(err)?;
    let mut options = FetchOptions::new();
    if let Some((login, token)) = credentials {
        options.remote_callbacks(callbacks(login, token, &rejected));
    }
    remote.fetch(&[refspec], Some(&mut options), None).map_err(err)
}

/// The shipped tree a branch's changes were made on: the `Shipped-Tree` trailer of its newest
/// shipped, update or restore commit (first parents).
pub fn shipped_base(repo: &Repository, tip: Oid) -> Result<Oid> {
    let mut commit = repo.find_commit(tip).map_err(err)?;
    loop {
        let (_, trailers) = parse_message(commit.message().unwrap_or_default());
        if let Some(tree) = trailers.get("Shipped-Tree") {
            return Oid::from_str(tree).map_err(err);
        }
        commit = commit.parent(0).map_err(|_| "the backup does not say which plugin data it was made on".to_owned())?;
    }
}

/// Replace the branch with a fetched backup, carried onto the packages' data this Mac has.
pub fn restore(repo: &Repository, backup: Oid) -> Result<Oid> {
    let backup_commit = repo.find_commit(backup).map_err(err)?;
    let backup_tree = backup_commit.tree().map_err(err)?;
    let current = repo.find_reference(SHIPPED).and_then(|r| r.peel_to_commit()).map_err(err)?;
    let current_tree = current.tree().map_err(err)?;
    let base = repo.find_tree(shipped_base(repo, backup)?).map_err(|_| "the backup was made on plugin data this Mac never had".to_owned())?;
    let result = if base.id() == current_tree.id() {
        backup_tree.id()
    } else {
        let mut index = repo.merge_trees(&base, &backup_tree, &current_tree, None).map_err(err)?;
        if index.has_conflicts() {
            return Err(format!("the backup's changes conflict with this version: {}", conflict_paths(&index).join(", ")));
        }
        index.write_tree_to(repo).map_err(err)?
    };
    let tree = repo.find_tree(result).map_err(err)?;
    checkout(repo, &tree, true)?;
    let text = message("Restored from backup", &[("Origin", "restore".into()), ("Shipped-Tree", current_tree.id().to_string())]);
    let sig = signature(now_ms())?;
    let oid = repo.commit(None, &sig, &sig, &text, &tree, &[&backup_commit, &current]).map_err(err)?;
    repo.reference(MAIN, oid, true, "restore").map_err(err)?;
    set_index(repo, &tree)?;
    Ok(oid)
}

/// Commits `commits` (in order) re-applied onto `onto` as a chain; returns the tip. Each new commit
/// keeps its summary and gets `extra` appended to its message.
pub fn replay(repo: &Repository, onto: Oid, commits: &[Oid], extra: &str) -> Result<Oid> {
    let mut tip = repo.find_commit(onto).map_err(err)?;
    for oid in commits {
        let commit = repo.find_commit(*oid).map_err(err)?;
        let mainline = if commit.parent_count() > 1 { 1 } else { 0 };
        let mut index = repo.cherrypick_commit(&commit, &tip, mainline, None).map_err(err)?;
        let (summary, _) = parse_message(commit.message().unwrap_or_default());
        if index.has_conflicts() {
            return Err(format!("“{summary}” does not apply to the official repository: {} changed there", conflict_paths(&index).join(", ")));
        }
        let tree = repo.find_tree(index.write_tree_to(repo).map_err(err)?).map_err(err)?;
        let sig = signature(now_ms())?;
        let text = format!("{summary}\n\n{extra}\n");
        let new = repo.commit(None, &sig, &sig, &text, &tree, &[&tip]).map_err(err)?;
        tip = repo.find_commit(new).map_err(err)?;
    }
    Ok(tip.id())
}

/// The shipped commit.
pub fn shipped(repo: &Repository) -> Option<String> {
    repo.find_reference(SHIPPED).and_then(|r| r.peel_to_commit()).ok().map(|c| c.id().to_string())
}

// ----- the code folder this folder replaced -----

/// Carry the user's data edits from the code folder (`old`: CortexCode's full source, managed by
/// an earlier version of this plugin) into the live folder: every data file whose content in the
/// old work tree differs from the old shipped source, where the live folder does not already have
/// that content. Only plugins the live folder holds. Returns the files written (not committed).
pub fn carry_over(old: &Path, repo: &Repository) -> Result<Vec<String>> {
    let old_repo = open(old)?;
    let base = old_repo.find_reference(SHIPPED).and_then(|r| r.peel_to_tree()).map_err(err)?;
    let old_work = old_repo.workdir().ok_or("the code folder has no work tree")?.to_path_buf();
    let work = repo.workdir().ok_or("the live folder has no work tree")?.to_path_buf();
    let head = repo.find_reference(MAIN).and_then(|r| r.peel_to_tree()).map_err(err)?;
    let blob = |repo: &Repository, tree: &Tree, file: &str| -> Option<Vec<u8>> {
        let entry = tree.get_path(Path::new(file)).ok()?;
        repo.find_blob(entry.id()).ok().map(|blob| blob.content().to_vec())
    };
    // The data files the old shipped source had, and the ones its work tree has now.
    let mut files: BTreeSet<String> = BTreeSet::new();
    base.walk(TreeWalkMode::PreOrder, |dir, entry| {
        if entry.kind() == Some(ObjectType::Blob)
            && let Ok(name) = entry.name()
        {
            files.insert(format!("{dir}{name}"));
        }
        TreeWalkResult::Ok
    })
    .map_err(err)?;
    if let Ok(plugins) = std::fs::read_dir(old_work.join("plugins")) {
        for plugin in plugins.flatten() {
            let id = plugin.file_name().to_string_lossy().into_owned();
            let mut found = Vec::new();
            for dir in ["data", "backend/data"] {
                files_under(&old_work, &format!("plugins/{id}/{dir}"), &mut found)?;
            }
            files.extend(found.into_iter().map(|(file, _)| file));
        }
    }
    let mut carried = Vec::new();
    for file in files.into_iter().filter(|file| paths::is_data(file)) {
        let Some(id) = paths::plugin_of(&file) else { continue };
        if head.get_path(Path::new(&format!("plugins/{id}"))).is_err() {
            continue;
        }
        let user = std::fs::read(old_work.join(&file)).ok();
        if user == blob(&old_repo, &base, &file) || user == blob(repo, &head, &file) {
            continue;
        }
        let target = work.join(&file);
        match &user {
            Some(bytes) => {
                if let Some(parent) = target.parent() {
                    std::fs::create_dir_all(parent).map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
                }
                std::fs::write(&target, bytes).map_err(|e| format!("cannot write {}: {e}", target.display()))?;
            }
            None => {
                let _ = std::fs::remove_file(&target);
            }
        }
        carried.push(file);
    }
    Ok(carried)
}
