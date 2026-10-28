//! Share improvements: the user's own changes, picked one by one, sent to the official repository
//! as a pull request from the user's fork. Before anything leaves the Mac, each change is checked
//! for the user's private details (paths of their Mac, their projects, anything that looks like a
//! secret) and the findings are shown with the change.

use crate::live;
use crate::service::{Service, rpc};
use cc_protocol::contracts::frame::{Project, methods as frame_methods};
use cc_protocol::contracts::source::{Change, Finding, ShareCandidate, ShareCandidates, ShareParams, Shared, SharedList};
use cc_protocol::contracts::vcs::{Credential, GithubPr, Repo, methods as vcs_methods};
use cc_protocol::{Ctx, RpcError};
use git2::Oid;
use serde_json::json;
use std::path::Path;
use std::sync::Arc;

/// The branch of the official repository changes are applied to.
const UPSTREAM_BRANCH: &str = "main";
/// Marks of credentials (prefixes and headers of well-known token formats).
const SECRET_MARKS: &[&str] = &["ghp_", "gho_", "ghs_", "github_pat_", "sk-", "AKIA", "-----BEGIN", "xoxb-", "xoxp-", "AIza"];
/// The changes the user made (not the packages' data, its updates or restores).
const OWN: &[&str] = &["edit"];

fn shorten(line: &str) -> String {
    let line = line.trim();
    if line.chars().count() > 140 { format!("{}…", line.chars().take(140).collect::<String>()) } else { line.to_owned() }
}

/// What the privacy check looks for: this Mac's home folder and the user's projects (by folder and
/// by name), except the live folder itself.
struct Private {
    home: Option<String>,
    projects: Vec<String>,
}

async fn private(ctx: &Ctx, live_dir: &Path) -> Private {
    let projects: Vec<Project> = match ctx.call(frame_methods::PROJECT_LIST, json!({})).await {
        Ok(value) => value.get("projects").and_then(|p| serde_json::from_value(p.clone()).ok()).unwrap_or_default(),
        Err(_) => Vec::new(),
    };
    let live = live_dir.canonicalize().ok();
    let mut words = Vec::new();
    for project in projects {
        if Path::new(&project.root).canonicalize().ok() == live {
            continue;
        }
        words.push(project.root.clone());
        if project.name.chars().count() >= 4 {
            words.push(project.name.clone());
        }
    }
    Private { home: std::env::var("HOME").ok().filter(|h| h.len() > 1), projects: words }
}

fn findings(diffs: &[cc_protocol::contracts::source::FileDiff], private: &Private) -> Vec<Finding> {
    let mut out = Vec::new();
    for diff in diffs {
        let mut new_line: u32 = 0;
        for line in diff.patch.lines() {
            if let Some(header) = line.strip_prefix("@@") {
                // `@@ -a,b +c,d @@`: the added side starts at c.
                new_line = header.split('+').nth(1).and_then(|rest| rest.split([',', ' ']).next()).and_then(|n| n.parse().ok()).unwrap_or(1);
                new_line = new_line.saturating_sub(1);
                continue;
            }
            if line.starts_with('-') {
                continue;
            }
            new_line += 1;
            let Some(added) = line.strip_prefix('+') else { continue };
            let reason = if private.home.as_deref().is_some_and(|home| added.contains(home)) || added.contains("/Users/") || added.contains("/home/") {
                Some("path")
            } else if private.projects.iter().any(|word| added.contains(word.as_str())) {
                Some("project")
            } else if SECRET_MARKS.iter().any(|mark| added.contains(mark)) {
                Some("secret")
            } else {
                None
            };
            if let Some(reason) = reason {
                out.push(Finding { path: diff.path.clone(), line: new_line, text: shorten(added), reason: reason.into() });
            }
        }
    }
    out
}

/// The user's changes not shared yet (and not reverted), oldest first.
async fn own_changes(service: &Arc<Service>) -> Result<Vec<Change>, RpcError> {
    let shared: Vec<String> = service.settings()?.shared.into_iter().flat_map(|s| s.commits).collect();
    let history = service.git(|repo| live::history(repo, 500)).await?;
    let mut out: Vec<Change> = history
        .into_iter()
        .filter(|c| OWN.contains(&c.origin.as_str()) && c.reverted_by.is_none() && !shared.contains(&c.commit))
        .collect();
    out.reverse();
    Ok(out)
}

pub async fn candidates(service: &Arc<Service>, ctx: &Ctx) -> Result<ShareCandidates, RpcError> {
    let private = private(ctx, &service.live_dir).await;
    let mut changes = Vec::new();
    for change in own_changes(service).await? {
        let oid = Oid::from_str(&change.commit).map_err(|e| rpc(e.message()))?;
        let diffs = service.git(move |repo| live::diff_commit(repo, oid)).await?;
        changes.push(ShareCandidate {
            findings: findings(&diffs, &private),
            commit: change.commit,
            ms: change.ms,
            summary: change.summary,
            plugins: change.plugins,
            files: change.files,
        });
    }
    Ok(ShareCandidates { changes })
}

/// Open a pull request to the official repository with the picked changes.
pub async fn send(service: &Arc<Service>, ctx: &Ctx, params: ShareParams) -> Result<Shared, RpcError> {
    let own = own_changes(service).await?;
    // Keep the history's order (oldest first), whatever order the user ticked them in.
    let picked: Vec<Change> = own.into_iter().filter(|c| params.commits.contains(&c.commit)).collect();
    if picked.is_empty() || picked.len() != params.commits.len() {
        return Err(RpcError::invalid_params("pick changes from the list of changes not shared yet"));
    }
    let credential: Credential = serde_json::from_value(
        ctx.call(vcs_methods::GITHUB_CREDENTIAL, json!({})).await.map_err(|e| RpcError::invalid_params(format!("connect GitHub first (Settings > GitHub): {}", e.message)))?,
    )
    .map_err(|e| rpc(e.to_string()))?;
    let official = service.official_repo();
    let fork: Repo = serde_json::from_value(ctx.call(vcs_methods::GITHUB_FORK, json!({"repo": official})).await?).map_err(|e| rpc(e.to_string()))?;
    let now = cc_protocol::contracts::now_ms();
    let branch = format!("cortexcode-improvements-{now}");
    let sign_off = format!("Shared from CortexCode.\n\nSigned-off-by: {0} <{0}@users.noreply.github.com>", credential.login);
    let commits: Vec<Oid> = picked.iter().filter_map(|c| Oid::from_str(&c.commit).ok()).collect();
    let (upstream_url, fork_url, local_ref) = (format!("https://github.com/{official}.git"), fork.clone_url.clone(), format!("refs/cortexcode/share/{branch}"));
    let (login, token, push_branch) = (credential.login.clone(), credential.token.clone(), branch.clone());
    service
        .git(move |repo| {
            live::fetch(repo, &upstream_url, Some((&login, &token)), &format!("+refs/heads/{UPSTREAM_BRANCH}:{}", live::UPSTREAM))
                .map_err(|e| format!("cannot read the official repository: {e}"))?;
            let onto = repo.find_reference(live::UPSTREAM).and_then(|r| r.peel_to_commit()).map_err(|e| e.message().to_owned())?.id();
            let tip = live::replay(repo, onto, &commits, &sign_off)?;
            repo.reference(&local_ref, tip, true, "share").map_err(|e| e.message().to_owned())?;
            live::push(repo, &fork_url, &login, &token, &format!("{local_ref}:refs/heads/{push_branch}"))
        })
        .await?;
    let title = match picked.as_slice() {
        [one] => one.summary.clone(),
        many => format!("Improvements from CortexCode ({} changes)", many.len()),
    };
    let mut body = String::from("Changes made in CortexCode and shared by its user:\n\n");
    for change in &picked {
        body.push_str(&format!("- {} ({})\n", change.summary, change.plugins.join(", ")));
    }
    body.push_str(&format!("\nSigned-off-by: {0} <{0}@users.noreply.github.com>\n", credential.login));
    let pr: GithubPr = serde_json::from_value(
        ctx.call(
            vcs_methods::GITHUB_PR_CREATE,
            json!({"repo": official, "head": format!("{}:{branch}", credential.login), "base": UPSTREAM_BRANCH, "title": title, "body": body}),
        )
        .await?,
    )
    .map_err(|e| rpc(e.to_string()))?;
    let shared = Shared { url: pr.url, number: pr.number, branch, commits: picked.iter().map(|c| c.commit.clone()).collect(), state: pr.state, ms: now };
    let record = shared.clone();
    service.update_settings(move |settings| settings.shared.push(record))?;
    service.changed(ctx);
    Ok(shared)
}

/// The shared pull requests, their state refreshed from GitHub when it answers.
pub async fn status(service: &Arc<Service>, ctx: &Ctx) -> Result<SharedList, RpcError> {
    let official = service.official_repo();
    let mut shared = service.settings()?.shared;
    for item in shared.iter_mut().filter(|s| s.state == "open") {
        if let Ok(value) = ctx.call(vcs_methods::GITHUB_PR_STATE, json!({"repo": official, "number": item.number})).await
            && let Ok(pr) = serde_json::from_value::<GithubPr>(value)
        {
            item.state = pr.state;
        }
    }
    let updated = shared.clone();
    service.update_settings(move |settings| settings.shared = updated)?;
    shared.reverse();
    Ok(SharedList { shared })
}
