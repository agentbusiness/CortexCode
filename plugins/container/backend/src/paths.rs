//! Host paths in the project root <-> container paths, through the container's actual bind mounts.
//!
//! The mounts are what Docker reports for the running container ([`PathMapping`]s from `docker
//! inspect`): the root bind-mounted at `workdir` for a plain container, whatever the Compose file
//! or `workspaceMount` binds for the others. Rules for a path given to `exec` (`cwd`):
//! - an absolute path equal to or under the project root (in the spelling the caller used or its
//!   canonical form, e.g. `/tmp/x` and `/private/tmp/x`) is a host path: it maps through the bind
//!   mount with the longest source that contains it (the root itself, a directory inside it, or one
//!   above it); a root path that no mount brings into the container is an error, never passed on;
//! - any other absolute path is a container path and is used as is;
//! - a relative path is relative to `workdir`.
//!
//! Paths are normalized lexically (`.`, `..`, repeated `/`); comparison is by whole components, so
//! `/a/bc` is not under `/a/b`.

use cc_protocol::contracts::container::PathMapping;
use std::path::PathBuf;

/// Lexically normalized absolute POSIX path (`..` above `/` stays at `/`). `path` must be absolute.
pub fn normalize(path: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            other => parts.push(other),
        }
    }
    format!("/{}", parts.join("/"))
}

/// `path` relative to `base` when it is `base` itself or under it (both normalized absolute).
fn strip(path: &str, base: &str) -> Option<String> {
    if base == "/" {
        return Some(path.trim_start_matches('/').to_owned());
    }
    if path == base {
        return Some(String::new());
    }
    path.strip_prefix(base)
        .and_then(|rest| rest.strip_prefix('/'))
        .map(str::to_owned)
}

fn join(base: &str, relative: &str) -> String {
    if relative.is_empty() {
        base.to_owned()
    } else {
        normalize(&format!("{base}/{relative}"))
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct PathMap {
    /// The root as given and its canonical form (deduplicated), normalized.
    roots: Vec<String>,
    /// Canonical root.
    root: String,
    /// (host source, container destination), normalized; a source whose canonical form differs is
    /// listed in both spellings.
    mounts: Vec<(String, String)>,
    workdir: String,
}

impl PathMap {
    pub fn new(
        given_root: &str,
        canonical_root: &str,
        workdir: &str,
        mounts: &[PathMapping],
    ) -> Self {
        let root = normalize(canonical_root);
        let mut roots = vec![root.clone()];
        if given_root.starts_with('/') {
            let given = normalize(given_root);
            if given != root {
                roots.push(given);
            }
        }
        let mut pairs = Vec::new();
        for mount in mounts
            .iter()
            .filter(|m| m.host.starts_with('/') && m.container.starts_with('/'))
        {
            let host = normalize(&mount.host);
            let container = normalize(&mount.container);
            if let Ok(canonical) = std::fs::canonicalize(&host) {
                let canonical = normalize(&canonical.to_string_lossy());
                if canonical != host {
                    pairs.push((canonical, container.clone()));
                }
            }
            pairs.push((host, container));
        }
        Self {
            roots,
            root,
            mounts: pairs,
            workdir: normalize(workdir),
        }
    }

    pub fn workdir(&self) -> &str {
        &self.workdir
    }

    /// The canonical spelling of `path` when it is under the root (in either spelling).
    fn in_root(&self, path: &str) -> Option<String> {
        self.roots
            .iter()
            .find_map(|root| strip(path, root))
            .map(|relative| join(&self.root, &relative))
    }

    /// The container path for a host path in the root, a container path, or a path relative to the
    /// workdir. `Err` names a root path that no bind mount brings into the container.
    pub fn to_container(&self, path: &str) -> Result<String, String> {
        if !path.starts_with('/') {
            return Ok(join(&self.workdir, path));
        }
        let normalized = normalize(path);
        let Some(canonical) = self.in_root(&normalized) else {
            return Ok(normalized);
        };
        let best = [normalized.as_str(), canonical.as_str()]
            .iter()
            .flat_map(|candidate| {
                self.mounts.iter().filter_map(move |(host, container)| {
                    strip(candidate, host).map(|rest| (host.len(), container, rest))
                })
            })
            .max_by_key(|(len, _, _)| *len);
        match best {
            Some((_, container, rest)) => Ok(join(container, &rest)),
            None => Err(format!(
                "{path} is in the project root, but no bind mount of the container contains it (mounts: {})",
                if self.mounts.is_empty() {
                    "none".to_owned()
                } else {
                    self.mounts
                        .iter()
                        .map(|(h, c)| format!("{h} -> {c}"))
                        .collect::<Vec<_>>()
                        .join(", ")
                }
            )),
        }
    }

    /// The host path for a container path inside a bind mount; `None` for other paths.
    pub fn to_host(&self, container_path: &str) -> Option<PathBuf> {
        let normalized = if container_path.starts_with('/') {
            normalize(container_path)
        } else {
            join(&self.workdir, container_path)
        };
        self.mounts
            .iter()
            .filter_map(|(host, container)| {
                strip(&normalized, container).map(|rest| (container.len(), host, rest))
            })
            .max_by_key(|(len, _, _)| *len)
            .map(|(_, host, rest)| PathBuf::from(join(host, &rest)))
    }
}
