//! Filesystem watching (notify / FSEvents) for one target path that may not exist yet.
//!
//! The watch is placed on the target itself when it is a directory, on its parent when it is a
//! file, and on the nearest existing ancestor (recursively) when it does not exist yet. Events are
//! filtered to the target and forwarded as a bare "something changed" signal; consumers debounce.

use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use std::path::{Path, PathBuf};
use tokio::sync::mpsc;

pub struct FsWatch {
    _watcher: RecommendedWatcher,
}

/// Canonicalize the longest existing prefix of `path` and append the rest (FSEvents reports
/// resolved paths, e.g. `/private/var/...` for `/var/...`).
pub fn canonical_prefix(path: &Path) -> PathBuf {
    let mut existing = path.to_path_buf();
    let mut rest = Vec::new();
    loop {
        if let Ok(canonical) = std::fs::canonicalize(&existing) {
            let mut out = canonical;
            for part in rest.iter().rev() {
                out.push(part);
            }
            return out;
        }
        match (existing.file_name().map(|n| n.to_os_string()), existing.parent().map(Path::to_path_buf)) {
            (Some(name), Some(parent)) => {
                rest.push(name);
                existing = parent;
            }
            _ => return path.to_path_buf(),
        }
    }
}

pub fn watch(target: &Path, tx: mpsc::UnboundedSender<()>) -> notify::Result<FsWatch> {
    let target = canonical_prefix(target);
    let (root, mode) = if target.is_dir() {
        (target.clone(), RecursiveMode::Recursive)
    } else if target.exists() {
        (target.parent().map(Path::to_path_buf).unwrap_or_else(|| target.clone()), RecursiveMode::NonRecursive)
    } else {
        let mut ancestor = target.parent().map(Path::to_path_buf);
        while let Some(candidate) = &ancestor {
            if candidate.is_dir() {
                break;
            }
            ancestor = candidate.parent().map(Path::to_path_buf);
        }
        (ancestor.unwrap_or_else(|| PathBuf::from("/")), RecursiveMode::Recursive)
    };
    let filter = target.clone();
    let mut watcher = notify::recommended_watcher(move |event: notify::Result<notify::Event>| match event {
        Ok(event) => {
            if matches!(event.kind, EventKind::Access(_)) {
                return;
            }
            if event.paths.iter().any(|path| path.starts_with(&filter)) || event.need_rescan() {
                let _ = tx.send(());
            }
        }
        Err(error) => tracing::warn!(%error, "file watch error"),
    })?;
    watcher.watch(&root, mode)?;
    tracing::debug!(target = %target.display(), root = %root.display(), ?mode, "watching");
    Ok(FsWatch { _watcher: watcher })
}
