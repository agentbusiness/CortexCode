//! Small shared helpers: ids, content hashes, path resolution and atomic file writes.

use sha2::{Digest, Sha256};
use std::io::Write;
use std::path::{Component, Path, PathBuf};

/// A short random id with a prefix, e.g. `a-3f9c0d1e2b4a`.
pub fn new_id(prefix: &str) -> String {
    let raw = uuid::Uuid::new_v4().simple().to_string();
    format!("{prefix}-{}", &raw[..12])
}

/// Hex SHA-256 of some bytes (used as the identity of a file version).
pub fn content_hash(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(64);
    for byte in digest {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

/// Milliseconds since the Unix epoch.
pub fn now_ms() -> u64 {
    cc_protocol::contracts::now_ms()
}

/// Expand a leading `~/` to the home directory.
pub fn expand_home(path: &str) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/")
        && let Some(home) = std::env::var_os("HOME")
    {
        return PathBuf::from(home).join(rest);
    }
    if path == "~"
        && let Some(home) = std::env::var_os("HOME")
    {
        return PathBuf::from(home);
    }
    PathBuf::from(path)
}

/// Lexically normalize a path (resolve `.` and `..` without touching the file system).
pub fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    out.push("..");
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Resolve a path given by the model or the UI against a base directory.
pub fn resolve(base: &Path, path: &str) -> PathBuf {
    let expanded = expand_home(path.trim());
    let joined = if expanded.is_absolute() { expanded } else { base.join(expanded) };
    normalize(&joined)
}

/// Canonical form for containment checks: canonicalize the longest existing ancestor and
/// re-append the rest (so not-yet-existing files still resolve through symlinked parents).
pub fn canonical_lenient(path: &Path) -> PathBuf {
    let mut existing = path.to_path_buf();
    let mut rest: Vec<std::ffi::OsString> = Vec::new();
    loop {
        if let Ok(canonical) = existing.canonicalize() {
            let mut out = canonical;
            for part in rest.iter().rev() {
                out.push(part);
            }
            return out;
        }
        match (existing.file_name().map(|name| name.to_os_string()), existing.parent()) {
            (Some(name), Some(parent)) => {
                rest.push(name);
                existing = parent.to_path_buf();
            }
            _ => return normalize(path),
        }
    }
}

/// Whether `path` lies inside `root` (both resolved through symlinks where they exist).
pub fn is_inside(path: &Path, root: &Path) -> bool {
    let path = canonical_lenient(path);
    let root = canonical_lenient(root);
    path.starts_with(&root)
}

/// Display a path relative to `base` when it is inside it.
pub fn display_rel(path: &Path, base: &Path) -> String {
    match path.strip_prefix(base) {
        Ok(rel) if !rel.as_os_str().is_empty() => rel.to_string_lossy().into_owned(),
        Ok(_) => ".".to_owned(),
        Err(_) => {
            let canonical_base = canonical_lenient(base);
            let canonical_path = canonical_lenient(path);
            match canonical_path.strip_prefix(&canonical_base) {
                Ok(rel) if !rel.as_os_str().is_empty() => rel.to_string_lossy().into_owned(),
                _ => path.to_string_lossy().into_owned(),
            }
        }
    }
}

/// Write a file atomically (temp file in the same directory, then rename), keeping the
/// permissions of an existing file. Symlinks are followed so the link target is updated.
pub fn atomic_write(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let target = match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() => std::fs::canonicalize(path)?,
        _ => path.to_path_buf(),
    };
    let dir = target.parent().map(Path::to_path_buf).unwrap_or_else(|| PathBuf::from("."));
    std::fs::create_dir_all(&dir)?;
    let permissions = std::fs::metadata(&target).ok().map(|meta| meta.permissions());
    let name = target.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default();
    let temp = dir.join(format!(".{name}.cortex-{}", &uuid::Uuid::new_v4().simple().to_string()[..8]));
    let result = (|| {
        let mut file = std::fs::File::create(&temp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        if let Some(permissions) = permissions {
            std::fs::set_permissions(&temp, permissions)?;
        }
        std::fs::rename(&temp, &target)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    result
}

/// Quote a string for POSIX shells (single quotes).
pub fn shell_quote(text: &str) -> String {
    if !text.is_empty() && text.bytes().all(|b| b.is_ascii_alphanumeric() || b"-_./=:,+@%".contains(&b)) {
        return text.to_owned();
    }
    format!("'{}'", text.replace('\'', "'\\''"))
}
