//! The code folder as one git repository, through git as a library (no git program needed).
//!
//! - `refs/heads/main` is the code folder's branch: the shipped source plus the user's changes.
//! - `refs/cortexcode/shipped` is the shipped source the folder was last updated from. A shipped
//!   commit is a root commit with fixed metadata, so the same source gives the same commit on
//!   every Mac, and an update or a restore merges three trees with an explicit base (the shipped
//!   tree the changes were made on), never by shared history.
//! - `refs/cortexcode/drafts/<id>` holds a proposed code change until the user answers.
//!
//! Every commit carries trailers the history reads back: `Origin:` (shipped | update | edit |
//! approved | revert | restore), `Plugins:`, `Shipped-Tree:` (shipped and update commits) and
//! `Reverts:`.

use crate::paths;
use cc_protocol::contracts::source::{Change, FileDiff};
use git2::{
    Cred, Delta, DiffFormat, FetchOptions, FileMode, Index, Oid, PushOptions, RemoteCallbacks, Repository,
    RepositoryInitOptions, Signature, Status, StatusOptions, Time, Tree, build::CheckoutBuilder,
};
use std::collections::{BTreeMap, HashMap};
use std::path::Path;

pub const MAIN: &str = "refs/heads/main";
pub const SHIPPED: &str = "refs/cortexcode/shipped";
pub const DRAFTS: &str = "refs/cortexcode/drafts/";
pub const BACKUP: &str = "refs/cortexcode/backup";
pub const UPSTREAM: &str = "refs/cortexcode/upstream";

/// Folders never taken from the shipped source (build outputs, installs).
const SKIPPED_DIRS: &[&str] = &["target", "node_modules", "dist", ".git"];

type Result<T> = std::result::Result<T, String>;

fn err(error: git2::Error) -> String {
    error.message().to_owned()
}

/// Whether `dir` is a code folder this plugin manages (it has a shipped commit).
pub fn is_managed(dir: &Path) -> bool {
    Repository::open(dir).ok().is_some_and(|repo| repo.find_reference(SHIPPED).is_ok())
}

pub fn open(dir: &Path) -> Result<Repository> {
    Repository::open(dir).map_err(|e| format!("cannot open the code folder {}: {}", dir.display(), e.message()))
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

// ----- the shipped source -----

/// The files of the shipped source, relative paths: what git tracks or would track when it is a
/// git work tree (ignored files left out), every file but build outputs otherwise.
fn shipped_files(source: &Path) -> Result<Vec<String>> {
    let mut files: Vec<String> = Vec::new();
    if let Ok(repo) = Repository::open(source)
        && !repo.is_bare()
        && repo.workdir().is_some_and(|work| same_dir(work, source))
    {
        let index = repo.index().map_err(err)?;
        files.extend(index.iter().map(|entry| String::from_utf8_lossy(&entry.path).into_owned()));
        let mut options = StatusOptions::new();
        options.include_untracked(true).recurse_untracked_dirs(true).include_ignored(false).exclude_submodules(true);
        for entry in repo.statuses(Some(&mut options)).map_err(err)?.iter() {
            if entry.status().contains(Status::WT_NEW)
                && let Ok(path) = entry.path()
            {
                files.push(path.to_owned());
            }
        }
    } else {
        walk(source, source, &mut files)?;
    }
    // Regular files only (a tracked file deleted from the work tree is not part of the source).
    files.retain(|file| std::fs::symlink_metadata(source.join(file)).is_ok_and(|meta| meta.is_file()));
    files.sort();
    files.dedup();
    Ok(files)
}

fn same_dir(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

fn walk(root: &Path, dir: &Path, out: &mut Vec<String>) -> Result<()> {
    let entries = std::fs::read_dir(dir).map_err(|e| format!("cannot read {}: {e}", dir.display()))?;
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let Ok(kind) = entry.file_type() else { continue };
        if kind.is_dir() {
            if !SKIPPED_DIRS.contains(&name.as_str()) {
                walk(root, &entry.path(), out)?;
            }
        } else if kind.is_file()
            && let Ok(relative) = entry.path().strip_prefix(root)
        {
            out.push(relative.to_string_lossy().into_owned());
        }
    }
    Ok(())
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

/// Write the shipped source's files into `repo` as a tree.
pub fn shipped_tree(repo: &Repository, source: &Path) -> Result<Oid> {
    use std::os::unix::fs::PermissionsExt;
    let mut root: BTreeMap<String, Node> = BTreeMap::new();
    for file in shipped_files(source)? {
        let path = source.join(&file);
        let bytes = std::fs::read(&path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        let executable = std::fs::metadata(&path).is_ok_and(|meta| meta.permissions().mode() & 0o111 != 0);
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

/// The shipped commit for `tree`: a root commit with fixed metadata (the same on every Mac).
fn shipped_commit(repo: &Repository, tree: Oid) -> Result<Oid> {
    let tree = repo.find_tree(tree).map_err(err)?;
    let sig = signature(0)?;
    let text = message("CortexCode source", &[("Origin", "shipped".into()), ("Shipped-Tree", tree.id().to_string())]);
    repo.commit(None, &sig, &sig, &text, &tree, &[]).map_err(err)
}

/// Create the code folder from the shipped source.
pub fn populate(dir: &Path, source: &Path) -> Result<()> {
    std::fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    let mut options = RepositoryInitOptions::new();
    options.initial_head("main");
    let repo = Repository::init_opts(dir, &options).map_err(err)?;
    let tree = shipped_tree(&repo, source)?;
    let commit = shipped_commit(&repo, tree)?;
    repo.reference(MAIN, commit, true, "shipped source").map_err(err)?;
    repo.reference(SHIPPED, commit, true, "shipped source").map_err(err)?;
    repo.set_head(MAIN).map_err(err)?;
    repo.checkout_head(Some(CheckoutBuilder::new().force())).map_err(err)?;
    Ok(())
}

pub struct Updated {
    /// up_to_date | merged | conflicts
    pub state: &'static str,
    pub conflicts: Vec<String>,
    /// Code files where the result differs from the new shipped source (the user's own code
    /// changes, which must be rebuilt on the new base).
    pub own_code: Vec<String>,
}

/// Merge the shipped source into the code folder.
pub fn update(repo: &Repository, source: &Path) -> Result<Updated> {
    let new_tree_id = shipped_tree(repo, source)?;
    let base = repo.find_reference(SHIPPED).and_then(|r| r.peel_to_commit()).map_err(err)?;
    let base_tree = base.tree().map_err(err)?;
    if base_tree.id() == new_tree_id {
        return Ok(Updated { state: "up_to_date", conflicts: Vec::new(), own_code: Vec::new() });
    }
    let new_tree = repo.find_tree(new_tree_id).map_err(err)?;
    let head = repo.find_reference(MAIN).and_then(|r| r.peel_to_commit()).map_err(err)?;
    let head_tree = head.tree().map_err(err)?;
    let result = if head_tree.id() == base_tree.id() {
        new_tree_id
    } else {
        let mut index = repo.merge_trees(&base_tree, &head_tree, &new_tree, None).map_err(err)?;
        if index.has_conflicts() {
            return Ok(Updated { state: "conflicts", conflicts: conflict_paths(&index), own_code: Vec::new() });
        }
        index.write_tree_to(repo).map_err(err)?
    };
    let result_tree = repo.find_tree(result).map_err(err)?;
    let blocked = checkout(repo, &result_tree, false)?;
    if !blocked.is_empty() {
        return Ok(Updated { state: "conflicts", conflicts: blocked, own_code: Vec::new() });
    }
    let shipped = shipped_commit(repo, new_tree_id)?;
    let shipped_parent = repo.find_commit(shipped).map_err(err)?;
    let text = message("Updated to the shipped source", &[("Origin", "update".into()), ("Shipped-Tree", new_tree_id.to_string())]);
    let sig = signature(now_ms())?;
    let merged = repo.commit(None, &sig, &sig, &text, &result_tree, &[&head, &shipped_parent]).map_err(err)?;
    repo.reference(MAIN, merged, true, "update").map_err(err)?;
    repo.reference(SHIPPED, shipped, true, "update").map_err(err)?;
    set_index(repo, &result_tree)?;
    let own_code = changed_files(repo, Some(&new_tree), Some(&result_tree))?.into_iter().filter(|file| !paths::is_data(file)).collect();
    Ok(Updated { state: "merged", conflicts: Vec::new(), own_code })
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
    let work = repo.workdir().ok_or("the code folder has no work tree")?;
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

/// A draft commit of `files` (from the work tree) on top of the branch, kept under
/// `refs/cortexcode/drafts/<id>`; the branch and the index are left as they are.
pub fn draft(repo: &Repository, id: &str, files: &[String], text: &str) -> Result<Oid> {
    let head = repo.find_reference(MAIN).and_then(|r| r.peel_to_commit()).map_err(err)?;
    let mut index = Index::new().map_err(err)?;
    index.read_tree(&head.tree().map_err(err)?).map_err(err)?;
    let work = repo.workdir().ok_or("the code folder has no work tree")?.to_path_buf();
    for file in files {
        let path = work.join(file);
        if path.is_file() {
            let bytes = std::fs::read(&path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
            let oid = repo.blob(&bytes).map_err(err)?;
            let mode = file_mode(&path);
            let entry = git2::IndexEntry {
                ctime: git2::IndexTime::new(0, 0),
                mtime: git2::IndexTime::new(0, 0),
                dev: 0,
                ino: 0,
                mode,
                uid: 0,
                gid: 0,
                file_size: bytes.len() as u32,
                id: oid,
                flags: 0,
                flags_extended: 0,
                path: file.as_bytes().to_vec(),
            };
            index.add(&entry).map_err(err)?;
        } else {
            let _ = index.remove_path(Path::new(file));
        }
    }
    let tree = repo.find_tree(index.write_tree_to(repo).map_err(err)?).map_err(err)?;
    let sig = signature(now_ms())?;
    let oid = repo.commit(None, &sig, &sig, text, &tree, &[&head]).map_err(err)?;
    repo.reference(&format!("{DRAFTS}{id}"), oid, true, "draft").map_err(err)?;
    Ok(oid)
}

fn file_mode(path: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    let executable = std::fs::metadata(path).is_ok_and(|meta| meta.permissions().mode() & 0o111 != 0);
    u32::try_from(i32::from(if executable { FileMode::BlobExecutable } else { FileMode::Blob })).unwrap_or(0o100644)
}

/// Whether the work tree still holds exactly what `draft` holds for `files`.
pub fn work_matches(repo: &Repository, draft: Oid, files: &[String]) -> Result<bool> {
    let tree = repo.find_commit(draft).and_then(|c| c.tree()).map_err(err)?;
    let work = repo.workdir().ok_or("the code folder has no work tree")?.to_path_buf();
    for file in files {
        let on_disk = std::fs::read(work.join(file)).ok();
        let in_draft = match tree.get_path(Path::new(file)) {
            Ok(entry) => Some(repo.find_blob(entry.id()).map_err(err)?.content().to_vec()),
            Err(_) => None,
        };
        if on_disk != in_draft {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Put a draft on the branch: a fast-forward when the branch has not moved, else its change applied
/// on top. The work tree already holds the change; the index is brought to the new tree.
pub fn apply_draft(repo: &Repository, draft: Oid) -> Result<Oid> {
    let draft = repo.find_commit(draft).map_err(err)?;
    let head = repo.find_reference(MAIN).and_then(|r| r.peel_to_commit()).map_err(err)?;
    let oid = if draft.parent_id(0).ok() == Some(head.id()) {
        draft.id()
    } else {
        let mut index = repo.cherrypick_commit(&draft, &head, 0, None).map_err(err)?;
        if index.has_conflicts() {
            return Err(format!("the change no longer applies: {} changed since", conflict_paths(&index).join(", ")));
        }
        let tree = repo.find_tree(index.write_tree_to(repo).map_err(err)?).map_err(err)?;
        let sig = signature(now_ms())?;
        repo.commit(None, &sig, &sig, draft.message().unwrap_or_default(), &tree, &[&head]).map_err(err)?
    };
    repo.reference(MAIN, oid, true, "approved").map_err(err)?;
    let tree = repo.find_commit(oid).and_then(|c| c.tree()).map_err(err)?;
    set_index(repo, &tree)?;
    Ok(oid)
}

/// Put `files` back as the branch has them (a file the branch does not have is removed).
pub fn discard(repo: &Repository, files: &[String]) -> Result<()> {
    let head = repo.find_reference(MAIN).and_then(|r| r.peel_to_commit()).and_then(|c| c.tree()).map_err(err)?;
    let work = repo.workdir().ok_or("the code folder has no work tree")?.to_path_buf();
    let mut builder = CheckoutBuilder::new();
    builder.force();
    let mut any = false;
    for file in files {
        if head.get_path(Path::new(file)).is_ok() {
            builder.path(file);
            any = true;
        } else {
            let _ = std::fs::remove_file(work.join(file));
        }
    }
    if any {
        repo.checkout_tree(head.as_object(), Some(&mut builder)).map_err(err)?;
    }
    Ok(())
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
        let (summary, trailers) = parse_message(commit.message().unwrap_or_default());
        let origin = trailers.get("Origin").cloned().unwrap_or_else(|| "edit".into());
        let parent_tree = commit.parent(0).ok().and_then(|p| p.tree().ok());
        let tree = commit.tree().map_err(err)?;
        // A shipped or update commit's own files are the whole source: list none.
        let files = if matches!(origin.as_str(), "shipped" | "update" | "restore") { Vec::new() } else { changed_files(repo, parent_tree.as_ref(), Some(&tree))? };
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
            kind: kind_of(&files).into(),
            files,
            shared: None,
        });
        if changes.len() >= limit {
            break;
        }
        match commit.parent(0) {
            Ok(parent) => commit = parent,
            Err(_) => break,
        }
    }
    Ok(changes)
}

/// The plugins (and app parts) `files` touch.
pub fn plugins_of(files: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for file in files {
        let name = paths::plugin_of(file).map(str::to_owned).or_else(|| paths::app_part(file).map(str::to_owned)).or_else(|| paths::shared_of(file).map(|s| s.trim_end_matches('/').to_owned()));
        if let Some(name) = name
            && !out.contains(&name)
        {
            out.push(name);
        }
    }
    out
}

pub fn kind_of(files: &[String]) -> &'static str {
    let data = files.iter().filter(|file| paths::is_data(file)).count();
    match (data, files.len()) {
        (_, 0) => "data",
        (d, n) if d == n => "data",
        (0, _) => "code",
        _ => "mixed",
    }
}

/// Revert one change of the branch with a new commit; returns it and the files it changed.
pub fn revert(repo: &Repository, commit: &str) -> Result<(Oid, Vec<String>)> {
    let target = repo.find_commit(Oid::from_str(commit).map_err(err)?).map_err(err)?;
    let (summary, trailers) = parse_message(target.message().unwrap_or_default());
    if target.parent_count() == 0 {
        return Err("the shipped source itself cannot be reverted".into());
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
    let parent = commit.parent(0).ok().and_then(|p| p.tree().ok());
    diff_trees(repo, parent.as_ref(), Some(&commit.tree().map_err(err)?))
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
fn shipped_base(repo: &Repository, tip: Oid) -> Result<Oid> {
    let mut commit = repo.find_commit(tip).map_err(err)?;
    loop {
        let (_, trailers) = parse_message(commit.message().unwrap_or_default());
        if let Some(tree) = trailers.get("Shipped-Tree") {
            return Oid::from_str(tree).map_err(err);
        }
        commit = commit.parent(0).map_err(|_| "the backup does not say which shipped source it was made on".to_owned())?;
    }
}

/// Replace the branch with a fetched backup, carried onto the shipped source this Mac has.
/// Returns the new commit and the code files that differ from the shipped source.
pub fn restore(repo: &Repository, backup: Oid) -> Result<(Oid, Vec<String>)> {
    let backup_commit = repo.find_commit(backup).map_err(err)?;
    let backup_tree = backup_commit.tree().map_err(err)?;
    let current = repo.find_reference(SHIPPED).and_then(|r| r.peel_to_commit()).map_err(err)?;
    let current_tree = current.tree().map_err(err)?;
    let base = repo.find_tree(shipped_base(repo, backup)?).map_err(|_| "the backup was made on a shipped source this Mac never had".to_owned())?;
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
    let own = changed_files(repo, Some(&current_tree), Some(&tree))?.into_iter().filter(|file| !paths::is_data(file)).collect();
    Ok((oid, own))
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
            return Err(format!("“{summary}” does not apply to the official source: {} changed there", conflict_paths(&index).join(", ")));
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
