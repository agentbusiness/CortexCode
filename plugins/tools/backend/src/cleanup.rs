//! What a turn leaves behind that is nobody's work, removed when the turn ends (the host does it,
//! never the model): the scratch folder `tmp/` (one-off tries) and new untracked files the turn's
//! commands made (build output, caches, logs; seen: a C++ `build/` folder in the change's diff).
//!
//! Kept: every file `edit` wrote in the session (the model's work: code, tests), files git
//! ignores (the project's own build and dependency folders), files that existed before the turn,
//! the workspace's `.cortex-*` files, and everything when the session's user let a command past the
//! sandbox (an install the user approved makes files they asked for).

use crate::core::Core;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

/// Remove what the turn of `session_id` that started at `since_ms` left behind in `workspace`;
/// returns the removed paths, relative to the workspace.
pub fn turn_end(core: &Core, workspace: &Path, session_id: &str, since_ms: u64) -> Vec<String> {
    let mut removed = Vec::new();
    if !workspace.is_dir() {
        return removed;
    }
    let scratch = crate::util::scratch_dir(workspace);
    if scratch.is_dir() && std::fs::remove_dir_all(&scratch).is_ok() {
        removed.push("tmp/".to_owned());
    }
    if restore_gitignore(workspace) {
        removed.push(".gitignore: the /tmp/ line".to_owned());
    }
    if core.approved(session_id).iter().any(|class| class.starts_with(cc_protocol::contracts::tools::approval_classes::LIFT_PREFIX)) {
        return removed;
    }
    let edited = core.edited.lock().map(|edited| edited.get(session_id).cloned().unwrap_or_default()).unwrap_or_default();
    for path in new_untracked(workspace, since_ms) {
        let relative = path.strip_prefix(workspace).map(Path::to_path_buf).unwrap_or_else(|_| path.clone());
        let name = relative.to_string_lossy();
        if edited.contains(&path) || name.starts_with(".cortex") || name.starts_with(".git/") {
            continue;
        }
        if std::fs::remove_file(&path).is_ok() {
            removed.push(name.into_owned());
            remove_empty_parents(&path, workspace);
        }
    }
    removed
}

/// Take back the `/tmp/` line the scratch folder added to a tracked `.gitignore`, when it is the
/// file's only change (a `.gitignore` the turn created is a new untracked file, removed below).
fn restore_gitignore(workspace: &Path) -> bool {
    let Ok(output) = std::process::Command::new("git")
        .args(["diff", "--unified=0", "--no-color", "--", ".gitignore"])
        .current_dir(workspace)
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
    else {
        return false;
    };
    let diff = String::from_utf8_lossy(&output.stdout);
    let changes: Vec<&str> = diff.lines().filter(|line| (line.starts_with('+') || line.starts_with('-')) && !line.starts_with("+++") && !line.starts_with("---")).collect();
    if changes != ["+/tmp/"] {
        return false;
    }
    let file = workspace.join(".gitignore");
    let Ok(text) = std::fs::read_to_string(&file) else { return false };
    let kept: Vec<&str> = text.lines().filter(|line| *line != "/tmp/").collect();
    let mut restored = kept.join("\n");
    if text.ends_with('\n') && !restored.is_empty() {
        restored.push('\n');
    }
    std::fs::write(&file, restored).is_ok()
}

/// Untracked files git does not ignore, created at or after `since_ms` (their birth time, so a file
/// that existed before the turn and was only changed stays). Empty outside a git repository.
fn new_untracked(workspace: &Path, since_ms: u64) -> Vec<PathBuf> {
    let Ok(output) = std::process::Command::new("git")
        .args(["status", "--porcelain=v1", "-z", "--untracked-files=all"])
        .current_dir(workspace)
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
    else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }
    String::from_utf8_lossy(&output.stdout)
        .split('\0')
        .filter_map(|entry| entry.strip_prefix("?? "))
        .map(|relative| workspace.join(relative))
        .filter(|path| born_ms(path).is_some_and(|born| born >= since_ms))
        .collect()
}

fn born_ms(path: &Path) -> Option<u64> {
    let meta = std::fs::symlink_metadata(path).ok()?;
    if !meta.is_file() && !meta.file_type().is_symlink() {
        return None;
    }
    let born = meta.created().ok()?;
    Some(born.duration_since(UNIX_EPOCH).ok()?.as_millis() as u64)
}

/// The folders above `path` (inside `workspace`) that are empty once it is gone.
fn remove_empty_parents(path: &Path, workspace: &Path) {
    let mut dir = path.parent();
    while let Some(folder) = dir {
        if folder == workspace || !folder.starts_with(workspace) || std::fs::remove_dir(folder).is_err() {
            break;
        }
        dir = folder.parent();
    }
}
