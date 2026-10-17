//! `vcs.changed`: watch HEAD, refs and the index of every repository the plugin has served.
//!
//! File-system events on the git directory (HEAD, index, ORIG_HEAD, MERGE_HEAD, packed-refs) and
//! the refs tree are debounced; then a fingerprint (HEAD file, resolved HEAD commit, index size
//! and mtime) is compared with the last one and an event is published only when it changed.
//! Reads only: the watcher never runs a command that writes.

use crate::git::Git;
use crate::repo::Repo;
use cc_protocol::contracts::vcs::ChangedEvent;
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};
use tokio::sync::mpsc;

const DEBOUNCE: Duration = Duration::from_millis(60);

pub type Publish = Arc<dyn Fn(ChangedEvent) + Send + Sync>;

struct Entry {
    _watcher: RecommendedWatcher,
    roots: Arc<Mutex<BTreeSet<String>>>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Entry {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Watchers keyed by working-tree top level.
pub struct Watches {
    git: Git,
    publish: Publish,
    /// The plugin's private directory (probe repositories live there); never a user project.
    private: PathBuf,
    entries: Mutex<HashMap<PathBuf, Entry>>,
}

#[derive(Debug, Clone, PartialEq)]
struct Fingerprint {
    head_file: Vec<u8>,
    head: Option<String>,
    index: Option<(u64, SystemTime)>,
}

async fn fingerprint(git: &Git, repo: &Repo) -> Fingerprint {
    let head_file = std::fs::read(repo.git_dir.join("HEAD")).unwrap_or_default();
    let head = repo.head(git).await.ok().flatten();
    let index = std::fs::metadata(repo.git_dir.join("index")).ok().and_then(|m| Some((m.len(), m.modified().ok()?)));
    Fingerprint { head_file, head, index }
}

/// Paths whose change can move HEAD, a branch or the index.
fn relevant(repo: &Repo, path: &Path) -> bool {
    if path.starts_with(repo.common_dir.join("refs")) || path.starts_with(repo.git_dir.join("refs")) {
        return path.extension().is_none_or(|e| e != "lock");
    }
    let parent_ok = path.parent().is_some_and(|p| p == repo.git_dir || p == repo.common_dir);
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
    parent_ok && matches!(name, "HEAD" | "index" | "ORIG_HEAD" | "MERGE_HEAD" | "packed-refs")
}

impl Watches {
    pub fn new(git: Git, publish: Publish, private: PathBuf) -> Self {
        Self { git, publish, private, entries: Mutex::new(HashMap::new()) }
    }

    /// Roots currently watched (for handing over to a successor instance).
    pub fn roots(&self) -> Vec<String> {
        let entries = match self.entries.lock() {
            Ok(entries) => entries,
            Err(poisoned) => poisoned.into_inner(),
        };
        let mut out: Vec<String> =
            entries.values().flat_map(|e| e.roots.lock().map(|r| r.iter().cloned().collect::<Vec<_>>()).unwrap_or_default()).collect();
        out.sort();
        out.dedup();
        out
    }

    /// Start watching `repo` (idempotent); events are published under `root` as the caller named it.
    pub fn ensure(&self, repo: &Repo, root: &str) {
        if repo.top.starts_with(&self.private) {
            return;
        }
        let mut entries = match self.entries.lock() {
            Ok(entries) => entries,
            Err(poisoned) => poisoned.into_inner(),
        };
        if let Some(entry) = entries.get(&repo.top) {
            if let Ok(mut roots) = entry.roots.lock() {
                roots.insert(root.to_owned());
            }
            return;
        }
        let (tx, rx) = mpsc::unbounded_channel::<()>();
        let filter_repo = repo.clone();
        let handler = move |result: notify::Result<notify::Event>| {
            if let Ok(event) = result
                && event.paths.iter().any(|p| relevant(&filter_repo, p))
            {
                let _ = tx.send(());
            }
        };
        let mut watcher = match notify::recommended_watcher(handler) {
            Ok(watcher) => watcher,
            Err(error) => {
                tracing::warn!(%error, "cannot create a file watcher");
                return;
            }
        };
        let mut targets = vec![(repo.git_dir.clone(), RecursiveMode::NonRecursive)];
        if repo.common_dir != repo.git_dir {
            targets.push((repo.common_dir.clone(), RecursiveMode::NonRecursive));
        }
        targets.push((repo.common_dir.join("refs"), RecursiveMode::Recursive));
        for (path, mode) in targets {
            if let Err(error) = watcher.watch(&path, mode) {
                tracing::warn!(%error, path = %path.display(), "cannot watch");
            }
        }
        let roots = Arc::new(Mutex::new(BTreeSet::from([root.to_owned()])));
        let task = tokio::spawn(run(self.git.clone(), repo.clone(), roots.clone(), self.publish.clone(), rx));
        entries.insert(repo.top.clone(), Entry { _watcher: watcher, roots, task });
    }

    pub fn clear(&self) {
        if let Ok(mut entries) = self.entries.lock() {
            entries.clear();
        }
    }
}

async fn run(git: Git, repo: Repo, roots: Arc<Mutex<BTreeSet<String>>>, publish: Publish, mut rx: mpsc::UnboundedReceiver<()>) {
    let mut last = fingerprint(&git, &repo).await;
    while rx.recv().await.is_some() {
        // Debounce: a commit touches index, objects and refs in quick succession.
        loop {
            match tokio::time::timeout(DEBOUNCE, rx.recv()).await {
                Ok(Some(())) => continue,
                Ok(None) => return,
                Err(_) => break,
            }
        }
        let now = fingerprint(&git, &repo).await;
        if now == last {
            continue;
        }
        last = now;
        let branch = repo.branch(&git).await.ok().flatten().unwrap_or_else(|| "HEAD".into());
        let names: Vec<String> = roots.lock().map(|r| r.iter().cloned().collect()).unwrap_or_default();
        for root in names {
            publish(ChangedEvent { root, head: last.head.clone(), branch: branch.clone() });
        }
    }
}
