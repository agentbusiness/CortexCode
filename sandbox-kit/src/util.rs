//! The path and hash helpers the sandbox needs (the same as the tools plugin's own).

use sha2::{Digest, Sha256};
use std::path::{Component, Path, PathBuf};

/// Hex SHA-256 of some bytes (the identity of a confinement).
pub fn content_hash(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(64);
    for byte in digest {
        out.push_str(&format!("{byte:02x}"));
    }
    out
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

/// `path` against `base` (a leading `~/` expanded), normalized.
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
