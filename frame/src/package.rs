//! Installed `.ccplugin` packages: `<home>/plugins/<id>/<version>/`, one folder per version.
//!
//! Installing unpacks the archive into a staging folder, reads its `manifest.json`, checks what the
//! frame needs to run it (format, id and version, safe paths, regular files only, the backend built
//! for this machine and speaking a contract this frame knows, UI bundles present) and renames the
//! folder into place. Beside the backend's executable the frame links [`PROCESS_NAME`], the name
//! the plugin process runs under, so every plugin shows as CortexCode (Activity Monitor, `ps`).
//!
//! `.incoming/` holds packages dropped in for installation (`cortex-pack build --install`); the
//! frame installs them at startup and whenever one appears.

use cc_protocol::contracts::frame::package::{CcPluginManifest, FORMAT, PACKAGE_MANIFEST, safe_relative, valid_id};
use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

/// Largest manifest.json accepted (structural limit against a hostile archive).
const MAX_MANIFEST_BYTES: u64 = 16 << 20;

/// The file name every plugin process runs under (a hard link beside the packed executable).
pub const PROCESS_NAME: &str = "CortexCode";

/// The folder packages are dropped into for installation.
pub const INCOMING: &str = ".incoming";

/// An unpacked package version.
#[derive(Debug, Clone, PartialEq)]
pub struct Installed {
    pub dir: PathBuf,
    pub manifest: CcPluginManifest,
}

impl Installed {
    pub fn id(&self) -> &str {
        &self.manifest.plugin.id
    }

    pub fn version(&self) -> &str {
        &self.manifest.plugin.version
    }

    /// What the backend process runs: the [`PROCESS_NAME`] link beside the packed executable.
    pub fn process_exec(&self) -> Option<PathBuf> {
        let backend = self.manifest.backend.as_ref()?;
        let exec = self.dir.join(&backend.exec);
        Some(exec.parent()?.join(PROCESS_NAME))
    }

    /// The packed executable's file name (`cc-chatflow`): the plugin's data folder name.
    pub fn exec_name(&self) -> Option<String> {
        let backend = self.manifest.backend.as_ref()?;
        Path::new(&backend.exec).file_name().map(|name| name.to_string_lossy().into_owned())
    }
}

/// The Rust target triple of this frame (packages' backends must match it).
pub fn host_target() -> String {
    let os = match std::env::consts::OS {
        "macos" => "apple-darwin",
        "linux" => "unknown-linux-gnu",
        other => other,
    };
    format!("{}-{os}", std::env::consts::ARCH)
}

/// A version string usable as a folder name.
pub fn valid_version(version: &str) -> bool {
    !version.is_empty()
        && version.len() <= 128
        && !version.starts_with('.')
        && version.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'+' | b'_'))
}

/// Load an installed version.
pub fn load(root: &Path, id: &str, version: &str) -> Result<Installed, String> {
    if !valid_id(id) || !valid_version(version) {
        return Err(format!("invalid package reference {id}@{version}"));
    }
    let dir = root.join(id).join(version);
    let path = dir.join(PACKAGE_MANIFEST);
    let bytes = std::fs::read(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    let manifest: CcPluginManifest = serde_json::from_slice(&bytes).map_err(|e| format!("{}: {e}", path.display()))?;
    if manifest.plugin.id != id || manifest.plugin.version != version {
        return Err(format!("{} holds {}@{}", dir.display(), manifest.plugin.id, manifest.plugin.version));
    }
    Ok(Installed { dir, manifest })
}

/// Every installed version of `id`.
pub fn versions(root: &Path, id: &str) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(root.join(id)) else { return Vec::new() };
    entries
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.path().join(PACKAGE_MANIFEST).is_file())
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|version| valid_version(version))
        .collect()
}

/// Unpack `archive` into the store. A version already stored is replaced unless it is one of
/// `in_use` (then the install is refused: a running version never changes under its process).
pub fn install_archive(root: &Path, archive: &Path, in_use: &dyn Fn(&str, &str) -> bool) -> Result<Installed, String> {
    std::fs::create_dir_all(root).map_err(|e| format!("cannot create {}: {e}", root.display()))?;
    let staging = root.join(format!(".staging-{}", uuid::Uuid::new_v4().simple()));
    let outcome = (|| {
        std::fs::create_dir_all(&staging).map_err(|e| format!("cannot create {}: {e}", staging.display()))?;
        let manifest = unpack(archive, &staging)?;
        check(&manifest, &staging)?;
        let (id, version) = (manifest.plugin.id.clone(), manifest.plugin.version.clone());
        if let Some(exec) = &manifest.backend {
            let packed = staging.join(&exec.exec);
            let link = packed.parent().ok_or("the backend executable has no folder")?.join(PROCESS_NAME);
            std::fs::hard_link(&packed, &link).map_err(|e| format!("cannot link {}: {e}", link.display()))?;
        }
        let dir = root.join(&id).join(&version);
        if dir.exists() {
            if in_use(&id, &version) {
                return Err(format!("{id} {version} is installed and in use; give the new build another version"));
            }
            std::fs::remove_dir_all(&dir).map_err(|e| format!("cannot replace {}: {e}", dir.display()))?;
        }
        std::fs::create_dir_all(root.join(&id)).map_err(|e| format!("cannot create {}: {e}", root.join(&id).display()))?;
        std::fs::rename(&staging, &dir).map_err(|e| format!("cannot store the package: {e}"))?;
        Ok(Installed { dir, manifest })
    })();
    if outcome.is_err() {
        let _ = std::fs::remove_dir_all(&staging);
    }
    outcome
}

/// Remove stored versions of `id` other than `keep`.
pub fn prune(root: &Path, id: &str, keep: &[&str]) {
    for version in versions(root, id) {
        if keep.contains(&version.as_str()) {
            continue;
        }
        let dir = root.join(id).join(&version);
        match std::fs::remove_dir_all(&dir) {
            Ok(()) => tracing::info!(plugin = id, %version, "removed an old package version"),
            Err(error) => tracing::warn!(dir = %dir.display(), %error, "cannot remove an old package version"),
        }
    }
}

/// Unpack every entry into `staging`; returns the manifest.
fn unpack(archive: &Path, staging: &Path) -> Result<CcPluginManifest, String> {
    let file = std::fs::File::open(archive).map_err(|e| format!("cannot open {}: {e}", archive.display()))?;
    let mut tar = tar::Archive::new(flate2::read::GzDecoder::new(std::io::BufReader::new(file)));
    let mut manifest_bytes: Option<Vec<u8>> = None;
    let entries = tar.entries().map_err(|e| format!("not a .ccplugin archive: {e}"))?;
    for entry in entries {
        let mut entry = entry.map_err(|e| format!("damaged archive: {e}"))?;
        let raw_path = entry.path().map_err(|e| format!("damaged archive path: {e}"))?.to_string_lossy().into_owned();
        let path = raw_path.trim_start_matches("./").trim_end_matches('/').to_owned();
        let kind = entry.header().entry_type();
        if path.is_empty() && kind.is_dir() {
            continue;
        }
        if !safe_relative(&path) {
            return Err(format!("entry `{raw_path}` points outside the package"));
        }
        if kind.is_dir() {
            std::fs::create_dir_all(staging.join(&path)).map_err(|e| format!("cannot unpack `{path}`: {e}"))?;
            continue;
        }
        if !kind.is_file() {
            return Err(format!("entry `{path}` is a {kind:?}; a package holds only files and directories"));
        }
        if path == PACKAGE_MANIFEST {
            if entry.size() > MAX_MANIFEST_BYTES {
                return Err(format!("{PACKAGE_MANIFEST} is larger than {MAX_MANIFEST_BYTES} bytes"));
            }
            let mut bytes = Vec::new();
            entry.read_to_end(&mut bytes).map_err(|e| format!("cannot read {PACKAGE_MANIFEST}: {e}"))?;
            std::fs::write(staging.join(PACKAGE_MANIFEST), &bytes).map_err(|e| format!("cannot write {PACKAGE_MANIFEST}: {e}"))?;
            manifest_bytes = Some(bytes);
            continue;
        }
        let target = staging.join(&path);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("cannot unpack `{path}`: {e}"))?;
        }
        let executable = entry.header().mode().map(|mode| mode & 0o100 != 0).unwrap_or(false);
        let mut out = std::fs::File::create(&target).map_err(|e| format!("cannot unpack `{path}`: {e}"))?;
        std::io::copy(&mut entry, &mut out).map_err(|e| format!("cannot unpack `{path}`: {e}"))?;
        let mode = if executable { 0o755 } else { 0o644 };
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(mode)).map_err(|e| format!("cannot set the mode of `{path}`: {e}"))?;
    }
    let bytes = manifest_bytes.ok_or_else(|| format!("the archive has no {PACKAGE_MANIFEST}"))?;
    serde_json::from_slice(&bytes).map_err(|e| format!("invalid {PACKAGE_MANIFEST}: {e}"))
}

/// What the frame needs to run the unpacked package.
fn check(manifest: &CcPluginManifest, staging: &Path) -> Result<(), String> {
    if manifest.format != FORMAT {
        return Err(format!("unsupported package format `{}` (this frame reads {FORMAT})", manifest.format));
    }
    let meta = &manifest.plugin;
    if !valid_id(&meta.id) || !valid_version(&meta.version) {
        return Err(format!("invalid plugin id `{}` or version `{}`", meta.id, meta.version));
    }
    if let Some(backend) = &manifest.backend {
        let host = host_target();
        if manifest.build.target != host {
            return Err(format!("the backend was built for `{}`; this frame runs on `{host}`", manifest.build.target));
        }
        let exec = staging.join(&backend.exec);
        let executable = safe_relative(&backend.exec)
            && std::fs::metadata(&exec).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o100 != 0);
        if !executable {
            return Err(format!("the backend executable `{}` is not in the package", backend.exec));
        }
        if Path::new(&backend.exec).file_name().is_some_and(|name| name == PROCESS_NAME) {
            return Err(format!("the backend executable must not be named {PROCESS_NAME}"));
        }
        crate::supervisor::check_contract(&backend.category, &backend.contract)?;
    }
    for ui in &manifest.ui {
        if !ui.id.starts_with(&format!("{}.", meta.id)) {
            return Err(format!("UI package `{}` is outside the plugin's namespace `{}.`", ui.id, meta.id));
        }
        let bundle_manifest = format!("{}/manifest.json", ui.dir.trim_end_matches('/'));
        let bytes = std::fs::read(staging.join(&bundle_manifest)).map_err(|_| format!("UI package `{}` has no {bundle_manifest}", ui.id))?;
        let bundle: serde_json::Value = serde_json::from_slice(&bytes).map_err(|e| format!("{bundle_manifest}: {e}"))?;
        if bundle["id"].as_str() != Some(ui.id.as_str()) || bundle["slot"].as_str() != Some(ui.slot.as_str()) {
            return Err(format!("{bundle_manifest} says {} on {}, the package says {} on {}", bundle["id"], bundle["slot"], ui.id, ui.slot));
        }
        let entry = bundle["entry"].as_str().unwrap_or_default();
        if !staging.join(ui.dir.trim_end_matches('/')).join(entry).is_file() {
            return Err(format!("UI package `{}`'s entry `{entry}` is not in the package", ui.id));
        }
    }
    for shell in &manifest.shell {
        if !staging.join(shell.dir.trim_end_matches('/')).join("package.json").is_file() {
            return Err(format!("shell module `{}` has no package.json", shell.id));
        }
    }
    Ok(())
}
