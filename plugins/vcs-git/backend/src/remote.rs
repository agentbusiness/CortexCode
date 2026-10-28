//! The only code that talks to a remote: `git push` and GitHub through `gh`. Each function runs
//! only when the corresponding method is called; nothing here is triggered automatically.
//! Processes run detached from any terminal (no password prompt can block) and with a deadline.

use crate::git::{Cmd, Git};
use crate::github::Github;
use crate::repo::Repo;
use cc_protocol::RpcError;
use cc_protocol::contracts::vcs::{PrStatus, PushResult};
use serde_json::Value;
use std::path::Path;
use std::time::Duration;

const PUSH_TIMEOUT: Duration = Duration::from_secs(600);
const GH_TIMEOUT: Duration = Duration::from_secs(120);

async fn remotes(git: &Git, repo: &Repo) -> Result<Vec<String>, RpcError> {
    let out = git.text(&repo.top, ["remote"]).await?;
    Ok(out.lines().filter(|l| !l.is_empty()).map(str::to_owned).collect())
}

/// Remote for `branch`: its configured upstream remote, else `origin`, else the only remote.
async fn pick_remote(git: &Git, repo: &Repo, branch: &str) -> Result<Option<String>, RpcError> {
    let key = format!("branch.{branch}.remote");
    if let Some(remote) = git.try_text(&repo.top, ["config", "--get", key.as_str()]).await?
        && !remote.is_empty()
        && remote != "."
    {
        return Ok(Some(remote));
    }
    let all = remotes(git, repo).await?;
    if all.iter().any(|r| r == "origin") {
        return Ok(Some("origin".into()));
    }
    Ok(if all.len() == 1 { all.into_iter().next() } else { None })
}

/// github.com over SSH: pushed over HTTPS when the push uses the Settings sign-in.
const GITHUB_SSH: &[&str] = &["git@github.com:", "ssh://git@github.com/"];

/// A credential helper that answers with the Settings sign-in, read from the push's own environment.
const TOKEN_HELPER: &str =
    "credential.helper=!f() { test \"$1\" = get && printf 'username=x-access-token\\npassword=%s\\n' \"$CORTEX_PUSH_TOKEN\"; }; f";

/// Whether the repository brings its own way to push to `remote`: a remote that is not plain
/// github.com (an SSH alias, another host, a user in the URL), or credential, SSH or URL settings
/// in the repository's own config or in a file included for it (an `includeIf` for its folder).
async fn own_setup(git: &Git, repo: &Repo, remote: &str) -> Result<bool, RpcError> {
    let url = git.try_text(&repo.top, ["remote", "get-url", "--push", remote]).await?.unwrap_or_default();
    let url = url.trim();
    if !(url.starts_with("https://github.com/") || GITHUB_SSH.iter().any(|prefix| url.starts_with(prefix))) {
        return Ok(true);
    }
    const KEYS: &str = r"^(credential\..*|core\.sshcommand|url\..*)$";
    let listed = |includes: &'static str| git.try_text(&repo.top, ["config", includes, "--show-scope", "--get-regexp", KEYS]);
    let with = listed("--includes").await?.unwrap_or_default();
    if with.lines().any(|line| line.starts_with("local\t") || line.starts_with("worktree\t")) {
        return Ok(true);
    }
    let without = listed("--no-includes").await?.unwrap_or_default();
    Ok(with.lines().count() > without.lines().count())
}

/// Push `branch` to `remote`. The repository's own setup pushes as it is; otherwise the GitHub
/// sign-in from Settings pushes, when there is one; otherwise git's defaults.
pub async fn push(git: &Git, github: &Github, repo: &Repo, remote: Option<&str>, branch: Option<&str>) -> Result<PushResult, RpcError> {
    let branch = match branch {
        Some(branch) => branch.to_owned(),
        None => repo
            .branch(git)
            .await?
            .ok_or_else(|| RpcError::invalid_params("HEAD is detached; name the branch to push"))?,
    };
    if branch.starts_with('-') {
        return Err(RpcError::invalid_params(format!("invalid branch `{branch}`")));
    }
    let remote = match remote {
        Some(remote) => remote.to_owned(),
        None => match pick_remote(git, repo, &branch).await? {
            Some(remote) => remote,
            None => {
                return Ok(PushResult {
                    ok: false,
                    available: false,
                    remote: String::new(),
                    branch,
                    detail: "no remote is configured for this repository".into(),
                });
            }
        },
    };
    if remote.starts_with('-') {
        return Err(RpcError::invalid_params(format!("invalid remote `{remote}`")));
    }
    let merge_key = format!("branch.{branch}.merge");
    let has_upstream = git.try_text(&repo.top, ["config", "--get", merge_key.as_str()]).await?.is_some();
    let refspec = format!("refs/heads/{branch}:refs/heads/{branch}");
    let token = if own_setup(git, repo, &remote).await? { None } else { github.clone_token().await };
    let mut cmd = git.cmd(&repo.top);
    if let Some(token) = &token {
        // The empty helper drops every configured one, so nothing else answers or stores the token.
        cmd = cmd.args(["-c", "credential.helper=", "-c", TOKEN_HELPER]).env("CORTEX_PUSH_TOKEN", token);
        for prefix in GITHUB_SSH {
            cmd = cmd.arg("-c").arg(format!("url.https://github.com/.insteadOf={prefix}"));
        }
    }
    let mut cmd = cmd.args(["push", "--porcelain"]);
    if !has_upstream {
        cmd = cmd.arg("--set-upstream");
    }
    let output = cmd.args([remote.as_str(), refspec.as_str()]).detach().timeout(PUSH_TIMEOUT).output().await;
    let output = match output {
        Ok(output) => output,
        Err(error) => {
            return Ok(PushResult { ok: false, available: false, remote, branch, detail: error.message });
        }
    };
    // `--porcelain` prints one `<flag>\t<from>:<to>\t<summary>` line per ref the remote answered
    // for; `!` marks a rejected ref. Without any ref line the remote was never reached.
    let stdout = output.stdout_text();
    let ref_lines: Vec<&str> = stdout.lines().filter(|line| line.len() > 1 && line.as_bytes()[1] == b'\t').collect();
    let rejected = ref_lines.iter().any(|line| line.starts_with('!'));
    let reached = !ref_lines.is_empty();
    let mut detail = ref_lines.iter().filter_map(|line| line.split('\t').nth(2)).collect::<Vec<_>>().join("; ");
    let stderr = output.stderr_text();
    if !output.success() && !stderr.is_empty() {
        if !detail.is_empty() {
            detail.push_str("; ");
        }
        detail.push_str(&stderr);
        if token.is_some() && !reached {
            detail.push_str("; this push used the GitHub sign-in from Settings: connect it again there if it has expired");
        }
    }
    Ok(PushResult { ok: output.success() && reached && !rejected, available: reached, remote, branch, detail })
}

fn unavailable(detail: impl Into<String>) -> PrStatus {
    PrStatus { available: false, number: None, url: None, state: None, ci: None, detail: detail.into() }
}

fn gh_cmd(gh: &Path, cwd: &Path) -> Cmd {
    Cmd::new(gh, cwd).detach().timeout(GH_TIMEOUT)
}

/// Mechanical roll-up of GitHub check states (the enum values GitHub documents for CheckRun
/// conclusions/statuses and commit StatusContext states) into passing | failing | pending | none.
pub fn rollup_ci(checks: &[Value]) -> String {
    if checks.is_empty() {
        return "none".into();
    }
    let mut pending = false;
    for check in checks {
        let field = |name: &str| check.get(name).and_then(Value::as_str).unwrap_or("").to_ascii_uppercase();
        let (status, conclusion, state) = (field("status"), field("conclusion"), field("state"));
        let failed = matches!(conclusion.as_str(), "FAILURE" | "TIMED_OUT" | "CANCELLED" | "ACTION_REQUIRED" | "STARTUP_FAILURE")
            || matches!(state.as_str(), "FAILURE" | "ERROR");
        if failed {
            return "failing".into();
        }
        let running = (!status.is_empty() && status != "COMPLETED") || matches!(state.as_str(), "PENDING" | "EXPECTED");
        pending |= running;
    }
    if pending { "pending".into() } else { "passing".into() }
}

pub async fn pr_status(git: &Git, gh: Option<&Path>, repo: &Repo) -> Result<PrStatus, RpcError> {
    let Some(gh) = gh else {
        return Ok(unavailable("the GitHub CLI `gh` is not installed"));
    };
    let Some(branch) = repo.branch(git).await? else {
        return Ok(PrStatus {
            available: true,
            number: None,
            url: None,
            state: None,
            ci: None,
            detail: "HEAD is detached; pull requests belong to branches".into(),
        });
    };
    let output = gh_cmd(gh, &repo.top)
        .args(["pr", "list", "--head", branch.as_str(), "--state", "all", "--limit", "1"])
        .args(["--json", "number,url,state,statusCheckRollup"])
        .output()
        .await;
    let output = match output {
        Ok(output) => output,
        Err(error) => return Ok(unavailable(error.message)),
    };
    if !output.success() {
        // gh's own explanation: not authenticated, offline, no GitHub remote, ...
        let reason = output.stderr_text();
        return Ok(unavailable(if reason.is_empty() { format!("gh exited with {:?}", output.code) } else { reason }));
    }
    let list: Value = serde_json::from_slice(&output.stdout)
        .map_err(|e| RpcError::internal(format!("unexpected `gh pr list` output: {e}")))?;
    let Some(pr) = list.as_array().and_then(|a| a.first()) else {
        return Ok(PrStatus {
            available: true,
            number: None,
            url: None,
            state: None,
            ci: None,
            detail: format!("no pull request for branch `{branch}`"),
        });
    };
    let checks = pr.get("statusCheckRollup").and_then(Value::as_array).cloned().unwrap_or_default();
    Ok(PrStatus {
        available: true,
        number: pr.get("number").and_then(Value::as_u64),
        url: pr.get("url").and_then(Value::as_str).map(str::to_owned),
        state: pr.get("state").and_then(Value::as_str).map(str::to_ascii_lowercase),
        ci: Some(rollup_ci(&checks)),
        detail: format!("branch `{branch}`"),
    })
}

pub async fn pr_create(
    git: &Git,
    gh: Option<&Path>,
    repo: &Repo,
    title: &str,
    body: &str,
    base: Option<&str>,
    draft: bool,
) -> Result<PrStatus, RpcError> {
    let Some(gh_path) = gh else {
        return Ok(unavailable("the GitHub CLI `gh` is not installed"));
    };
    if title.trim().is_empty() {
        return Err(RpcError::invalid_params("pull request title is empty"));
    }
    let branch = repo
        .branch(git)
        .await?
        .ok_or_else(|| RpcError::invalid_params("HEAD is detached; a pull request needs a branch"))?;
    let mut cmd = gh_cmd(gh_path, &repo.top).args(["pr", "create", "--head", branch.as_str(), "--title", title, "--body", body]);
    if let Some(base) = base {
        cmd = cmd.args(["--base", base]);
    }
    if draft {
        cmd = cmd.arg("--draft");
    }
    let output = match cmd.output().await {
        Ok(output) => output,
        Err(error) => return Ok(unavailable(error.message)),
    };
    if !output.success() {
        let reason = output.stderr_text();
        return Ok(unavailable(if reason.is_empty() { format!("gh exited with {:?}", output.code) } else { reason }));
    }
    pr_status(git, gh, repo).await
}
