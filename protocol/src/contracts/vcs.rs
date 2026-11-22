//! `vcs@1`: git (and GitHub through `gh` when online and asked). Never contacts a remote on its own.

use serde::{Deserialize, Serialize};

pub mod methods {
    pub const STATUS: &str = "vcs.status";
    /// scope: uncommitted (working tree incl. untracked vs HEAD) | unstaged (working tree vs index)
    /// | staged (index vs HEAD) | branch (HEAD vs merge-base with `base`) | commit:<sha> (vs first parent)
    /// | since:<rev> (working tree incl. untracked vs that commit)
    pub const DIFF: &str = "vcs.diff";
    pub const LOG: &str = "vcs.log";
    pub const BRANCHES: &str = "vcs.branches";
    /// [`BranchParams`] -> [`BranchesResult`]. Creates without switching.
    pub const BRANCH_CREATE: &str = "vcs.branch.create";
    /// [`BranchParams`]: switch to `name`; with `from`, create `name` from `from` first.
    pub const CHECKOUT: &str = "vcs.checkout";
    /// [`PathsParams`]. With `hunk`: the index of the hunk in the `unstaged` diff of that one path.
    pub const STAGE: &str = "vcs.stage";
    /// [`PathsParams`]. With `hunk`: the index of the hunk in the `staged` diff of that one path.
    pub const UNSTAGE: &str = "vcs.unstage";
    /// Discard working-tree changes of paths or one hunk (index in the `unstaged` diff).
    /// Untracked paths named explicitly are deleted.
    pub const REVERT: &str = "vcs.revert";
    pub const COMMIT: &str = "vcs.commit";
    /// Shadow-repository checkpoint of the working tree (does not touch the user's history).
    pub const CHECKPOINT: &str = "vcs.checkpoint";
    pub const CHECKPOINTS: &str = "vcs.checkpoints";
    /// [`RestoreParams`] -> [`RestoreResult`]. Takes a backup checkpoint of the current tree first.
    pub const RESTORE: &str = "vcs.restore";
    /// Merge a branch (e.g. an agent's) into the current branch of `root`.
    pub const MERGE: &str = "vcs.merge";
    /// [`RootParams`] -> [`MergeAbortResult`]: abandon a conflicted merge and restore the pre-merge state.
    pub const MERGE_ABORT: &str = "vcs.merge.abort";
    /// [`PushParams`] -> [`PushResult`]. Contacts the remote only when called.
    pub const PUSH: &str = "vcs.push";
    /// GitHub pull-request and CI state for the current branch (requires `gh` and network).
    pub const PR_STATUS: &str = "vcs.pr.status";
    /// [`PrCreateParams`] -> [`PrStatus`].
    pub const PR_CREATE: &str = "vcs.pr.create";

    /// `{}` -> [`super::GithubStatus`]. The GitHub connection (token kept in the macOS Keychain).
    pub const GITHUB_STATUS: &str = "vcs.github.status";
    /// `{}` -> [`super::DeviceCode`]. Start GitHub's device sign-in: the user enters the code at the
    /// address shown; [`GITHUB_DEVICE_POLL`] finishes it.
    pub const GITHUB_DEVICE_START: &str = "vcs.github.device.start";
    /// `{}` -> [`super::GithubStatus`] (`pending` while the user has not approved yet).
    pub const GITHUB_DEVICE_POLL: &str = "vcs.github.device.poll";
    /// [`super::TokenParams`] -> [`super::GithubStatus`]. Connect with a personal access token.
    pub const GITHUB_TOKEN_SET: &str = "vcs.github.token.set";
    /// `{}` -> [`super::GithubStatus`].
    pub const GITHUB_DISCONNECT: &str = "vcs.github.disconnect";
    /// [`super::ReposParams`] -> [`super::Repos`]. The connected user's repositories.
    pub const GITHUB_REPOS: &str = "vcs.github.repos";
    /// [`super::RepoCreateParams`] -> [`super::Repo`].
    pub const GITHUB_REPO_CREATE: &str = "vcs.github.repo.create";
    /// [`super::ForkParams`] -> [`super::Repo`]. The user's fork of a repository (made when missing).
    pub const GITHUB_FORK: &str = "vcs.github.fork";
    /// [`super::GithubPrParams`] -> [`super::GithubPr`]. Open a pull request.
    pub const GITHUB_PR_CREATE: &str = "vcs.github.pr.create";
    /// [`super::GithubPrStateParams`] -> [`super::GithubPr`].
    pub const GITHUB_PR_STATE: &str = "vcs.github.pr.state";
    /// `{}` -> [`super::Credential`]. For CortexCode's own plugins that push with the connection.
    pub const GITHUB_CREDENTIAL: &str = "vcs.github.credential";
    /// [`super::CloneParams`] -> [`super::CloneResult`]. Clone a repository into a folder (no
    /// git program needed).
    pub const CLONE: &str = "vcs.clone";
    /// [`super::DevFolderParams`] -> [`super::GithubStatus`]. The folder new clones go into.
    pub const DEV_FOLDER_SET: &str = "vcs.dev_folder.set";

    // A runner's work on its own clones (`runner@1`): a bare mirror of a source, one working tree
    // per job. Called by host code only; `root`/`repo` is a working tree or a bare repository.
    /// [`super::FetchParams`] -> [`super::FetchResult`]. Contacts the remote only when called.
    pub const FETCH: &str = "vcs.fetch";
    /// [`super::MirrorParams`] -> [`super::MirrorResult`]: a bare repository of `url` at `path`
    /// (made when absent), then fetched with prune. The source's branches are
    /// `refs/remotes/origin/*`; `refs/heads/*` are the runner's own, so a prune never takes one.
    pub const MIRROR: &str = "vcs.mirror";
    /// [`super::WorktreeAddParams`] -> [`super::WorktreeResult`]: a working tree of `repo` at
    /// `path` on `branch`, made from `from` when the branch is new.
    pub const WORKTREE_ADD: &str = "vcs.worktree.add";
    /// [`super::WorktreeRemoveParams`] -> `{ok}`. The branch stays.
    pub const WORKTREE_REMOVE: &str = "vcs.worktree.remove";
    /// [`super::RebaseParams`] -> [`super::RebaseResult`]: the current branch onto `onto`. A
    /// conflict aborts the rebase and names the files.
    pub const REBASE: &str = "vcs.rebase";
    /// [`super::ResetParams`] -> [`super::RevResult`]: the working tree and index back to a commit
    /// (`clean`: untracked files go too; ignored files stay). Discards work: host code only.
    pub const RESET: &str = "vcs.reset";
    /// [`super::SquashParams`] -> [`super::SquashResult`]: the commits after `base` rewritten as
    /// one commit per group, grouped by a trailer of their messages.
    pub const SQUASH: &str = "vcs.squash";
    /// [`super::AdvanceParams`] -> [`super::RevResult`]: fast-forward a branch that no working
    /// tree has checked out to a commit; refused with `conflict` when it is not a fast-forward.
    pub const BRANCH_ADVANCE: &str = "vcs.branch.advance";
    /// [`super::RevParams`] -> [`super::RevResult`]: the commit a revision names.
    pub const REV: &str = "vcs.rev";
    /// [`super::ShowParams`] -> `{text}`: a file as it is at a revision, without a checkout.
    pub const SHOW: &str = "vcs.show";
    /// [`super::GrepParams`] -> [`super::GrepResult`]: a fixed string in the files of a revision.
    pub const GREP: &str = "vcs.grep";
    /// [`super::TreeParams`] -> [`super::TreeResult`]: a folder of a revision (folders end in `/`).
    pub const TREE: &str = "vcs.tree";
}

pub mod topics {
    /// [`ChangedEvent`] when HEAD, a ref or the index of a repository the plugin has served changed.
    pub const CHANGED: &str = "vcs.changed";
}

/// Payload of [`topics::CHANGED`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ChangedEvent {
    pub root: String,
    #[serde(default)]
    pub head: Option<String>,
    #[serde(default)]
    pub branch: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RootParams {
    pub root: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FileStatus {
    pub path: String,
    /// added | modified | deleted | renamed | untracked | conflicted
    pub status: String,
    pub staged: bool,
    #[serde(default)]
    pub added: u32,
    #[serde(default)]
    pub removed: u32,
    /// Source path of a rename or copy.
    #[serde(default)]
    pub old_path: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Status {
    pub root: String,
    pub is_repo: bool,
    pub branch: String,
    #[serde(default)]
    pub upstream: Option<String>,
    pub ahead: u32,
    pub behind: u32,
    pub files: Vec<FileStatus>,
    pub added_lines: u32,
    pub removed_lines: u32,
    #[serde(default)]
    pub head: Option<String>,
    /// HEAD is detached (`branch` is then "HEAD").
    #[serde(default)]
    pub detached: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DiffParams {
    pub root: String,
    #[serde(default = "default_scope")]
    pub scope: String,
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub base: Option<String>,
}

fn default_scope() -> String {
    "uncommitted".to_owned()
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DiffLine {
    /// "+", "-" or " "
    pub kind: String,
    pub text: String,
    #[serde(default)]
    pub old_line: Option<u32>,
    #[serde(default)]
    pub new_line: Option<u32>,
    /// The line has no trailing newline (`\ No newline at end of file` in the patch).
    #[serde(default)]
    pub no_eol: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Hunk {
    pub header: String,
    pub old_start: u32,
    pub old_lines: u32,
    pub new_start: u32,
    pub new_lines: u32,
    pub lines: Vec<DiffLine>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FileDiff {
    pub path: String,
    #[serde(default)]
    pub old_path: Option<String>,
    pub status: String,
    pub binary: bool,
    /// The change is too large to carry: `hunks` is empty, the counts still say how much.
    #[serde(default)]
    pub truncated: bool,
    pub hunks: Vec<Hunk>,
    #[serde(default)]
    pub added: u32,
    #[serde(default)]
    pub removed: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Diff {
    pub files: Vec<FileDiff>,
    pub added: u32,
    pub removed: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LogParams {
    pub root: String,
    #[serde(default = "default_log_limit")]
    pub limit: u32,
}

fn default_log_limit() -> u32 {
    50
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Commit {
    pub sha: String,
    pub author: String,
    pub ts_ms: u64,
    pub subject: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LogResult {
    pub commits: Vec<Commit>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BranchesResult {
    pub current: String,
    pub branches: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BranchParams {
    pub root: String,
    pub name: String,
    #[serde(default)]
    pub from: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PathsParams {
    pub root: String,
    pub paths: Vec<String>,
    /// Restrict to one hunk (index in `vcs.diff` of that path).
    #[serde(default)]
    pub hunk: Option<u32>,
    /// `vcs.revert` only: also discard staged changes (restore the paths to HEAD).
    #[serde(default)]
    pub include_staged: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CommitParams {
    pub root: String,
    pub message: String,
    /// Stage everything before committing.
    #[serde(default)]
    pub all: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CommitResult {
    pub sha: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CheckpointParams {
    pub root: String,
    pub label: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Checkpoint {
    pub id: String,
    pub root: String,
    pub label: String,
    pub ts_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CheckpointsResult {
    pub checkpoints: Vec<Checkpoint>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RestoreParams {
    pub root: String,
    pub id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MergeParams {
    pub root: String,
    pub branch: String,
    #[serde(default)]
    pub message: Option<String>,
    /// On conflict, report the conflicting files and abort, restoring the pre-merge state.
    /// Otherwise the repository is left mid-merge (conflict markers in the files) for resolution.
    #[serde(default)]
    pub abort_on_conflict: bool,
    /// Merge into a working tree that has uncommitted changes, without touching them: the merge is
    /// computed apart from the working tree (`git merge-tree`), so a conflict leaves nothing
    /// half-merged (it is reported as with `abort_on_conflict`), and the working tree is then moved
    /// to the merge commit keeping every local change. When the merge would change a file that has
    /// uncommitted changes (or replace an untracked file), nothing changes and those files are
    /// reported in [`MergeResult::blocked_by_local`]. Without it, a dirty tracked tree is refused.
    #[serde(default)]
    pub keep_local_changes: bool,
    /// With `keep_local_changes`: always make a merge commit (with `message`), also when HEAD could
    /// fast-forward to the branch.
    #[serde(default)]
    pub no_ff: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MergeResult {
    pub ok: bool,
    #[serde(default)]
    pub conflicts: Vec<String>,
    #[serde(default)]
    pub sha: Option<String>,
    /// The merge was aborted after conflicts were found (`abort_on_conflict`).
    #[serde(default)]
    pub aborted: bool,
    /// `keep_local_changes`: files with uncommitted changes, or untracked files, that the merge
    /// would change. Nothing was changed; the merge can go ahead once they are committed, stashed or
    /// moved.
    #[serde(default)]
    pub blocked_by_local: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MergeAbortResult {
    #[serde(default)]
    pub head: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RestoreResult {
    pub restored: Checkpoint,
    /// Checkpoint of the tree as it was just before the restore (restore it to undo).
    pub backup: Checkpoint,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PushParams {
    pub root: String,
    /// Default: the branch's upstream remote, else `origin`, else the only remote.
    #[serde(default)]
    pub remote: Option<String>,
    /// Default: the current branch.
    #[serde(default)]
    pub branch: Option<String>,
    /// A GitHub token to push with instead of the sign-in from Settings (a manager's connection
    /// for this repository). Used for this call only, never kept.
    #[serde(default)]
    pub token: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PushResult {
    /// The push was accepted by the remote.
    pub ok: bool,
    /// The remote could be reached (false: offline, unknown host, authentication failure...).
    pub available: bool,
    #[serde(default)]
    pub remote: String,
    #[serde(default)]
    pub branch: String,
    #[serde(default)]
    pub detail: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PrCreateParams {
    pub root: String,
    pub title: String,
    #[serde(default)]
    pub body: String,
    #[serde(default)]
    pub base: Option<String>,
    #[serde(default)]
    pub draft: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PrStatus {
    pub available: bool,
    #[serde(default)]
    pub number: Option<u64>,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub state: Option<String>,
    /// passing | failing | pending | none
    #[serde(default)]
    pub ci: Option<String>,
    #[serde(default)]
    pub detail: String,
}

/// The GitHub connection.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct GithubStatus {
    pub connected: bool,
    /// A device sign-in is waiting for the user.
    #[serde(default)]
    pub pending: bool,
    #[serde(default)]
    pub login: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub avatar_url: Option<String>,
    /// device | token
    #[serde(default)]
    pub method: Option<String>,
    /// The token's scopes, as GitHub reports them.
    #[serde(default)]
    pub scopes: Vec<String>,
    /// Device sign-in is available (a GitHub app client id is configured).
    pub device_available: bool,
    /// The folder new clones go into.
    #[serde(default)]
    pub dev_folder: Option<String>,
    #[serde(default)]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DeviceCode {
    pub user_code: String,
    pub verification_uri: String,
    pub expires_in: u64,
    /// Seconds between polls.
    pub interval: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TokenParams {
    pub token: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ReposParams {
    /// Filter by name (case-insensitive substring).
    #[serde(default)]
    pub query: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Repos {
    pub repos: Vec<Repo>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Repo {
    /// `owner/name`
    pub full_name: String,
    pub private: bool,
    #[serde(default)]
    pub description: String,
    pub clone_url: String,
    pub default_branch: String,
    #[serde(default)]
    pub updated_ms: u64,
    #[serde(default)]
    pub fork: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RepoCreateParams {
    pub name: String,
    #[serde(default = "yes")]
    pub private: bool,
    #[serde(default)]
    pub description: String,
}

fn yes() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ForkParams {
    /// `owner/name` of the repository to fork.
    pub repo: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct GithubPrParams {
    /// `owner/name` the pull request goes to.
    pub repo: String,
    /// `user:branch`
    pub head: String,
    pub base: String,
    pub title: String,
    #[serde(default)]
    pub body: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct GithubPrStateParams {
    pub repo: String,
    pub number: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct GithubPr {
    pub url: String,
    pub number: u64,
    /// open | merged | closed
    pub state: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Credential {
    pub login: String,
    pub token: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CloneParams {
    /// `owner/name` (GitHub) or any https clone URL.
    pub repo: String,
    /// The folder the clone goes into (default: the dev folder).
    #[serde(default)]
    pub parent: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CloneResult {
    /// The new folder: `<parent>/<repository name>`.
    pub path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DevFolderParams {
    pub path: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct FetchParams {
    /// A working tree or a bare repository.
    pub root: String,
    /// Default `origin`.
    #[serde(default)]
    pub remote: Option<String>,
    /// Remove remote-tracking branches the remote no longer has.
    #[serde(default)]
    pub prune: bool,
    /// As [`PushParams::token`].
    #[serde(default)]
    pub token: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct FetchResult {
    pub ok: bool,
    /// The remote could be reached.
    pub available: bool,
    #[serde(default)]
    pub remote: String,
    #[serde(default)]
    pub detail: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct MirrorParams {
    /// Any git remote: an https or ssh address, or a folder.
    pub url: String,
    /// Absolute path of the bare repository.
    pub path: String,
    /// As [`PushParams::token`].
    #[serde(default)]
    pub token: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct MirrorResult {
    pub path: String,
    /// The repository was made by this call.
    pub created: bool,
    /// The fetch succeeded.
    pub ok: bool,
    pub available: bool,
    #[serde(default)]
    pub detail: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct WorktreeAddParams {
    /// The repository the working tree belongs to (a mirror).
    pub repo: String,
    /// Absolute path of the new working tree.
    pub path: String,
    pub branch: String,
    /// The commit a new branch starts from (ignored when the branch exists).
    pub from: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct WorktreeResult {
    pub path: String,
    pub branch: String,
    pub head: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct WorktreeRemoveParams {
    pub repo: String,
    pub path: String,
    /// Remove it also with uncommitted changes in it.
    #[serde(default)]
    pub force: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct RebaseParams {
    pub root: String,
    pub onto: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct RebaseResult {
    pub ok: bool,
    /// Files in conflict; the rebase was aborted and the branch is as it was.
    #[serde(default)]
    pub conflicts: Vec<String>,
    #[serde(default)]
    pub head: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ResetParams {
    pub root: String,
    pub to: String,
    #[serde(default)]
    pub clean: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct RevParams {
    pub root: String,
    pub rev: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct RevResult {
    /// None when the revision names no commit.
    #[serde(default)]
    pub sha: Option<String>,
}

/// One commit a squash makes: the commits whose trailer has `value`, under `message`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct SquashGroup {
    pub value: String,
    pub message: String,
}

/// `vcs.squash`: each group's commit holds the tree of the last commit after `base` whose
/// `trailer` line has the group's value, in the order those commits come (a commit of another
/// group, or of none, in between goes into the group that ends after it; what follows the last
/// group's commit goes into the last group). A group without commits is left out.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct SquashParams {
    pub root: String,
    pub base: String,
    /// The trailer's name (`Request`).
    pub trailer: String,
    pub groups: Vec<SquashGroup>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct SquashResult {
    pub head: String,
    /// The new commits, oldest first.
    pub commits: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct AdvanceParams {
    pub root: String,
    pub branch: String,
    pub to: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ShowParams {
    pub root: String,
    pub rev: String,
    pub path: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct GrepParams {
    pub root: String,
    pub rev: String,
    /// A fixed string (not a pattern).
    pub text: String,
    /// A path to search under.
    #[serde(default)]
    pub under: String,
    /// Default 100.
    #[serde(default)]
    pub limit: Option<u32>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct GrepHit {
    pub path: String,
    pub line: u32,
    pub text: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct GrepResult {
    pub hits: Vec<GrepHit>,
    #[serde(default)]
    pub truncated: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct TreeParams {
    pub root: String,
    pub rev: String,
    /// The folder; empty: the top.
    #[serde(default)]
    pub path: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct TreeResult {
    pub entries: Vec<String>,
}
