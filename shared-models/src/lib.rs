//! The models folder every Cortex app shares (CortexCode, CortexTerm and the others):
//! `~/.cortex-models`, laid out `<org>-<repo>/<revision>/<file>` (`docs/SHARED-MODELS.md`).
//!
//! - A download into a revision folder holds that folder's [`LOCK`] (an exclusive `flock`), looks
//!   again with the lock held (another app may have just fetched the file), writes `<file>.part`,
//!   checks it and renames it into place. Two apps that want the same model download it once.
//! - At every start, before the first model lookup, an app moves its own old model folders in
//!   ([`move_in`]). Nothing is ever copied: a rename that fails leaves the old copy where it is.

use std::fs::{self, File, OpenOptions, TryLockError};
use std::io;
use std::path::{Path, PathBuf};

/// The lock file in each revision folder, held while a file of that revision is downloaded or moved in.
pub const LOCK: &str = ".download.lock";
/// The lock file the IronGraph SDK and earlier memory plugins kept in a revision folder.
const OLD_LOCK: &str = ".install.lock";

/// `~/.cortex-models`.
pub fn root() -> PathBuf {
    PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".cortex-models")
}

/// `<root>/<org>-<repo>/<revision>`: where a pinned revision's files live.
pub fn revision_dir(root: &Path, repo: &str, revision: &str) -> PathBuf {
    root.join(repo.replace('/', "-")).join(revision)
}

/// Create `root` (0755) when it is missing.
pub fn ensure_root(root: &Path) -> io::Result<()> {
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o755);
    builder.create(root)
}

fn open_lock(dir: &Path, name: &str) -> io::Result<File> {
    OpenOptions::new().create(true).truncate(false).read(true).write(true).open(dir.join(name))
}

/// Take the revision folder's [`LOCK`] (creating the folder), waiting while another app holds it.
/// It is released when the file is dropped. Blocking: async callers run it on a blocking thread.
pub fn lock(dir: &Path) -> io::Result<File> {
    fs::create_dir_all(dir)?;
    let file = open_lock(dir, LOCK)?;
    file.lock()?;
    Ok(file)
}

/// The revision folder's [`LOCK`] when it is free now (creating the folder); `None` while another
/// app holds it.
pub fn try_lock(dir: &Path) -> io::Result<Option<File>> {
    fs::create_dir_all(dir)?;
    try_lock_file(dir, LOCK)
}

fn try_lock_file(dir: &Path, name: &str) -> io::Result<Option<File>> {
    let file = open_lock(dir, name)?;
    match file.try_lock() {
        Ok(()) => Ok(Some(file)),
        Err(TryLockError::WouldBlock) => Ok(None),
        Err(TryLockError::Error(error)) => Err(error),
    }
}

/// Move an app's own old model folder (`<old>/<org>-<repo>/<revision>/…`) into `root`:
///
/// 1. a revision `root` does not have is renamed there whole;
/// 2. otherwise, with the target's lock held, each file is renamed in when the target lacks it,
///    deleted when the target has it at the same size (same repository, commit and size: the same
///    file), and left when the sizes differ (the app's own pinned check settles it);
/// 3. a revision folder still being downloaded (a held lock) is skipped; stale `*.part` files go;
/// 4. the old folders left empty are removed, `<old>` included;
/// 5. a rename that fails (another volume, permissions) leaves the old copy and is logged.
///
/// Anything else in `<old>` stays. Runs on every start, so leftovers are picked up next time.
pub fn move_in(old: &Path, root: &Path) {
    let Ok(repos) = fs::read_dir(old) else { return };
    if same_dir(old, root) {
        return;
    }
    if let Err(error) = ensure_root(root) {
        tracing::warn!(dir = %root.display(), %error, "cannot create the shared models folder; old models stay where they are");
        return;
    }
    for repo in repos.flatten() {
        if !repo.file_type().is_ok_and(|kind| kind.is_dir()) {
            continue;
        }
        let Ok(revisions) = fs::read_dir(repo.path()) else { continue };
        for revision in revisions.flatten() {
            if revision.file_type().is_ok_and(|kind| kind.is_dir()) {
                move_revision(&revision.path(), &root.join(repo.file_name()).join(revision.file_name()));
            }
        }
        let _ = fs::remove_dir(repo.path());
    }
    let _ = fs::remove_dir(old);
}

fn move_revision(from: &Path, to: &Path) {
    // A lock someone holds in the old folder: it is still being downloaded there.
    let mut held = Vec::new();
    for name in [LOCK, OLD_LOCK] {
        if !from.join(name).exists() {
            continue;
        }
        match try_lock_file(from, name) {
            Ok(Some(file)) => held.push(file),
            Ok(None) => {
                tracing::info!(dir = %from.display(), "old model folder is being downloaded into; it is moved at a later start");
                return;
            }
            Err(error) => {
                tracing::warn!(dir = %from.display(), %error, "cannot lock the old model folder; it stays where it is");
                return;
            }
        }
    }
    remove_parts(from);
    if !to.exists() {
        let bytes = size_of(from);
        let renamed = to.parent().map_or(Ok(()), fs::create_dir_all).and_then(|()| fs::rename(from, to));
        match renamed {
            Ok(()) => {
                tracing::info!(revision = %to.display(), from = %from.display(), bytes, "model moved into the shared models folder");
                return;
            }
            // Another app made the folder meanwhile: merge into it below.
            Err(_) if to.exists() => {}
            Err(error) => {
                tracing::warn!(from = %from.display(), to = %to.display(), %error, "model not moved into the shared models folder; it stays where it is");
                return;
            }
        }
    }
    let _target_lock = match try_lock(to) {
        Ok(Some(file)) => file,
        Ok(None) => {
            tracing::info!(dir = %to.display(), "shared model folder is being downloaded into; the old copy is moved at a later start");
            return;
        }
        Err(error) => {
            tracing::warn!(dir = %to.display(), %error, "cannot lock the shared model folder; the old copy stays where it is");
            return;
        }
    };
    let (mut moved, mut deleted) = (0u64, 0u64);
    merge(from, to, &mut moved, &mut deleted);
    // The old folder's lock files go with it.
    drop(held);
    let _ = fs::remove_file(from.join(LOCK));
    let _ = fs::remove_file(from.join(OLD_LOCK));
    remove_empty(from);
    if moved > 0 {
        tracing::info!(revision = %to.display(), from = %from.display(), bytes = moved, "model files moved into the shared models folder");
    }
    if deleted > 0 {
        tracing::info!(revision = %to.display(), from = %from.display(), bytes = deleted, "duplicate model files deleted; the shared models folder has them");
    }
}

/// Rename each file of `from` into `to` when `to` lacks it; delete it when `to` has it at the same size.
fn merge(from: &Path, to: &Path, moved: &mut u64, deleted: &mut u64) {
    let Ok(entries) = fs::read_dir(from) else { return };
    for entry in entries.flatten() {
        let name = entry.file_name();
        if name == LOCK || name == OLD_LOCK {
            continue;
        }
        let (source, target) = (entry.path(), to.join(&name));
        let Ok(kind) = entry.file_type() else { continue };
        if kind.is_dir() && target.exists() {
            merge(&source, &target, moved, deleted);
            let _ = fs::remove_dir(&source);
            continue;
        }
        let bytes = if kind.is_dir() { size_of(&source) } else { entry.metadata().map(|meta| meta.len()).unwrap_or(0) };
        match fs::symlink_metadata(&target) {
            Err(_) => match fs::rename(&source, &target) {
                Ok(()) => *moved += bytes,
                Err(error) => tracing::warn!(from = %source.display(), to = %target.display(), %error, "model file not moved into the shared models folder; it stays where it is"),
            },
            Ok(meta) if meta.is_file() && meta.len() == bytes => match fs::remove_file(&source) {
                Ok(()) => *deleted += bytes,
                Err(error) => tracing::warn!(file = %source.display(), %error, "cannot delete a duplicate model file"),
            },
            // Different sizes: both stay, the app's own pinned check settles it.
            Ok(_) => {}
        }
    }
}

/// Delete the `*.part` files left by downloads that are no longer running.
fn remove_parts(dir: &Path) {
    let Ok(entries) = fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            remove_parts(&path);
        } else if path.extension().is_some_and(|extension| extension == "part") {
            let _ = fs::remove_file(&path);
        }
    }
}

/// Remove `dir` and the folders under it that hold nothing.
fn remove_empty(dir: &Path) {
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                remove_empty(&entry.path());
            }
        }
    }
    let _ = fs::remove_dir(dir);
}

fn size_of(path: &Path) -> u64 {
    let Ok(meta) = fs::symlink_metadata(path) else { return 0 };
    if !meta.is_dir() {
        return meta.len();
    }
    fs::read_dir(path).map(|entries| entries.flatten().map(|entry| size_of(&entry.path())).sum()).unwrap_or(0)
}

fn same_dir(a: &Path, b: &Path) -> bool {
    match (fs::canonicalize(a), fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}
