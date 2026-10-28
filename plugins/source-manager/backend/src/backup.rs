//! Backup & Sync: the live folder's branch pushed to a repository the user picked on GitHub (branch
//! `cortexcode`), and restored from it on a new Mac. The GitHub connection (vcs plugin) supplies the
//! credentials; nothing is stored here but the repository's name.

use crate::live;
use crate::service::{Service, rpc};
use cc_protocol::contracts::source::{Backup, BackupSetParams, RestoreParams, RestoreResult};
use cc_protocol::contracts::vcs::{Credential, methods as vcs_methods};
use cc_protocol::{Ctx, RpcError};
use serde_json::json;
use std::sync::Arc;

/// The branch the live folder is pushed to.
pub const BRANCH: &str = "cortexcode";

pub fn status(service: &Service) -> Result<Backup, RpcError> {
    let settings = service.settings()?;
    Ok(Backup {
        repo: settings.backup_repo,
        auto: settings.backup_auto,
        last_push_ms: settings.last_push_ms,
        last_error: settings.last_push_error,
        branch: BRANCH.into(),
    })
}

pub fn set(service: &Service, params: BackupSetParams) -> Result<Backup, RpcError> {
    if let Some(repo) = params.repo.as_deref().map(str::trim).filter(|r| !r.is_empty())
        && repo.split('/').filter(|part| !part.is_empty()).count() != 2
    {
        return Err(RpcError::invalid_params(format!("`{repo}` is not a repository name (owner/name)")));
    }
    service.update_settings(|settings| {
        if let Some(repo) = params.repo {
            let repo = repo.trim().to_owned();
            settings.backup_repo = (!repo.is_empty()).then_some(repo);
        }
        if let Some(auto) = params.auto {
            settings.backup_auto = auto;
        }
    })?;
    status(service)
}

async fn credential(ctx: &Ctx) -> Result<Credential, RpcError> {
    let value = ctx
        .call(vcs_methods::GITHUB_CREDENTIAL, json!({}))
        .await
        .map_err(|e| RpcError::invalid_params(format!("connect GitHub first (Settings > GitHub): {}", e.message)))?;
    serde_json::from_value(value).map_err(|e| rpc(e.to_string()))
}

fn url(repo: &str) -> String {
    format!("https://github.com/{repo}.git")
}

/// Push the branch to the backup repository.
pub async fn push(service: &Arc<Service>, ctx: &Ctx) -> Result<Backup, RpcError> {
    let repo = service.settings()?.backup_repo.ok_or_else(|| RpcError::invalid_params("choose a backup repository first"))?;
    let outcome = match credential(ctx).await {
        Ok(credential) => {
            let refspec = format!("+{}:refs/heads/{BRANCH}", live::MAIN);
            let target = url(&repo);
            service.git(move |git| live::push(git, &target, &credential.login, &credential.token, &refspec)).await
        }
        Err(error) => Err(error),
    };
    let now = cc_protocol::contracts::now_ms();
    service.update_settings(|settings| match &outcome {
        Ok(()) => {
            settings.last_push_ms = Some(now);
            settings.last_push_error = None;
        }
        Err(error) => settings.last_push_error = Some(error.message.clone()),
    })?;
    service.changed(ctx);
    outcome?;
    status(service)
}

/// After a change: push when "back up after each change" is on (failures are recorded, not raised).
pub async fn after_change(service: &Arc<Service>, ctx: &Ctx) {
    if let Ok(settings) = service.settings()
        && settings.backup_auto
        && settings.backup_repo.is_some()
    {
        let _ = push(service, ctx).await;
    }
}

/// Replace the live folder's changes with the backup's, carried onto this Mac's packages' data.
pub async fn restore(service: &Arc<Service>, ctx: &Ctx, params: RestoreParams) -> Result<RestoreResult, RpcError> {
    let repo = match params.repo.filter(|r| !r.trim().is_empty()) {
        Some(repo) => repo,
        None => service.settings()?.backup_repo.ok_or_else(|| RpcError::invalid_params("choose a backup repository first"))?,
    };
    let credential = credential(ctx).await.ok();
    let source = url(&repo);
    let refspec = format!("+refs/heads/{BRANCH}:{}", live::BACKUP);
    let commit = service
        .git(move |git| {
            live::fetch(git, &source, credential.as_ref().map(|c| (c.login.as_str(), c.token.as_str())), &refspec)
                .map_err(|e| format!("cannot read the backup from {repo}: {e}"))?;
            let backup = git.find_reference(live::BACKUP).and_then(|r| r.peel_to_commit()).map_err(|e| e.message().to_owned())?;
            live::restore(git, backup.id())
        })
        .await?;
    service.changed(ctx);
    Ok(RestoreResult { commit: commit.to_string() })
}
