//! File-system watching (FSEvents on macOS through `notify`) with a short debounce.

use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::sync::mpsc;

/// Quiet period that ends a burst of events, and the longest a burst is held.
pub const DEBOUNCE_QUIET: Duration = Duration::from_millis(60);
pub const DEBOUNCE_MAX: Duration = Duration::from_millis(500);

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

/// Watch `root` recursively; `on_batch` receives each debounced batch of changed absolute paths.
pub fn watch<F, Fut>(root: &Path, on_batch: F) -> notify::Result<TreeWatcher>
where
    F: Fn(Vec<PathBuf>) -> Fut + Send + Sync + 'static,
    Fut: std::future::Future<Output = ()> + Send + 'static,
{
    let (tx, mut rx) = mpsc::unbounded_channel::<PathBuf>();
    let mut watcher = notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
        if let Ok(event) = event {
            if matches!(event.kind, notify::EventKind::Access(_)) {
                return;
            }
            for path in event.paths {
                let _ = tx.send(path);
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
                    }
                    Ok(None) => return,
                    Err(_) => break,
                }
            }
            on_batch(batch.into_iter().collect()).await;
        }
    });
    Ok(TreeWatcher { _watcher: watcher, task })
}
