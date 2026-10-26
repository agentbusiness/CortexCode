//! `update@1`: updates from the official releases (`docs/RELEASES.md`).
//!
//! The updater reads the release catalog of the official repository, installs the plugins the user
//! has not changed by themselves when no run is going, asks about the plugins the user changed,
//! and stages a newer frame for the next start ("Restart to update").

use serde::{Deserialize, Serialize};

pub mod methods {
    /// `{}` -> [`super::Status`].
    pub const STATUS: &str = "update.status";
    /// `{}` -> [`super::Status`]. Look at the catalog now (and apply what applies by itself).
    pub const CHECK: &str = "update.check";
    /// [`super::DecideParams`] -> [`super::Status`]. The answer for a plugin the user changed.
    pub const DECIDE: &str = "update.decide";
    /// `{}` -> [`super::Status`]. Restart on the staged frame.
    pub const RESTART: &str = "update.restart";
}

pub mod topics {
    /// [`super::Status`] whenever it changed.
    pub const CHANGED: &str = "update.changed";
}

/// The answers for a plugin the user changed.
pub mod choices {
    /// Merge the release into the user's version (built here when the user changed its code).
    pub const MERGE: &str = "merge";
    /// Keep the user's version; ask again at the next release of the plugin.
    pub const KEEP: &str = "keep";
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct Status {
    /// Updates are looked for (the plugin's `enabled` config).
    pub enabled: bool,
    /// checking | downloading | installing | idle
    pub state: String,
    /// What is happening, in plain words ("Downloading chatflow 0.3.2").
    #[serde(default)]
    pub detail: String,
    #[serde(default)]
    pub last_check_ms: u64,
    /// The last failure, in plain words.
    #[serde(default)]
    pub error: Option<String>,
    /// A newer frame is staged: "Restart to update".
    #[serde(default)]
    pub restart: Option<FrameUpdate>,
    /// Plugins the user changed that have a release waiting for an answer.
    #[serde(default)]
    pub pending: Vec<Pending>,
    /// Plugins waiting for no run to be going.
    #[serde(default)]
    pub waiting: Vec<String>,
    /// Recently installed updates, newest first.
    #[serde(default)]
    pub recent: Vec<Applied>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct FrameUpdate {
    pub running: String,
    pub staged: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct Pending {
    pub plugin: String,
    #[serde(default)]
    pub name: String,
    /// The released version.
    pub version: String,
    /// The version installed now.
    #[serde(default)]
    pub installed: String,
    /// The user's changed files.
    #[serde(default)]
    pub files: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct Applied {
    pub plugin: String,
    pub version: String,
    pub ms: u64,
    /// installed | built (the user's change merged and built here) | kept | failed
    pub outcome: String,
    #[serde(default)]
    pub detail: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DecideParams {
    pub plugin: String,
    /// The release version the answer is for.
    pub version: String,
    /// [`choices::MERGE`] or [`choices::KEEP`].
    pub choice: String,
}
