//! The code folder over time: created from the shipped source at the first start, updated from it
//! at every start (a merge that keeps the user's changes), and watched while the app runs.
//!
//! - A changed data file (skills, guides, prompts, texts) is already live (plugins read it from the
//!   code folder); once it has been quiet for a moment it is committed, so History lists it and
//!   it can be reverted.
//! - Changed code is not live: once the files have been quiet for a while and no conversation in
//!   the code folder is running, the change becomes a proposal with its card.

use crate::service::Service;
use crate::{backup, code, paths, proposals};
use cc_protocol::Ctx;
use cc_protocol::contracts::source::{ProposeParams, UpdateResult};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

/// How often the work tree is looked at.
const EVERY: Duration = Duration::from_secs(2);
/// A data file is committed once it has not changed for this long (an editor may write in steps).
const DATA_QUIET: Duration = Duration::from_secs(2);
/// Changed code is proposed once its files have not changed for this long.
const CODE_QUIET: Duration = Duration::from_secs(60);
/// After a proposal could not be made, wait this long before trying again.
const RETRY: Duration = Duration::from_secs(300);

/// Create or update the code folder from the shipped source.
pub async fn update(service: &Arc<Service>, ctx: &Ctx) -> UpdateResult {
    let no_source = || UpdateResult { state: "no_source".into(), conflicts: Vec::new(), rebuilt: Vec::new() };
    if !service.managed() {
        return UpdateResult { state: "up_to_date".into(), conflicts: Vec::new(), rebuilt: Vec::new() };
    }
    let Some(source) = service.shipped_source() else { return no_source() };
    // The result, and the code files where the folder differs from the new shipped source.
    let outcome = if code::is_managed(&service.code_dir) {
        service
            .git(move |repo| code::update(repo, &source))
            .await
            .map(|updated| (UpdateResult { state: updated.state.into(), conflicts: updated.conflicts, rebuilt: Vec::new() }, updated.own_code))
    } else {
        service
            .git_raw(move |dir| code::populate(dir, &source))
            .await
            .map(|()| (UpdateResult { state: "up_to_date".into(), conflicts: Vec::new(), rebuilt: Vec::new() }, Vec::new()))
    };
    let (mut result, own) = match outcome {
        Ok(outcome) => outcome,
        Err(error) => {
            tracing::warn!(error = %error.message, "the code folder could not be updated");
            (UpdateResult { state: "conflicts".into(), conflicts: vec![error.message], rebuilt: Vec::new() }, Vec::new())
        }
    };
    // The user's own code on a new base: build it again.
    if result.state == "merged" && !own.is_empty() {
        let (plugins, _) = paths::affected(&service.code_dir, &own);
        result.rebuilt = proposals::rebuild(service, ctx, &plugins).await.0;
    }
    let (state, conflicts) = (if result.state == "merged" { "up_to_date".to_owned() } else { result.state.clone() }, result.conflicts.clone());
    let _ = service.update_settings(move |settings| {
        settings.update = state;
        settings.conflicts = conflicts;
    });
    service.changed(ctx);
    result
}

fn modified(service: &Service, file: &str) -> Option<SystemTime> {
    std::fs::symlink_metadata(service.code_dir.join(file)).and_then(|meta| meta.modified()).ok()
}

fn quiet_for(service: &Service, files: &[String], quiet: Duration) -> bool {
    let now = SystemTime::now();
    files.iter().all(|file| modified(service, file).is_none_or(|at| now.duration_since(at).unwrap_or_default() >= quiet))
}

/// Commit the data files that changed and have settled, one commit per plugin.
async fn commit_data(service: &Arc<Service>) -> bool {
    let Ok(changed) = service.git(code::changed_paths).await else { return false };
    let data: Vec<String> = changed.into_iter().filter(|file| paths::is_data(file)).collect();
    if data.is_empty() || !quiet_for(service, &data, DATA_QUIET) {
        return false;
    }
    let names = paths::plugin_names(&service.code_dir);
    let mut by_plugin: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for file in data {
        let plugin = paths::plugin_of(&file).unwrap_or_default().to_owned();
        by_plugin.entry(plugin).or_default().push(file);
    }
    let mut committed = false;
    for (plugin, files) in by_plugin {
        let shown: Vec<String> = files.iter().filter_map(|f| f.rsplit('/').next().map(str::to_owned)).collect();
        let name = names.get(&plugin).filter(|n| !n.is_empty()).cloned().unwrap_or_else(|| plugin.clone());
        let summary = format!("Edited {} in {name}", shown.join(", "));
        let text = code::message(&summary, &[("Origin", "edit".into()), ("Plugins", plugin)]);
        match service.git(move |repo| code::commit_files(repo, &files, &text)).await {
            Ok(_) => committed = true,
            Err(error) => tracing::warn!(error = %error.message, "a data edit could not be committed"),
        }
    }
    committed
}

/// Watch the code folder until the plugin stops.
pub async fn run(service: Arc<Service>, ctx: Ctx, stop: tokio_util::sync::CancellationToken) {
    let mut retry_after: Option<std::time::Instant> = None;
    loop {
        tokio::select! {
            _ = stop.cancelled() => return,
            _ = tokio::time::sleep(EVERY) => {}
        }
        if !service.managed() || !code::is_managed(&service.code_dir) {
            continue;
        }
        if commit_data(&service).await {
            service.changed(&ctx);
            backup::after_change(&service, &ctx).await;
        }
        if retry_after.is_some_and(|at| at > std::time::Instant::now()) {
            continue;
        }
        let Ok(code) = proposals::unproposed(&service).await else { continue };
        if code.is_empty() || !quiet_for(&service, &code, CODE_QUIET) || proposals::code_session_running(&ctx, &service.code_dir).await {
            continue;
        }
        retry_after = match proposals::propose(&service, &ctx, ProposeParams::default()).await {
            Ok(_) => None,
            Err(error) => {
                tracing::warn!(error = %error.message, "a code change could not be proposed");
                Some(std::time::Instant::now() + RETRY)
            }
        };
    }
}
