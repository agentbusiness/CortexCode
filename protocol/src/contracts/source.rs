//! `source@1`: CortexCode's own source, live and editable (see `docs/LIVE-EDITING.md`).
//!
//! The full source (every plugin, the shared libraries, the frame) is one git repository in the
//! code folder (`<home>/code` by default). Plugins read their data (skills, guides, prompts, texts)
//! from there live. Edits to data are committed as they happen; edits to code become a proposal the
//! user approves (a plain-English summary), and an approved proposal is built, packed and
//! hot-swapped. The same repository is updated from the shipped source (a merge), backed up to the
//! user's own GitHub repository and shared with the official project by pull request.

use serde::{Deserialize, Serialize};

pub mod methods {
    /// `{}` -> [`super::Status`].
    pub const STATUS: &str = "source.status";
    /// `{}` -> [`super::UpdateResult`]. Merge the shipped source into the code folder.
    pub const UPDATE: &str = "source.update";
    /// [`super::HistoryParams`] -> [`super::History`].
    pub const HISTORY: &str = "source.history";
    /// [`super::RevertParams`] -> [`super::RevertResult`].
    pub const REVERT: &str = "source.revert";
    /// [`super::DiffParams`] -> [`super::Diff`].
    pub const DIFF: &str = "source.diff";
    /// `{}` -> [`super::Proposals`].
    pub const PROPOSALS: &str = "source.proposals";
    /// [`super::ProposeParams`] -> [`super::Proposal`]. A proposal from the code folder's
    /// uncommitted code changes (the summary is written by the model when not given).
    pub const PROPOSE: &str = "source.propose";
    /// [`super::DecideParams`] -> [`super::Proposal`]. Approve (build, pack, hot-swap) or decline.
    pub const DECIDE: &str = "source.decide";
    /// `{}` -> [`super::Backup`].
    pub const BACKUP_STATUS: &str = "source.backup.status";
    /// [`super::BackupSetParams`] -> [`super::Backup`].
    pub const BACKUP_SET: &str = "source.backup.set";
    /// `{}` -> [`super::Backup`]. Push the code folder to the backup repository.
    pub const BACKUP_PUSH: &str = "source.backup.push";
    /// [`super::RestoreParams`] -> [`super::RestoreResult`]. Replace the code folder's changes with
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
    /// plugin away from the shipped source (an update of it then asks first).
    pub const RELEASE_STATUS: &str = "source.release.status";
    /// [`super::ReleaseMergeParams`] -> [`super::UpdateResult`]. Merge a released source (one
    /// plugin's, or the frame's: everything outside `plugins/`) into the code folder, keeping the
    /// user's changes; the user's own code on the new base is built again.
    pub const RELEASE_MERGE: &str = "source.release.merge";
    /// [`super::ReleaseResetParams`] -> [`super::ReleaseResetResult`]. Put one plugin's folder back
    /// to its released source: the user's changes in it (committed or not) are dropped, the rest
    /// of the code folder stays.
    pub const RELEASE_RESET: &str = "source.release.reset";
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ReleaseStatusParams {
    pub plugin: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ReleaseStatus {
    pub plugin: String,
    /// The code folder is managed (false for a developer checkout: nothing is merged there).
    pub managed: bool,
    /// The user changed the plugin's files (committed or not) away from the shipped source.
    pub edited: bool,
    /// The changed files, relative to the code folder.
    #[serde(default)]
    pub files: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ReleaseMergeParams {
    /// `plugin` (one plugin's folder, `plugins/<plugin>`) or `frame` (everything outside `plugins/`).
    pub scope: String,
    #[serde(default)]
    pub plugin: String,
    /// The released source: a folder holding exactly the files of that scope.
    pub dir: String,
    /// The release version, for the history.
    #[serde(default)]
    pub version: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ReleaseResetParams {
    pub plugin: String,
    /// The released source: a folder holding exactly the plugin's files.
    pub dir: String,
    /// The release version, for the history.
    #[serde(default)]
    pub version: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ReleaseResetResult {
    /// reset | up_to_date | unmanaged
    pub state: String,
    /// The files put back (relative to the code folder).
    #[serde(default)]
    pub files: Vec<String>,
}

pub mod topics {
    /// `{}`: status, history, proposals or backup changed (refetch).
    pub const CHANGED: &str = "source.changed";
    /// [`super::Progress`] while a proposal builds.
    pub const PROGRESS: &str = "source.progress";
}

/// The journal entry kind of a code-change card in a session (`data`: a [`Proposal`]).
pub const CHANGE_ENTRY: &str = "change";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Progress {
    pub proposal: String,
    /// The plugin being built, or empty for the whole proposal.
    #[serde(default)]
    pub plugin: String,
    /// started | output | done | failed
    pub state: String,
    #[serde(default)]
    pub detail: String,
    #[serde(default)]
    pub ts_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Status {
    /// The code folder.
    pub code_dir: String,
    /// Managed by this plugin (populated, committed, updated). False for a developer checkout
    /// configured as the code folder: then nothing is committed or merged automatically.
    pub managed: bool,
    /// The shipped source the code folder was last updated from (its commit in the code folder).
    #[serde(default)]
    pub shipped: Option<String>,
    /// up_to_date | conflicts | no_source
    pub update: String,
    /// Files in conflict when the last update could not merge.
    #[serde(default)]
    pub conflicts: Vec<String>,
    pub plugins: Vec<PluginEditing>,
    pub tools: Vec<Toolchain>,
    /// Proposals waiting for an answer.
    pub pending: usize,
}

/// What can be edited in one plugin on this Mac.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PluginEditing {
    pub id: String,
    pub name: String,
    /// Always true: data (skills, guides, prompts, texts) needs no tools.
    pub data_editable: bool,
    /// Every tool its build needs is present.
    pub code_editable: bool,
    /// What its build needs (`[build] needs` in its plugin.toml).
    pub needs: Vec<String>,
    /// Plain names of what is missing ("Rust", "Apple's command line tools", ...).
    pub missing: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Toolchain {
    /// rust | apple-clt | metal | node
    pub name: String,
    /// Plain name for the user.
    pub label: String,
    pub found: bool,
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default)]
    pub detail: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct UpdateResult {
    /// up_to_date | merged | conflicts | no_source
    pub state: String,
    #[serde(default)]
    pub conflicts: Vec<String>,
    /// Plugins rebuilt because the user's own code changes sit on the new base.
    #[serde(default)]
    pub rebuilt: Vec<String>,
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

/// One change in the code folder (a commit), newest first.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Change {
    pub commit: String,
    pub ms: u64,
    /// edit | approved | update | restore | revert | shipped
    pub origin: String,
    /// Plain-English summary.
    pub summary: String,
    pub plugins: Vec<String>,
    /// data | code | mixed
    pub kind: String,
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
    /// Plugins rebuilt and hot-swapped because code was reverted.
    #[serde(default)]
    pub rebuilt: Vec<String>,
    /// Parts that take effect at the next start (the frame, the app shell).
    #[serde(default)]
    pub next_start: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct DiffParams {
    /// A proposal's changes.
    #[serde(default)]
    pub proposal: Option<String>,
    /// A commit's changes.
    #[serde(default)]
    pub commit: Option<String>,
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
pub struct Proposals {
    pub proposals: Vec<Proposal>,
}

/// A code change waiting for, or answered by, the user.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Proposal {
    pub id: String,
    pub created_ms: u64,
    /// The plugins whose code changes (plugin ids; `frame` / `shell` for parts that need a restart).
    pub plugins: Vec<String>,
    /// Plain-English: what changes.
    pub summary: String,
    /// Plain-English: why.
    #[serde(default)]
    pub why: String,
    /// Plain-English: what else it can affect.
    #[serde(default)]
    pub risk: String,
    pub files: Vec<String>,
    /// The session the card is in, when the change was made in a conversation.
    #[serde(default)]
    pub session_id: Option<String>,
    /// The card's journal entry in that session.
    #[serde(default)]
    pub entry_seq: Option<u64>,
    /// pending | building | done | failed | rolled_back | declined
    pub state: String,
    /// What happened, in plain words (the build failure, the rollback, what needs a restart).
    #[serde(default)]
    pub outcome: String,
    /// The draft commit holding the change.
    pub draft: String,
    /// The commit on the code folder's branch once approved.
    #[serde(default)]
    pub commit: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ProposeParams {
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub summary: Option<String>,
    #[serde(default)]
    pub why: Option<String>,
    #[serde(default)]
    pub risk: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DecideParams {
    pub id: String,
    pub approve: bool,
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
    /// The branch the code folder is pushed to.
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
    #[serde(default)]
    pub rebuilt: Vec<String>,
    /// Plugins whose restored code could not be built on this Mac (missing tools).
    #[serde(default)]
    pub not_built: Vec<String>,
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
