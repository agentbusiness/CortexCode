//! File-system watching (FSEvents on macOS through `notify`) with a short debounce.

use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tokio::sync::mpsc;

/// Quiet period that ends a burst of events, and the longest a burst is held.
pub const DEBOUNCE_QUIET: Duration = Duration::from_millis(60);
pub const DEBOUNCE_MAX: Duration = Duration::from_millis(500);

/// Both the raw channel and one extracted batch are capped. Overflow is repaired by a scan.
const MAX_PENDING_PATHS: usize = 4096;

pub struct Batch {
    pub paths: Vec<PathBuf>,
    pub reconcile: bool,
}

/// Ignore/structural rules change membership, including files that no longer produce events.
pub fn changes_membership(root: &Path, path: &Path) -> bool {
    path == root || path.file_name().is_some_and(|name| matches!(name.to_str(), Some(".gitignore" | ".ignore" | "CACHEDIR.TAG" | "pyvenv.cfg")))
        || path == root.join(".git/info/exclude")
}

/// A running watcher; dropping it stops watching.
pub struct TreeWatcher {
    _watcher: RecommendedWatcher,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for TreeWatcher {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Watch `root` recursively; overflow and backend rescan requests explicitly reconcile the tree.
pub fn watch<F, Fut>(root: &Path, on_batch: F) -> notify::Result<TreeWatcher>
where
    F: Fn(Batch) -> Fut + Send + Sync + 'static,
    Fut: std::future::Future<Output = ()> + Send + 'static,
{
    let (tx, mut rx) = mpsc::channel::<PathBuf>(MAX_PENDING_PATHS);
    let overflow = Arc::new(AtomicBool::new(false));
    let callback_overflow = overflow.clone();
    let watch_root = root.to_path_buf();
    let mut watcher = notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
        let event = match event {
            Ok(event) if !event.need_rescan() => event,
            // Lost backend events are as significant as a full channel.
            _ => {
                callback_overflow.store(true, Ordering::Release);
                let _ = tx.try_send(watch_root.clone());
                return;
            }
        };
        if matches!(event.kind, notify::EventKind::Access(_)) {
            return;
        }
        for path in event.paths {
            let Ok(rel) = path.strip_prefix(&watch_root) else { continue };
            let rel = rel.to_string_lossy().replace('\\', "/");
            let history = rel == ".git/HEAD" || rel == ".git/ORIG_HEAD" || rel.starts_with(".git/refs/");
            let membership = changes_membership(&watch_root, &path);
            if rel.split('/').any(|part| part == "node_modules") || (rel.starts_with(".git/") && !history && rel != ".git/info/exclude") {
                continue;
            }
            // Drop ignored/build output before the bounded channel. Membership and history
            // controls must survive filtering so changes to those rules can remove old facts.
            if !membership && !history && (rel == ".git" || rel.starts_with(".git/") || !crate::tree::included(&watch_root, &rel)) {
                continue;
            }
            match tx.try_send(path) {
                Ok(()) => {}
                Err(mpsc::error::TrySendError::Full(_)) => callback_overflow.store(true, Ordering::Release),
                Err(mpsc::error::TrySendError::Closed(_)) => return,
            }
        }
    })?;
    watcher.watch(root, RecursiveMode::Recursive)?;
    let task = tokio::spawn(async move {
        while let Some(first) = rx.recv().await {
            let mut batch: BTreeSet<PathBuf> = BTreeSet::new();
            batch.insert(first);
            let started = tokio::time::Instant::now();
            loop {
                let remaining = DEBOUNCE_MAX.saturating_sub(started.elapsed());
                if remaining.is_zero() {
                    break;
                }
                match tokio::time::timeout(DEBOUNCE_QUIET.min(remaining), rx.recv()).await {
                    Ok(Some(path)) => {
                        batch.insert(path);
                        if batch.len() >= MAX_PENDING_PATHS {
                            overflow.store(true, Ordering::Release);
                            break;
                        }
                    }
                    Ok(None) => return,
                    Err(_) => break,
                }
            }
            on_batch(Batch { paths: batch.into_iter().collect(), reconcile: overflow.swap(false, Ordering::AcqRel) }).await;
        }
    });
    Ok(TreeWatcher { _watcher: watcher, task })
}
