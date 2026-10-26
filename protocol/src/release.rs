//! Releases (`docs/RELEASES.md`): the catalog of every published plugin, frame and app version,
//! the platform names and the version rule.
//!
//! - Every plugin, the frame and the app are released on their own, each as a GitHub release of the
//!   official repository. The catalog (`catalog.json` in the release tagged `catalog`) names the
//!   current version of each with its files, and is the only thing the updater reads to decide.
//! - A version is three numbers, each 0 to 99 (`0.3.7`); the next after `0.3.99` is `0.4.0`, after
//!   `0.99.99` it is `1.0.0`. Build metadata after `+` (`0.3.7+b1a2b3c4d5e6`) is not compared.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
pub use crate::version::{PART_MAX, Version, is_newer};

/// The repository releases are published to.
pub const DEFAULT_REPO: &str = "agentbusiness/CortexCode";
/// The release holding the catalog, and the catalog's file name in it.
pub const CATALOG_TAG: &str = "catalog";
pub const CATALOG_FILE: &str = "catalog.json";
pub const CATALOG_FORMAT: &str = "cortexcode-catalog/1";
/// The platform key of files that run anywhere (a plugin without a backend).
pub const ANY: &str = "any";
/// The platforms releases are built for.
pub const PLATFORMS: &[&str] = &["darwin-arm64", "linux-x64", "linux-arm64"];

/// Where the catalog of `repo` is downloaded from.
pub fn catalog_url(repo: &str) -> String {
    format!("https://github.com/{repo}/releases/download/{CATALOG_TAG}/{CATALOG_FILE}")
}

/// This machine's platform key, when releases are built for it.
pub fn platform() -> Option<&'static str> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => Some("darwin-arm64"),
        ("linux", "x86_64") => Some("linux-x64"),
        ("linux", "aarch64") => Some("linux-arm64"),
        _ => None,
    }
}

/// The operating system as plugin.toml's `platforms` names it (`macos`, `linux`).
pub fn os_name() -> &'static str {
    std::env::consts::OS
}

// ----- the catalog -----

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct Catalog {
    pub format: String,
    /// When the catalog was last written (RFC 3339, UTC).
    #[serde(default)]
    pub updated: String,
    #[serde(default)]
    pub repo: String,
    /// The installer new users download (the full app).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app: Option<AppEntry>,
    /// The frame, its window code and Electron.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frame: Option<FrameEntry>,
    /// Every released plugin by id.
    #[serde(default)]
    pub plugins: BTreeMap<String, PluginEntry>,
}

/// One downloadable file of a release.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct Asset {
    pub name: String,
    pub url: String,
    pub sha256: String,
    pub size: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct AppEntry {
    pub version: String,
    pub tag: String,
    /// By platform key.
    #[serde(default)]
    pub files: BTreeMap<String, AppFiles>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct AppFiles {
    /// The app as a `.tar.gz` (macOS: `CortexCode.app`; Linux: the `cortexcode` folder).
    pub archive: Asset,
    /// macOS: the same app in a disk image, for a download by hand.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dmg: Option<Asset>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct FrameEntry {
    pub version: String,
    pub tag: String,
    /// The Electron version the window runs on.
    pub electron: String,
    /// By platform key.
    #[serde(default)]
    pub files: BTreeMap<String, FrameFiles>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct FrameFiles {
    /// The frame folder: the frame, cx, cortex-pack, the window code, the frame's source.
    pub frame: Asset,
    /// Electron, branded; shared by every frame version that names the same Electron.
    pub electron: Asset,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct PluginEntry {
    pub version: String,
    pub tag: String,
    #[serde(default)]
    pub name: String,
    /// The frame version the plugin was built with; an older frame waits for its own update.
    #[serde(default)]
    pub min_frame: String,
    /// plugin.toml's `platforms` (`macos`, `linux`).
    #[serde(default)]
    pub platforms: Vec<String>,
    /// The digest of the plugin's source the package was built from.
    #[serde(default)]
    pub digest: String,
    /// By platform key, or one file under [`ANY`] for a plugin without a backend.
    #[serde(default)]
    pub files: BTreeMap<String, Asset>,
}

impl PluginEntry {
    /// The file for `platform`: its own, else the one that runs anywhere.
    pub fn file_for(&self, platform: &str) -> Option<&Asset> {
        self.files.get(platform).or_else(|| self.files.get(ANY))
    }
}

/// Read a downloaded catalog.
pub fn parse_catalog(bytes: &[u8]) -> Result<Catalog, String> {
    let catalog: Catalog = serde_json::from_slice(bytes).map_err(|e| format!("invalid catalog: {e}"))?;
    if catalog.format != CATALOG_FORMAT {
        return Err(format!("unknown catalog format `{}`", catalog.format));
    }
    Ok(catalog)
}
