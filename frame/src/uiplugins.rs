//! UI plugins. A built UI package is a directory with a `manifest.json`
//! `{id, version, slot, entry, css?, label?, order?}`. The frame serves the UI packages of every
//! enabled installed plugin, from its package version's bundle directories. The bundle hash is a
//! SHA-256 over every file's relative path and bytes (it changes with every version, so a client
//! never mixes two); a package is served at `/ui-plugins/<id>/<hash>/<file>`.
//!
//! Every package belongs to the plugin that ships it (its owner). The packages of a disabled plugin
//! are not served.

use cc_protocol::contracts::frame::{UiPlugin, WINDOW_SLOT};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path, PathBuf};
use std::sync::RwLock;

#[derive(Debug, Deserialize)]
struct Manifest {
    id: String,
    version: String,
    slot: String,
    entry: String,
    #[serde(default)]
    css: Option<String>,
    #[serde(default)]
    label: Option<String>,
    #[serde(default)]
    order: Option<f64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct UiEntry {
    pub plugin: UiPlugin,
    pub dir: PathBuf,
}

/// The plugin a package id names: its first dot-separated part (structural rule: UI package ids
/// are `<plugin>.<package>`).
pub fn owner_of(id: &str) -> String {
    id.split('.').next().unwrap_or(id).to_owned()
}

/// A change between two scans.
#[derive(Debug, Clone, PartialEq)]
pub enum UiChange {
    Changed(UiPlugin),
    Removed(String),
}

pub struct UiPlugins {
    /// Bundle directories of plugin packages, with their owner, in priority order.
    packages: RwLock<Vec<(String, PathBuf)>>,
    /// Plugins whose packages are not served.
    disabled: RwLock<BTreeSet<String>>,
    /// Every loadable package found by the last scan, served or not.
    known: RwLock<BTreeMap<String, UiEntry>>,
    /// What is served now.
    current: RwLock<BTreeMap<String, UiEntry>>,
}

impl UiPlugins {
    pub fn new(packages: Vec<(String, PathBuf)>, disabled: BTreeSet<String>) -> Self {
        let plugins = Self {
            packages: RwLock::new(packages),
            disabled: RwLock::new(disabled),
            known: RwLock::new(BTreeMap::new()),
            current: RwLock::new(BTreeMap::new()),
        };
        plugins.rescan();
        plugins
    }

    /// The window: the served package on [`WINDOW_SLOT`] (the newest, when more than one).
    pub fn window(&self) -> Option<UiEntry> {
        let current = self.current.read().ok()?;
        current
            .values()
            .filter(|entry| entry.plugin.slot == WINDOW_SLOT)
            .max_by_key(|entry| cc_protocol::version::Version::parse(&entry.plugin.version))
            .cloned()
    }

    pub fn list(&self) -> Vec<UiPlugin> {
        self.current.read().map(|current| current.values().map(|e| e.plugin.clone()).collect()).unwrap_or_default()
    }

    /// Replace the bundle directories of plugin packages (call [`Self::rescan`] after).
    pub fn set_packages(&self, packages: Vec<(String, PathBuf)>) {
        if let Ok(mut current) = self.packages.write() {
            *current = packages;
        }
    }

    /// Serve or stop serving a plugin's packages (call [`Self::rescan`] after).
    pub fn set_disabled(&self, owner: &str, disabled: bool) {
        if let Ok(mut set) = self.disabled.write() {
            if disabled {
                set.insert(owner.to_owned());
            } else {
                set.remove(owner);
            }
        }
    }

    /// Rescan every source and return what changed in what is served since the last scan.
    pub fn rescan(&self) -> Vec<UiChange> {
        let known = self.scan();
        let disabled = self.disabled.read().map(|set| set.clone()).unwrap_or_default();
        let scanned: BTreeMap<String, UiEntry> =
            known.iter().filter(|(_, entry)| !disabled.contains(&entry.plugin.plugin)).map(|(id, entry)| (id.clone(), entry.clone())).collect();
        if let Ok(mut all) = self.known.write() {
            *all = known;
        }
        let Ok(mut current) = self.current.write() else { return Vec::new() };
        let mut changes = Vec::new();
        for (id, entry) in &scanned {
            if current.get(id).map(|old| &old.plugin) != Some(&entry.plugin) {
                changes.push(UiChange::Changed(entry.plugin.clone()));
            }
        }
        for id in current.keys() {
            if !scanned.contains_key(id) {
                changes.push(UiChange::Removed(id.clone()));
            }
        }
        *current = scanned;
        changes
    }

    /// Resolve a request for `/ui-plugins/<id>/<hash>/<file>` to a file on disk. Only the
    /// current hash is served; `file` must stay inside the plugin directory.
    pub fn resolve(&self, id: &str, hash: &str, file: &str) -> Option<PathBuf> {
        let dir = {
            let current = self.current.read().ok()?;
            let entry = current.get(id)?;
            if entry.plugin.hash != hash {
                return None;
            }
            entry.dir.clone()
        };
        let relative = safe_relative(file)?;
        let path = dir.join(relative);
        let canonical = std::fs::canonicalize(&path).ok()?;
        let root = std::fs::canonicalize(&dir).ok()?;
        (canonical.starts_with(&root) && canonical.is_file()).then_some(canonical)
    }

    fn scan(&self) -> BTreeMap<String, UiEntry> {
        let mut found: BTreeMap<String, UiEntry> = BTreeMap::new();
        let mut add = |entry: UiEntry, dir: &Path| {
            if let Some(existing) = found.get(&entry.plugin.id) {
                if existing.dir != entry.dir {
                    tracing::warn!(id = %entry.plugin.id, dir = %dir.display(), served = %existing.dir.display(), "duplicate UI plugin id ignored");
                }
                return;
            }
            found.insert(entry.plugin.id.clone(), entry);
        };
        let packages = self.packages.read().map(|p| p.clone()).unwrap_or_default();
        for (owner, dir) in &packages {
            match load(dir, owner) {
                Ok(entry) => add(entry, dir),
                Err(error) => tracing::debug!(dir = %dir.display(), %error, "UI package not built"),
            }
        }
        found
    }
}

/// Load the bundle in `dir`. `owner` empty: the owner follows from the id ([`owner_of`]).
fn load(dir: &Path, owner: &str) -> Result<UiEntry, String> {
    let manifest_bytes = std::fs::read(dir.join("manifest.json")).map_err(|e| format!("manifest.json: {e}"))?;
    let manifest: Manifest = serde_json::from_slice(&manifest_bytes).map_err(|e| format!("manifest.json: {e}"))?;
    if manifest.id.is_empty() || !manifest.id.bytes().all(|b| b.is_ascii_alphanumeric() || b"-_.@".contains(&b)) {
        return Err(format!("invalid UI plugin id `{}`", manifest.id));
    }
    let entry = safe_relative(&manifest.entry).ok_or_else(|| format!("invalid entry `{}`", manifest.entry))?;
    if !dir.join(&entry).is_file() {
        return Err(format!("entry `{}` does not exist", manifest.entry));
    }
    let css = match &manifest.css {
        Some(css) => {
            let relative = safe_relative(css).ok_or_else(|| format!("invalid css `{css}`"))?;
            if !dir.join(&relative).is_file() {
                return Err(format!("css `{css}` does not exist"));
            }
            Some(relative)
        }
        None => None,
    };
    let hash = bundle_hash(dir).map_err(|e| format!("hashing: {e}"))?;
    let url = |relative: &Path| format!("/ui-plugins/{}/{}/{}", manifest.id, hash, relative.to_string_lossy());
    Ok(UiEntry {
        plugin: UiPlugin {
            entry_url: url(&entry),
            css_url: css.as_deref().map(url),
            plugin: if owner.is_empty() { owner_of(&manifest.id) } else { owner.to_owned() },
            id: manifest.id,
            version: manifest.version,
            slot: manifest.slot,
            hash,
            label: manifest.label.filter(|label| !label.trim().is_empty()),
            order: manifest.order.filter(|order| order.is_finite()),
        },
        dir: dir.to_path_buf(),
    })
}

/// Content hash of a bundle: SHA-256 over each file's relative path, length and bytes, in
/// sorted path order (structural rule; any byte change anywhere changes the hash).
pub fn bundle_hash(dir: &Path) -> std::io::Result<String> {
    let mut files = Vec::new();
    collect(dir, dir, &mut files)?;
    files.sort();
    let mut hasher = Sha256::new();
    for relative in files {
        let bytes = std::fs::read(dir.join(&relative))?;
        hasher.update(relative.as_bytes());
        hasher.update([0]);
        hasher.update((bytes.len() as u64).to_be_bytes());
        hasher.update(&bytes);
    }
    Ok(hex::encode(hasher.finalize())[..16].to_owned())
}

fn collect(root: &Path, dir: &Path, out: &mut Vec<String>) -> std::io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let kind = entry.file_type()?;
        if kind.is_dir() {
            collect(root, &path, out)?;
        } else if kind.is_file()
            && let Ok(relative) = path.strip_prefix(root)
        {
            out.push(relative.to_string_lossy().into_owned());
        }
    }
    Ok(())
}

/// A relative path with only normal components (no `..`, no root, no empty).
pub fn safe_relative(value: &str) -> Option<PathBuf> {
    let path = Path::new(value);
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Normal(part) => out.push(part),
            Component::CurDir => {}
            _ => return None,
        }
    }
    (!out.as_os_str().is_empty()).then_some(out)
}

pub fn content_type(path: &Path) -> &'static str {
    match path.extension().and_then(|e| e.to_str()).unwrap_or("") {
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "json" | "map" => "application/json",
        "html" => "text/html; charset=utf-8",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "woff2" => "font/woff2",
        "woff" => "font/woff",
        "ttf" => "font/ttf",
        "wasm" => "application/wasm",
        "txt" | "md" => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}
