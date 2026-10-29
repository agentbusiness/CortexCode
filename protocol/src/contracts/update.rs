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
    /// [`super::DismissParams`] -> [`super::Status`]. The notice about a version that could not
    /// start has been seen.
    pub const DISMISS: &str = "update.dismiss";
    /// `{}` -> [`super::ResetPlan`]. What a reset of every plugin to its official release would do.
    pub const RESET_PLAN: &str = "update.reset_plan";
    /// [`super::ResetParams`] -> [`super::ResetResult`]. Put the named plugins (none named: every
    /// plugin) back to their official release: the user's edits to their data in the live folder
    /// are dropped and the release package installed. Refused while a run is going.
    pub const RESET: &str = "update.reset";
}

pub mod topics {
    /// [`super::Status`] whenever it changed.
    pub const CHANGED: &str = "update.changed";
}

/// The answers for a plugin the user changed.
pub mod choices {
    /// Use the newer version: the user's changes to the plugin are dropped and the package installed.
    pub const USE: &str = "use";
    /// Merge the release's data into the user's edited version.
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
    /// Newer versions that could not start (the previous version keeps running), one per plugin.
    #[serde(default)]
    pub failed: Vec<Failed>,
    /// The updates of the current (or last) round, all together: one row per plugin, and one for
    /// the frame (`plugin` "frame").
    #[serde(default)]
    pub round: Vec<RoundItem>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct RoundItem {
    pub plugin: String,
    #[serde(default)]
    pub name: String,
    pub version: String,
    /// queued | downloading | installing | done | at_restart | failed
    pub state: String,
    /// The download's progress, 0 to 100.
    #[serde(default)]
    pub percent: Option<u64>,
    /// Why it failed, in plain words.
    #[serde(default)]
    pub error: String,
    /// When it finished (0 while it is going).
    #[serde(default)]
    pub ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct Failed {
    pub plugin: String,
    #[serde(default)]
    pub name: String,
    /// The version that could not start.
    pub version: String,
    /// Why, in plain words.
    #[serde(default)]
    pub error: String,
    pub ms: u64,
    /// The user has seen the notice (it stays in Settings > Plugins).
    #[serde(default)]
    pub seen: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DismissParams {
    pub plugin: String,
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
    /// The newer version.
    pub version: String,
    /// One of [`reset_sources`]: an official release, or the build the app came with.
    #[serde(default)]
    pub from: String,
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
    /// installed | kept | failed
    pub outcome: String,
    #[serde(default)]
    pub detail: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DecideParams {
    pub plugin: String,
    /// The release version the answer is for.
    pub version: String,
    /// [`choices::USE`], [`choices::KEEP`] or [`choices::MERGE`].
    pub choice: String,
}

/// What a reset would do to one plugin.
pub mod reset_actions {
    /// Drop the user's changes and install the release package.
    pub const INSTALL: &str = "install";
    /// The release package runs already; only the user's edits in the live folder go.
    pub const SOURCE: &str = "source";
    /// Already the release, unchanged.
    pub const NONE: &str = "none";
    /// The release needs a newer frame: it waits for the frame update.
    pub const FRAME: &str = "frame";
    /// Not in the official releases: left as it is.
    pub const UNOFFICIAL: &str = "unofficial";
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct ResetPlan {
    pub plugins: Vec<ResetItem>,
    /// A run is going: the reset is refused until it ends.
    #[serde(default)]
    pub running: bool,
    /// The official releases could not be read (why); the app's own packages stand in.
    #[serde(default)]
    pub offline: Option<String>,
}

/// Where a reset's version comes from.
pub mod reset_sources {
    /// The official release from the catalog.
    pub const RELEASE: &str = "release";
    /// The package the app itself carries.
    pub const APP: &str = "app";
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct ResetItem {
    pub plugin: String,
    #[serde(default)]
    pub name: String,
    /// The version installed now.
    #[serde(default)]
    pub installed: String,
    /// The version the reset goes to (empty when there is none).
    #[serde(default)]
    pub release: String,
    /// One of [`reset_sources`] (empty when there is none).
    #[serde(default)]
    pub from: String,
    /// The user's changed files in the live folder.
    #[serde(default)]
    pub files: Vec<String>,
    /// One of [`reset_actions`].
    pub action: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct ResetParams {
    /// The plugins to reset; empty: every plugin.
    #[serde(default)]
    pub plugins: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct ResetResult {
    /// One outcome per plugin the reset touched: reset | failed.
    pub plugins: Vec<Applied>,
}
