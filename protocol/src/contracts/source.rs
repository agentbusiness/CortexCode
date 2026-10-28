//! `source@1`: the live folder, every plugin's data (prompts, texts, stages, schemas, skills),
//! live and editable (see `docs/LIVE-EDITING.md`).
//!
//! The data of every installed plugin is one git repository in the live folder (`<home>/live` by
//! default), laid out as in CortexCode's repository (`plugins/<id>/data/...`). Plugins read their
//! data from there; an edit is in use at once and committed as it settles. The folder follows the
//! installed packages (a merge that keeps the user's edits), is backed up to the user's own GitHub
//! repository and shared with the official project by pull request. CortexCode's code is never
//! edited here: it changes only with a new package.

use serde::{Deserialize, Serialize};

pub mod methods {
    /// `{}` -> [`super::Status`].
    pub const STATUS: &str = "source.status";
    /// `{}` -> [`super::UpdateResult`]. Merge the installed packages' data into the live folder.
    pub const UPDATE: &str = "source.update";
    /// [`super::HistoryParams`] -> [`super::History`].
    pub const HISTORY: &str = "source.history";
    /// [`super::RevertParams`] -> [`super::RevertResult`].
    pub const REVERT: &str = "source.revert";
    /// [`super::DiffParams`] -> [`super::Diff`].
    pub const DIFF: &str = "source.diff";
    /// `{}` -> [`super::Backup`].
    pub const BACKUP_STATUS: &str = "source.backup.status";
    /// [`super::BackupSetParams`] -> [`super::Backup`].
    pub const BACKUP_SET: &str = "source.backup.set";
    /// `{}` -> [`super::Backup`]. Push the live folder to the backup repository.
    pub const BACKUP_PUSH: &str = "source.backup.push";
    /// [`super::RestoreParams`] -> [`super::RestoreResult`]. Replace the live folder's changes with
    /// the backup's.
    pub const BACKUP_RESTORE: &str = "source.backup.restore";
    /// `{}` -> [`super::ShareCandidates`]. The user's changes not shared yet, each with the
    /// privacy check's findings.
    pub const SHARE_CANDIDATES: &str = "source.share.candidates";
    /// [`super::ShareParams`] -> [`super::Shared`]. A pull request to the official repository.
    pub const SHARE_SEND: &str = "source.share.send";
    /// `{}` -> [`super::SharedList`]. Shared pull requests and their state.
    pub const SHARE_STATUS: &str = "source.share.status";
    /// `{}` -> [`super::Storage`].
    pub const STORAGE: &str = "source.storage";
    /// [`super::ReleaseStatusParams`] -> [`super::ReleaseStatus`]. Whether the user changed a
    /// plugin's data away from its package (an update of it then asks first).
    pub const RELEASE_STATUS: &str = "source.release.status";
    /// [`super::ReleaseMergeParams`] -> [`super::UpdateResult`]. Merge a package's data into the
    /// plugin's folder of the live folder, keeping the user's edits.
    pub const RELEASE_MERGE: &str = "source.release.merge";
    /// [`super::ReleaseResetParams`] -> [`super::ReleaseResetResult`]. Put one plugin's folder back
    /// to its package's data: the user's edits in it (committed or not) are dropped, the rest of
    /// the live folder stays.
    pub const RELEASE_RESET: &str = "source.release.reset";
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ReleaseStatusParams {
    pub plugin: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ReleaseStatus {
    pub plugin: String,
    /// The live folder is managed (false for a developer checkout: nothing is merged there).
    pub managed: bool,
    /// The user changed the plugin's data (committed or not) away from its package.
    pub edited: bool,
    /// The changed files, relative to the live folder.
    #[serde(default)]
    pub files: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ReleaseMergeParams {
    pub plugin: String,
    /// The unpacked package (its `manifest.json` names the data folders).
    pub dir: String,
    /// The release version, for the history.
    #[serde(default)]
    pub version: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ReleaseResetParams {
    pub plugin: String,
    /// The unpacked package (its `manifest.json` names the data folders).
    pub dir: String,
    /// The release version, for the history.
    #[serde(default)]
    pub version: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ReleaseResetResult {
    /// reset | up_to_date | unmanaged
    pub state: String,
    /// The files put back (relative to the live folder).
    #[serde(default)]
    pub files: Vec<String>,
}

pub mod topics {
    /// `{}`: status, history or backup changed, or the live data moved (refetch, read again).
    pub const CHANGED: &str = "source.changed";
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Status {
    /// The live folder.
    pub live_dir: String,
    /// Managed by this plugin (populated, committed, updated). False for a developer checkout
    /// configured as the live folder: then nothing is committed or merged automatically.
    pub managed: bool,
    /// The packages' data the live folder was last updated from (its commit in the live folder).
    #[serde(default)]
    pub shipped: Option<String>,
    /// up_to_date | conflicts | no_source
    pub update: String,
    /// Files in conflict when the last update could not merge.
    #[serde(default)]
    pub conflicts: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct UpdateResult {
    /// up_to_date | merged | conflicts | no_source
    pub state: String,
    #[serde(default)]
    pub conflicts: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct HistoryParams {
    #[serde(default)]
    pub limit: Option<usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct History {
    pub changes: Vec<Change>,
}

/// One change in the live folder (a commit), newest first.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Change {
    pub commit: String,
    pub ms: u64,
    /// edit | update | restore | revert | shipped
    pub origin: String,
    /// Plain-English summary.
    pub summary: String,
    pub plugins: Vec<String>,
    #[serde(default)]
    pub files: Vec<String>,
    /// The commit that reverted this one, when it was reverted.
    #[serde(default)]
    pub reverted_by: Option<String>,
    /// The pull request it was shared in, when shared.
    #[serde(default)]
    pub shared: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RevertParams {
    pub commit: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RevertResult {
    pub commit: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct DiffParams {
    /// The commit whose changes to show.
    pub commit: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Diff {
    pub files: Vec<FileDiff>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FileDiff {
    pub path: String,
    /// added | modified | deleted | renamed
    pub status: String,
    /// Unified diff text.
    pub patch: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Backup {
    /// `owner/name` of the backup repository, when one is set.
    #[serde(default)]
    pub repo: Option<String>,
    /// Push after every change.
    pub auto: bool,
    #[serde(default)]
    pub last_push_ms: Option<u64>,
    #[serde(default)]
    pub last_error: Option<String>,
    /// The branch the live folder is pushed to.
    pub branch: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct BackupSetParams {
    /// `owner/name`; empty removes the backup repository.
    #[serde(default)]
    pub repo: Option<String>,
    #[serde(default)]
    pub auto: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RestoreParams {
    /// `owner/name` (default: the backup repository).
    #[serde(default)]
    pub repo: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RestoreResult {
    pub commit: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ShareCandidates {
    pub changes: Vec<ShareCandidate>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ShareCandidate {
    pub commit: String,
    pub ms: u64,
    pub summary: String,
    pub plugins: Vec<String>,
    pub files: Vec<String>,
    /// What the privacy check found in the change's added lines.
    pub findings: Vec<Finding>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Finding {
    pub path: String,
    pub line: u32,
    /// The line, shortened.
    pub text: String,
    /// path | project | secret
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ShareParams {
    pub commits: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Shared {
    pub url: String,
    pub number: u64,
    pub branch: String,
    pub commits: Vec<String>,
    /// open | merged | closed
    pub state: String,
    pub ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SharedList {
    pub shared: Vec<Shared>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Storage {
    pub home: String,
    pub items: Vec<StorageItem>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct StorageItem {
    /// Plain name ("Models", "Conversations", ...).
    pub name: String,
    pub path: String,
    pub bytes: u64,
}
