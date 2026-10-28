//! The live folder over time: created from the installed packages' data at the first start,
//! updated from them at every start and whenever a plugin is installed, rolled back or switched
//! (`plugin.unit`; a merge that keeps the user's edits), and watched while the app runs. A changed
//! file is already live (plugins read it from the live folder); once it has been quiet for a moment
//! it is committed, so History lists it and it can be reverted.
//!
//! The first start after an update from a version with a code folder (CortexCode's full source in
//! `<home>/code`) carries the user's data edits over from it, then removes it.

use crate::packages::{self, Package};
use crate::service::Service;
use crate::{backup, live, paths};
use cc_protocol::Ctx;
use cc_protocol::contracts::frame::PluginUnit;
use cc_protocol::contracts::source::UpdateResult;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

/// How often the work tree is looked at.
const EVERY: Duration = Duration::from_secs(2);
/// A file is committed once it has not changed for this long (an editor may write in steps).
const QUIET: Duration = Duration::from_secs(2);

fn result(state: &str, conflicts: Vec<String>) -> UpdateResult {
    UpdateResult { state: state.into(), conflicts }
}

/// Record the outcome for `source.status` and tell listeners the data may have moved.
fn record(service: &Arc<Service>, ctx: &Ctx, result: &UpdateResult) {
    let state = if result.state == "merged" { "up_to_date".to_owned() } else { result.state.clone() };
    let conflicts = result.conflicts.clone();
    let _ = service.update_settings(move |settings| {
        settings.update = state;
        settings.conflicts = conflicts;
    });
    service.changed(ctx);
}

/// Create or update the live folder from the installed packages' data.
pub async fn update(service: &Arc<Service>, ctx: &Ctx) -> UpdateResult {
    if !service.managed() {
        return result("up_to_date", Vec::new());
    }
    let installed = match packages::installed(ctx).await {
        Ok(installed) => installed,
        Err(error) => {
            tracing::warn!(error = %error.message, "the installed plugins could not be listed");
            return result("no_source", Vec::new());
        }
    };
    service.remember_names(installed.iter().map(|(package, name)| (package.id.clone(), name.clone())));
    let packages: Vec<Package> = installed.into_iter().map(|(package, _)| package).collect();
    let outcome = if live::is_managed(&service.live_dir) {
        service.git(move |repo| live::update(repo, &packages)).await.map(|updated| result(updated.state, updated.conflicts))
    } else {
        service.git_raw(move |dir| live::populate(dir, &packages)).await.map(|()| result("up_to_date", Vec::new()))
    };
    // Tried at every start until it went through (a code folder left after a failure).
    if outcome.is_ok() {
        carry_over(service).await;
    }
    let outcome = outcome.unwrap_or_else(|error| {
        tracing::warn!(error = %error.message, "the live folder could not be updated");
        result("conflicts", vec![error.message])
    });
    record(service, ctx, &outcome);
    outcome
}

/// The user's data edits from the code folder of earlier versions into the new live folder (one
/// commit), then the code folder goes. On any failure it stays, so nothing the user made is lost.
async fn carry_over(service: &Arc<Service>) {
    let old = service.old_code_dir();
    if !live::is_managed(&old) {
        return;
    }
    let from = old.clone();
    let carried = service
        .git(move |repo| {
            let files = live::carry_over(&from, repo)?;
            if !files.is_empty() {
                let plugins = live::plugins_of(&files).join(", ");
                let text = live::message("Carried over your edits from the code folder", &[("Origin", "edit".into()), ("Plugins", plugins)]);
                live::commit_files(repo, &files, &text)?;
            }
            Ok(files)
        })
        .await;
    match carried {
        Ok(files) => {
            tracing::info!(files = files.len(), "the data edits of the code folder were carried over");
            if let Err(error) = std::fs::remove_dir_all(&old) {
                tracing::warn!(%error, dir = %old.display(), "the code folder could not be removed");
            }
        }
        Err(error) => tracing::warn!(error = %error.message, dir = %old.display(), "the code folder's edits could not be carried over; it stays"),
    }
}

/// A plugin was installed, rolled back, enabled or disabled (`plugin.unit`): its folder in the live
/// folder follows the package it now runs from.
pub async fn follow(service: &Arc<Service>, ctx: &Ctx, unit: PluginUnit) {
    if !service.managed() || !live::is_managed(&service.live_dir) {
        return;
    }
    service.remember_names([(unit.id.clone(), if unit.name.is_empty() { unit.id.clone() } else { unit.name.clone() })]);
    let package = match packages::of_unit(&unit) {
        Ok(package) => package,
        Err(error) => {
            tracing::warn!(plugin = %unit.id, %error, "the installed package could not be read");
            return;
        }
    };
    let version = unit.version.clone();
    let outcome = service
        .git(move |repo| live::release(repo, &package, &version))
        .await
        .map(|updated| result(updated.state, updated.conflicts))
        .unwrap_or_else(|error| result("conflicts", vec![error.message]));
    if outcome.state != "up_to_date" {
        record(service, ctx, &outcome);
    }
}

fn modified(service: &Service, file: &str) -> Option<SystemTime> {
    std::fs::symlink_metadata(service.live_dir.join(file)).and_then(|meta| meta.modified()).ok()
}

fn quiet_for(service: &Service, files: &[String], quiet: Duration) -> bool {
    let now = SystemTime::now();
    files.iter().all(|file| modified(service, file).is_none_or(|at| now.duration_since(at).unwrap_or_default() >= quiet))
}

/// Commit the files that changed and have settled, one commit per plugin.
async fn commit_edits(service: &Arc<Service>) -> bool {
    let Ok(changed) = service.git(live::changed_paths).await else { return false };
    if changed.is_empty() || !quiet_for(service, &changed, QUIET) {
        return false;
    }
    let mut by_plugin: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for file in changed {
        let plugin = paths::plugin_of(&file).unwrap_or_default().to_owned();
        by_plugin.entry(plugin).or_default().push(file);
    }
    let mut committed = false;
    for (plugin, files) in by_plugin {
        let shown: Vec<String> = files.iter().filter_map(|f| f.rsplit('/').next().map(str::to_owned)).collect();
        let summary = if plugin.is_empty() { format!("Edited {}", shown.join(", ")) } else { format!("Edited {} in {}", shown.join(", "), service.name_of(&plugin)) };
        let text = live::message(&summary, &[("Origin", "edit".into()), ("Plugins", plugin)]);
        match service.git(move |repo| live::commit_files(repo, &files, &text)).await {
            Ok(_) => committed = true,
            Err(error) => tracing::warn!(error = %error.message, "an edit could not be committed"),
        }
    }
    committed
}

/// Watch the live folder until the plugin stops.
pub async fn run(service: Arc<Service>, ctx: Ctx, stop: tokio_util::sync::CancellationToken) {
    loop {
        tokio::select! {
            _ = stop.cancelled() => return,
            _ = tokio::time::sleep(EVERY) => {}
        }
        if !service.managed() || !live::is_managed(&service.live_dir) {
            continue;
        }
        if commit_edits(&service).await {
            service.changed(&ctx);
            backup::after_change(&service, &ctx).await;
        }
    }
}
