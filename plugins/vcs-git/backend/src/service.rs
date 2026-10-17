//! Method dispatch for `vcs@1`.

use crate::checkpoint::Shadow;
use crate::git::{Git, find_executable};
use crate::github::Github;
use crate::repo::{Repo, require};
use crate::watch::{Publish, Watches};
use crate::{clone, ops, remote, status};
use cc_protocol::contracts::vcs::{
    BranchParams, CheckpointParams, CloneParams, CommitParams, DevFolderParams, DiffParams, ForkParams, GithubPrParams,
    GithubPrStateParams, LogParams, MergeParams, PathsParams, PrCreateParams, PushParams, RepoCreateParams, ReposParams,
    RestoreParams, RootParams, TokenParams, methods,
};
use cc_protocol::{RpcError, parse_params, to_value};
use serde_json::Value;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// The git binary itself rather than a launcher in front of it: macOS's `/usr/bin/git` is an
/// `xcrun` shim that adds several milliseconds to every invocation. `git --exec-path` names the
/// directory holding the real executable; use it when it is there, else the binary as found.
fn resolve_git(found: PathBuf) -> PathBuf {
    let output = std::process::Command::new(&found).arg("--exec-path").stdin(std::process::Stdio::null()).output();
    if let Ok(output) = output
        && output.status.success()
    {
        let dir = PathBuf::from(String::from_utf8_lossy(&output.stdout).trim());
        let candidate = dir.join("git");
        if find_executable(&candidate.to_string_lossy()).is_some() {
            return candidate;
        }
    }
    found
}

/// Plugin configuration (`[plugin.config]`): optional `git` and `gh` executables, and the GitHub
/// app's client id for the device sign-in.
#[derive(Debug, Clone, Default)]
pub struct Config {
    pub git: Option<String>,
    pub gh: Option<String>,
    pub github_client_id: Option<String>,
}

impl Config {
    pub fn from_value(value: &Value) -> Self {
        let get = |key: &str| value.get(key).and_then(Value::as_str).map(str::to_owned);
        Self { git: get("git"), gh: get("gh"), github_client_id: get("github_client_id") }
    }
}

pub struct Vcs {
    pub git: Git,
    gh: Mutex<Option<PathBuf>>,
    data_dir: PathBuf,
    scratch: PathBuf,
    /// One async mutex per working tree: mutating operations on a repository never interleave.
    locks: Mutex<HashMap<PathBuf, Arc<tokio::sync::Mutex<()>>>>,
    pub watches: Watches,
    pub github: Github,
}

impl Vcs {
    pub fn new(data_dir: PathBuf, config: &Config, publish: Publish) -> Result<Self, RpcError> {
        let git_exe = find_executable(config.git.as_deref().unwrap_or("git"))
            .ok_or_else(|| RpcError::new(cc_protocol::codes::UNAVAILABLE, "git is not installed"))?;
        let git = Git::new(resolve_git(git_exe));
        // Scratch index paths are handed to git through the environment: keep them absolute.
        let data_dir = if data_dir.is_absolute() {
            data_dir
        } else {
            std::env::current_dir().map(|cwd| cwd.join(&data_dir)).unwrap_or(data_dir)
        };
        let data_dir = std::fs::canonicalize(&data_dir).unwrap_or(data_dir);
        let gh = find_executable(config.gh.as_deref().unwrap_or("gh"));
        let scratch = data_dir.join("tmp");
        let github = Github::new(&data_dir)?;
        github.set_client_id(config.github_client_id.as_deref());
        Ok(Self {
            watches: Watches::new(git.clone(), publish, data_dir.clone()),
            git,
            gh: Mutex::new(gh),
            data_dir,
            scratch,
            locks: Mutex::new(HashMap::new()),
            github,
        })
    }

    /// Re-read configuration delivered after construction (the frame's hello acknowledgement).
    pub fn configure(&self, config: &Config) {
        if let Some(gh) = &config.gh
            && let Ok(mut slot) = self.gh.lock()
        {
            *slot = find_executable(gh);
        }
        self.github.set_client_id(config.github_client_id.as_deref());
    }

    fn gh(&self) -> Option<PathBuf> {
        self.gh.lock().ok().and_then(|g| g.clone())
    }

    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    fn repo_lock(&self, repo: &Repo) -> Arc<tokio::sync::Mutex<()>> {
        let mut locks = match self.locks.lock() {
            Ok(locks) => locks,
            Err(poisoned) => poisoned.into_inner(),
        };
        locks.entry(repo.top.clone()).or_default().clone()
    }

    async fn repo(&self, root: &str) -> Result<Repo, RpcError> {
        let repo = require(&self.git, root).await?;
        self.watches.ensure(&repo, root);
        Ok(repo)
    }

    pub async fn dispatch(&self, method: &str, params: Value) -> Result<Value, RpcError> {
        let git = &self.git;
        match method {
            methods::STATUS => {
                let p: RootParams = parse_params(params)?;
                let (result, repo) = status::status_with_repo(git, &self.scratch, &p.root).await?;
                if let Some(repo) = repo {
                    self.watches.ensure(&repo, &p.root);
                }
                to_value(&result)
            }
            methods::DIFF => {
                let p: DiffParams = parse_params(params)?;
                let repo = self.repo(&p.root).await?;
                to_value(&ops::diff(git, &repo, &self.scratch, &p.scope, p.path.as_deref(), p.base.as_deref()).await?)
            }
            methods::LOG => {
                let p: LogParams = parse_params(params)?;
                let repo = self.repo(&p.root).await?;
                to_value(&ops::log(git, &repo, p.limit).await?)
            }
            methods::BRANCHES => {
                let p: RootParams = parse_params(params)?;
                let repo = self.repo(&p.root).await?;
                to_value(&ops::branches(git, &repo).await?)
            }
            methods::BRANCH_CREATE => {
                let p: BranchParams = parse_params(params)?;
                let repo = self.repo(&p.root).await?;
                let lock = self.repo_lock(&repo);
                let _guard = lock.lock().await;
                to_value(&ops::branch_create(git, &repo, &p.name, p.from.as_deref()).await?)
            }
            methods::CHECKOUT => {
                let p: BranchParams = parse_params(params)?;
                let repo = self.repo(&p.root).await?;
                let lock = self.repo_lock(&repo);
                let _guard = lock.lock().await;
                to_value(&ops::checkout(git, &repo, &p.name, p.from.as_deref()).await?)
            }
            methods::STAGE | methods::UNSTAGE | methods::REVERT => {
                let p: PathsParams = parse_params(params)?;
                let repo = self.repo(&p.root).await?;
                let lock = self.repo_lock(&repo);
                let _guard = lock.lock().await;
                match method {
                    methods::STAGE => ops::stage(git, &repo, &self.scratch, &p.paths, p.hunk).await?,
                    methods::UNSTAGE => ops::unstage(git, &repo, &self.scratch, &p.paths, p.hunk).await?,
                    _ => ops::revert(git, &repo, &self.scratch, &p.paths, p.hunk, p.include_staged).await?,
                }
                to_value(&status::status(git, &self.scratch, &p.root).await?)
            }
            methods::COMMIT => {
                let p: CommitParams = parse_params(params)?;
                let repo = self.repo(&p.root).await?;
                let lock = self.repo_lock(&repo);
                let _guard = lock.lock().await;
                to_value(&ops::commit(git, &repo, &p.message, p.all).await?)
            }
            methods::CHECKPOINT => {
                let p: CheckpointParams = parse_params(params)?;
                to_value(&Shadow::new(git, &self.data_dir, &p.root)?.checkpoint(&p.label).await?)
            }
            methods::CHECKPOINTS => {
                let p: RootParams = parse_params(params)?;
                to_value(&Shadow::new(git, &self.data_dir, &p.root)?.list().await?)
            }
            methods::RESTORE => {
                let p: RestoreParams = parse_params(params)?;
                to_value(&Shadow::new(git, &self.data_dir, &p.root)?.restore(&p.id).await?)
            }
            methods::MERGE => {
                let p: MergeParams = parse_params(params)?;
                let repo = self.repo(&p.root).await?;
                let lock = self.repo_lock(&repo);
                let _guard = lock.lock().await;
                if p.keep_local_changes {
                    to_value(&ops::merge_keeping_local(git, &repo, &p.branch, p.message.as_deref(), p.no_ff).await?)
                } else {
                    to_value(&ops::merge(git, &repo, &p.branch, p.message.as_deref(), p.abort_on_conflict).await?)
                }
            }
            methods::MERGE_ABORT => {
                let p: RootParams = parse_params(params)?;
                let repo = self.repo(&p.root).await?;
                let lock = self.repo_lock(&repo);
                let _guard = lock.lock().await;
                to_value(&ops::merge_abort(git, &repo).await?)
            }
            methods::PUSH => {
                let p: PushParams = parse_params(params)?;
                let repo = self.repo(&p.root).await?;
                to_value(&remote::push(git, &repo, p.remote.as_deref(), p.branch.as_deref()).await?)
            }
            methods::PR_STATUS => {
                let p: RootParams = parse_params(params)?;
                let repo = self.repo(&p.root).await?;
                to_value(&remote::pr_status(git, self.gh().as_deref(), &repo).await?)
            }
            methods::PR_CREATE => {
                let p: PrCreateParams = parse_params(params)?;
                let repo = self.repo(&p.root).await?;
                to_value(
                    &remote::pr_create(git, self.gh().as_deref(), &repo, &p.title, &p.body, p.base.as_deref(), p.draft)
                        .await?,
                )
            }
            methods::GITHUB_STATUS => to_value(&self.github.status()),
            methods::GITHUB_DEVICE_START => to_value(&self.github.device_start().await?),
            methods::GITHUB_DEVICE_POLL => to_value(&self.github.device_poll().await?),
            methods::GITHUB_TOKEN_SET => {
                let p: TokenParams = parse_params(params)?;
                to_value(&self.github.token_set(&p.token).await?)
            }
            methods::GITHUB_DISCONNECT => to_value(&self.github.disconnect().await?),
            methods::GITHUB_REPOS => {
                let p: ReposParams = parse_params(params)?;
                to_value(&self.github.repos(p.query.as_deref()).await?)
            }
            methods::GITHUB_REPO_CREATE => {
                let p: RepoCreateParams = parse_params(params)?;
                to_value(&self.github.repo_create(&p.name, p.private, &p.description).await?)
            }
            methods::GITHUB_FORK => {
                let p: ForkParams = parse_params(params)?;
                to_value(&self.github.fork(&p.repo).await?)
            }
            methods::GITHUB_PR_CREATE => {
                let p: GithubPrParams = parse_params(params)?;
                to_value(&self.github.pr_create(&p.repo, &p.head, &p.base, &p.title, &p.body).await?)
            }
            methods::GITHUB_PR_STATE => {
                let p: GithubPrStateParams = parse_params(params)?;
                to_value(&self.github.pr_state(&p.repo, p.number).await?)
            }
            methods::GITHUB_CREDENTIAL => to_value(&self.github.credential().await?),
            methods::CLONE => {
                let p: CloneParams = parse_params(params)?;
                to_value(&clone::clone(&self.github, &p.repo, p.parent.as_deref()).await?)
            }
            methods::DEV_FOLDER_SET => {
                let p: DevFolderParams = parse_params(params)?;
                to_value(&self.github.dev_folder_set(&p.path)?)
            }
            other => Err(RpcError::unknown_method(other)),
        }
    }
}

/// Every method this plugin serves.
pub const PROVIDES: &[&str] = &[
    methods::STATUS,
    methods::DIFF,
    methods::LOG,
    methods::BRANCHES,
    methods::BRANCH_CREATE,
    methods::CHECKOUT,
    methods::STAGE,
    methods::UNSTAGE,
    methods::REVERT,
    methods::COMMIT,
    methods::CHECKPOINT,
    methods::CHECKPOINTS,
    methods::RESTORE,
    methods::MERGE,
    methods::MERGE_ABORT,
    methods::PUSH,
    methods::PR_STATUS,
    methods::PR_CREATE,
    methods::GITHUB_STATUS,
    methods::GITHUB_DEVICE_START,
    methods::GITHUB_DEVICE_POLL,
    methods::GITHUB_TOKEN_SET,
    methods::GITHUB_DISCONNECT,
    methods::GITHUB_REPOS,
    methods::GITHUB_REPO_CREATE,
    methods::GITHUB_FORK,
    methods::GITHUB_PR_CREATE,
    methods::GITHUB_PR_STATE,
    methods::GITHUB_CREDENTIAL,
    methods::CLONE,
    methods::DEV_FOLDER_SET,
];
