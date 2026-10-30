//! The updater: every plugin on the latest version, by itself.
//!
//! The latest version of a plugin is its official release from the catalog; when no catalog can be
//! read, it is the package the app itself came with (`frame.plugins.bundled`).
//! - A plugin the user did not change goes to the latest by itself when no run is going: its
//!   data goes into the live folder first (plugins read their data there), then the package is
//!   installed and swapped in (`frame.plugins.install_many`: all or none).
//! - A plugin the user changed waits for their answer (the popup): use the newer version and drop
//!   the changes, or keep theirs until the next version.
//! - A version that cannot start (the frame's check refused it and the previous one keeps running)
//!   is not tried again; the popup and Settings > Plugins say so with the reason (`failed`).
//! - A build of the user's own (`+dev.` version) is left alone; Settings > Plugins shows it.
//! - A newer frame is downloaded and staged; it runs from the next start ("Restart to update").
//! - A reset (`update.reset`, Settings > Plugins) puts every plugin back to the latest at once,
//!   the user's own builds and changes included, after one confirmation.

use crate::fetch;
use cc_protocol::contracts::chatflow::{RunningResult, verbs as chatflow_verbs};
use cc_protocol::contracts::frame::{
    BundledResult, InstallManyParams, InstallManyResult, InstallParams, PackageSwapResult, PluginListResult, UpdateStageParams,
    UpdateStatus as FrameStatus, flows, methods as frame_methods,
};
use cc_protocol::contracts::lifecycle::{ProbeCheck, ProbeReport};
use cc_protocol::contracts::source::{
    ReleaseMergeParams, ReleaseResetParams, ReleaseResetResult, ReleaseStatus, ReleaseStatusParams, UpdateResult, methods as source_methods,
};
use cc_protocol::contracts::update::{
    Applied, DecideParams, DismissParams, Failed, FrameUpdate, Pending, ResetItem, ResetParams, ResetPlan, ResetResult, RoundItem, Status, choices, methods,
    reset_actions, reset_sources, topics,
};
use cc_protocol::release::{self, Asset, Catalog, PluginEntry};
use cc_protocol::{Ctx, RpcError, parse_params, to_value};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

/// The first look after the start (the plugins settle first).
const FIRST_CHECK: Duration = Duration::from_secs(30);
/// While updates wait for runs to end, look this often.
const WAITING_CHECK: Duration = Duration::from_secs(60);
/// Recent outcomes kept for the band.
const RECENT: usize = 20;
/// What the updater remembers between starts.
const SAVED: &str = "updater.json";
/// The frame's row in the round.
const FRAME: &str = "frame";

#[derive(Debug, Clone)]
pub struct Config {
    pub enabled: bool,
    pub catalog_url: String,
    pub every: Duration,
}

impl Config {
    /// `[plugin.updater.config]`: `enabled` (default true), `repo` (default the official one) or
    /// `catalog_url`, `check_every_s` (default 3600).
    pub fn from_value(value: &Value) -> Config {
        let repo = value.get("repo").and_then(Value::as_str).filter(|repo| !repo.is_empty()).unwrap_or(release::DEFAULT_REPO);
        Config {
            enabled: value.get("enabled").and_then(Value::as_bool).unwrap_or(true),
            catalog_url: value
                .get("catalog_url")
                .and_then(Value::as_str)
                .filter(|url| !url.is_empty())
                .map(str::to_owned)
                .unwrap_or_else(|| release::catalog_url(repo)),
            every: Duration::from_secs(value.get("check_every_s").and_then(Value::as_u64).unwrap_or(3600).max(60)),
        }
    }
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
struct Saved {
    /// Plugin -> the release the user chose to stay off ("Keep mine", or a merge that clashed).
    #[serde(default)]
    kept: BTreeMap<String, String>,
    /// Plugin -> the version the user chose to use (or merge), until it is applied.
    #[serde(default)]
    approved: BTreeMap<String, String>,
    /// Plugin -> the version the user chose to use with their changes merged in, not dropped.
    #[serde(default)]
    merged: BTreeMap<String, String>,
    /// Plugin -> the version that could not start (not tried again under the same frame).
    #[serde(default)]
    failed: BTreeMap<String, Failed>,
    /// Plugin -> the frame version that refused its failed version: another frame tries it again.
    #[serde(default)]
    failed_under: BTreeMap<String, String>,
    #[serde(default)]
    recent: Vec<Applied>,
}

/// A plugin version to apply now.
struct Planned {
    id: String,
    name: String,
    version: String,
    package: Package,
    /// Drop the user's changes to the plugin's folder (they chose the newer version).
    drop_changes: bool,
}

pub struct Updater {
    data_dir: PathBuf,
    config: Mutex<Config>,
    status: Mutex<Status>,
    saved: Mutex<Saved>,
    ctx: OnceLock<Ctx>,
    /// The last catalog lookup (or why it failed), so the status line in Settings > Plugins does
    /// not go to the network every time it is drawn.
    catalog: Mutex<Option<(Instant, Result<Catalog, String>)>>,
    /// A check is running; a second request only asks it to look once more.
    checking: AtomicBool,
    again: AtomicBool,
    wake: Notify,
    /// Versions the frame keeps for its restart (a frame update is staged): (plugin, version).
    /// Not downloaded again; the restart installs them, and this process ends with it.
    at_restart: Mutex<BTreeSet<(String, String)>>,
    /// The current check or reset has put plugins in the round (the next one starts a new list).
    round_open: AtomicBool,
}

impl Updater {
    /// The version of the frame this updater runs under ("" before its first connection).
    fn running_frame(&self) -> String {
        self.ctx.get().and_then(|ctx| ctx.frame_version()).unwrap_or_default()
    }
}

fn now_ms() -> u64 {
    cc_protocol::contracts::now_ms()
}

impl Updater {
    pub fn new(data_dir: PathBuf) -> Arc<Updater> {
        let saved: Saved = std::fs::read(data_dir.join(SAVED)).ok().and_then(|bytes| serde_json::from_slice(&bytes).ok()).unwrap_or_default();
        let status = Status { enabled: true, state: "idle".into(), recent: saved.recent.clone(), failed: saved.failed.values().cloned().collect(), ..Status::default() };
        Arc::new(Updater {
            data_dir,
            config: Mutex::new(Config::from_value(&Value::Null)),
            status: Mutex::new(status),
            saved: Mutex::new(saved),
            ctx: OnceLock::new(),
            catalog: Mutex::new(None),
            checking: AtomicBool::new(false),
            again: AtomicBool::new(false),
            wake: Notify::new(),
            at_restart: Mutex::new(BTreeSet::new()),
            round_open: AtomicBool::new(false),
        })
    }

    /// The context and configuration arrive with the frame's hello acknowledgement.
    pub fn attach(&self, ctx: &Ctx) {
        if self.ctx.set(ctx.clone()).is_ok() {
            let config = Config::from_value(&ctx.config());
            self.with_status(|status| status.enabled = config.enabled);
            *self.config.lock().unwrap_or_else(|p| p.into_inner()) = config;
        }
    }

    fn config(&self) -> Config {
        self.config.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }

    pub fn status(&self) -> Status {
        self.status.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }

    fn with_status(&self, change: impl FnOnce(&mut Status)) {
        let status = {
            let mut status = self.status.lock().unwrap_or_else(|p| p.into_inner());
            change(&mut status);
            status.clone()
        };
        if let Some(ctx) = self.ctx.get() {
            ctx.publish(topics::CHANGED, serde_json::to_value(&status).unwrap_or(Value::Null));
        }
    }

    fn state(&self, state: &str, detail: impl Into<String>) {
        let detail = detail.into();
        self.with_status(|status| {
            status.state = state.into();
            status.detail = detail;
        });
    }

    fn with_saved<T>(&self, change: impl FnOnce(&mut Saved) -> T) -> T {
        let (out, snapshot) = {
            let mut saved = self.saved.lock().unwrap_or_else(|p| p.into_inner());
            let out = change(&mut saved);
            (out, saved.clone())
        };
        let path = self.data_dir.join(SAVED);
        let tmp = self.data_dir.join(format!(".{SAVED}.tmp"));
        let written = serde_json::to_vec_pretty(&snapshot)
            .map_err(std::io::Error::other)
            .and_then(|bytes| std::fs::write(&tmp, bytes))
            .and_then(|()| std::fs::rename(&tmp, &path));
        if let Err(error) = written {
            tracing::warn!(%error, "cannot save the updater's state");
        }
        out
    }

    fn record(&self, plugin: &str, version: &str, outcome: &str, detail: impl Into<String>) -> Applied {
        let applied = Applied { plugin: plugin.into(), version: version.into(), ms: now_ms(), outcome: outcome.into(), detail: detail.into() };
        tracing::info!(plugin, version, outcome, detail = %applied.detail, "update");
        let recent = self.with_saved(|saved| {
            saved.recent.insert(0, applied.clone());
            saved.recent.truncate(RECENT);
            saved.recent.clone()
        });
        self.with_status(|status| status.recent = recent);
        let state = match outcome {
            "failed" => "failed",
            "at_restart" => "at_restart",
            _ => "done",
        };
        self.finish(plugin, version, state, if state == "failed" { &applied.detail } else { "" });
        applied
    }

    // ----- the round: every update of a check or a reset, shown together -----

    /// Put plugins in the round as queued. The first of a check or a reset drops the last round's list.
    fn enter_round(&self, rows: impl IntoIterator<Item = (String, String, String)>) {
        let fresh = !self.round_open.swap(true, Ordering::SeqCst);
        self.with_status(|status| {
            if fresh {
                status.round.clear();
            }
            for (plugin, name, version) in rows {
                status.round.retain(|row| row.plugin != plugin);
                status.round.push(RoundItem { plugin, name, version, state: "queued".into(), ..RoundItem::default() });
            }
        });
    }

    /// Where plugins of the round stand; the status line says it in words.
    fn step(&self, plugins: &[&str], state: &str, percent: Option<u64>, detail: String) {
        self.with_status(|status| {
            status.state = state.into();
            status.detail = detail;
            for row in status.round.iter_mut().filter(|row| plugins.contains(&row.plugin.as_str())) {
                row.state = state.into();
                row.percent = percent;
            }
        });
    }

    fn finish(&self, plugin: &str, version: &str, state: &str, error: &str) {
        self.with_status(|status| {
            if let Some(row) = status.round.iter_mut().find(|row| row.plugin == plugin && row.version == version) {
                row.state = state.into();
                row.percent = None;
                row.error = error.into();
                row.ms = now_ms();
            }
        });
    }

    // ----- methods -----

    pub async fn handle(self: &Arc<Self>, method: &str, params: Value) -> Result<Value, RpcError> {
        match method {
            methods::STATUS => to_value(&self.status()),
            methods::CHECK => {
                self.check().await;
                to_value(&self.status())
            }
            methods::DECIDE => {
                let params: DecideParams = parse_params(params)?;
                self.decide(params)?;
                to_value(&self.status())
            }
            methods::RESTART => {
                let ctx = self.ctx.get().ok_or_else(|| RpcError::internal("not connected to the frame yet"))?;
                ctx.call(frame_methods::UPDATE_RESTART, json!({})).await?;
                to_value(&self.status())
            }
            methods::DISMISS => {
                let params: DismissParams = parse_params(params)?;
                let failed = self.with_saved(|saved| {
                    if let Some(failed) = saved.failed.get_mut(&params.plugin) {
                        failed.seen = true;
                    }
                    saved.failed.values().cloned().collect::<Vec<_>>()
                });
                self.with_status(|status| status.failed = failed);
                to_value(&self.status())
            }
            methods::RESET_PLAN => to_value(&self.reset_plan().await?),
            methods::RESET => {
                let params: ResetParams = if params.is_null() { ResetParams::default() } else { parse_params(params)? };
                to_value(&self.reset(&params.plugins).await?)
            }
            other => Err(RpcError::unknown_method(other)),
        }
    }

    // ----- the reset -----

    /// Where a reset takes a plugin's package from. `fresh` looks the catalog up now; otherwise a
    /// lookup younger than the check interval serves (the status line is drawn often).
    async fn reset_sources(&self, fresh: bool) -> Result<(Ctx, Option<reqwest::Client>, Option<Catalog>, Option<String>, BundledResult, &'static str, String), RpcError> {
        let ctx = self.ctx.get().cloned().ok_or_else(|| RpcError::internal("not connected to the frame yet"))?;
        let platform = release::platform().unwrap_or("");
        let config = self.config();
        // The official releases when they can be read; otherwise the app's own packages stand in.
        let (client, catalog, offline) = match fetch::client() {
            Ok(client) => {
                let outcome = match self.recent_catalog(config.every).filter(|_| !fresh) {
                    Some(outcome) => outcome,
                    None => self.lookup_catalog(&client, &config.catalog_url).await,
                };
                match outcome {
                    Ok(catalog) => (Some(client), Some(catalog), None),
                    Err(error) => (None, None, Some(error)),
                }
            }
            Err(error) => (None, None, Some(error)),
        };
        let bundled: BundledResult = call(&ctx, frame_methods::PLUGINS_BUNDLED, json!({})).await.unwrap_or_default();
        if catalog.is_none() && bundled.packages.is_empty() {
            return Err(RpcError::internal(format!(
                "The official releases could not be read ({}) and this app carries no plugin packages of its own; nothing to reset to.",
                offline.clone().unwrap_or_default()
            )));
        }
        let running_frame = ctx.frame_version().unwrap_or_default();
        Ok((ctx, client, catalog, offline, bundled, platform, running_frame))
    }

    /// Every installed plugin against the latest version (an official release, else the app's own
    /// package) with what a reset does to it, and the package: a release asset (downloaded) or the
    /// app's package file.
    async fn reference_items(&self, ctx: &Ctx, catalog: Option<&Catalog>, bundled: &BundledResult, platform: &str, running_frame: &str) -> Result<Vec<(ResetItem, Option<Package>)>, RpcError> {
        let list: PluginListResult = call(ctx, frame_methods::PLUGINS_LIST, json!({})).await?;
        let mut items = Vec::new();
        for unit in &list.units {
            let files = call::<ReleaseStatus>(ctx, source_methods::RELEASE_STATUS, json!(ReleaseStatusParams { plugin: unit.id.clone() }))
                .await
                .map(|status| status.files)
                .unwrap_or_default();
            let mut item = ResetItem {
                plugin: unit.id.clone(),
                name: if unit.name.is_empty() { unit.id.clone() } else { unit.name.clone() },
                installed: unit.version.clone(),
                release: String::new(),
                from: String::new(),
                files,
                action: reset_actions::UNOFFICIAL.into(),
            };
            let official = catalog.and_then(|catalog| catalog.plugins.get(&unit.id)).and_then(|entry| {
                let fits = entry.platforms.is_empty() || entry.platforms.iter().any(|os| os == release::os_name());
                entry.file_for(platform).filter(|_| fits).map(|asset| (entry.clone(), asset.clone()))
            });
            let package = match official {
                Some((entry, asset)) => {
                    item.release = entry.version.clone();
                    item.from = reset_sources::RELEASE.into();
                    if !entry.min_frame.is_empty() && release::is_newer(&entry.min_frame, running_frame) {
                        item.action = reset_actions::FRAME.into();
                        items.push((item, None));
                        continue;
                    }
                    Package::Release(asset)
                }
                None => match bundled.packages.iter().find(|package| package.id == unit.id) {
                    Some(package) => {
                        item.release = package.version.clone();
                        item.from = reset_sources::APP.into();
                        Package::App(PathBuf::from(&package.path))
                    }
                    None => {
                        items.push((item, None));
                        continue;
                    }
                },
            };
            item.action = if !same_version(&unit.version, &item.release) {
                reset_actions::INSTALL
            } else if item.files.is_empty() {
                reset_actions::NONE
            } else {
                reset_actions::SOURCE
            }
            .into();
            items.push((item, Some(package)));
        }
        Ok(items)
    }

    pub async fn reset_plan(&self) -> Result<ResetPlan, RpcError> {
        let (ctx, _, catalog, offline, bundled, platform, running_frame) = self.reset_sources(false).await?;
        let items = self.reference_items(&ctx, catalog.as_ref(), &bundled, platform, &running_frame).await?;
        Ok(ResetPlan { plugins: items.into_iter().map(|(item, _)| item).collect(), running: self.run_going(&ctx).await, offline })
    }

    /// The named plugins (none named: every plugin) back to their official release, or to the
    /// app's own package when the releases cannot be read. Refused while a run is going (the
    /// install would cut it off) or while a check runs.
    pub async fn reset(self: &Arc<Self>, only: &[String]) -> Result<ResetResult, RpcError> {
        let ctx = self.ctx.get().cloned().ok_or_else(|| RpcError::internal("not connected to the frame yet"))?;
        if self.run_going(&ctx).await {
            return Err(RpcError::invalid_params("A conversation is running and the reset would cut it off. Wait for it to end, then reset again."));
        }
        if self.checking.swap(true, Ordering::SeqCst) {
            return Err(RpcError::invalid_params("An update check is running. Try again in a moment."));
        }
        let outcome = self.reset_all(only).await;
        self.with_status(|status| {
            status.state = "idle".into();
            status.detail = String::new();
        });
        self.checking.store(false, Ordering::SeqCst);
        // A check follows: plugins new to this machine and the frame.
        self.wake.notify_one();
        outcome
    }

    async fn reset_all(&self, only: &[String]) -> Result<ResetResult, RpcError> {
        let (ctx, client, catalog, _, bundled, platform, running_frame) = self.reset_sources(true).await?;
        let mut items = self.reference_items(&ctx, catalog.as_ref(), &bundled, platform, &running_frame).await?;
        if !only.is_empty() {
            items.retain(|(item, _)| only.contains(&item.plugin));
        }
        self.round_open.store(false, Ordering::SeqCst);
        self.enter_round(
            items
                .iter()
                .filter(|(item, package)| package.is_some() && item.action != reset_actions::NONE)
                .map(|(item, _)| (item.plugin.clone(), item.name.clone(), item.release.clone())),
        );
        let work = self.data_dir.join("work").join(uuid::Uuid::new_v4().simple().to_string());
        let mut outcomes = Vec::new();
        let mut packages: Vec<(ResetItem, PathBuf)> = Vec::new();
        for (item, package) in items {
            let Some(package) = package else { continue };
            if item.action == reset_actions::NONE {
                continue;
            }
            let (id, version) = (item.plugin.clone(), item.release.clone());
            let package = match package {
                Package::App(path) => path,
                Package::Release(asset) => {
                    let Some(client) = &client else { continue };
                    self.step(&[&id], "downloading", Some(0), format!("Downloading {id} {version}"));
                    match fetch::asset(client, &asset, &work.join("downloads"), |percent| {
                        self.step(&[&id], "downloading", Some(percent), format!("Downloading {id} {version} ({percent}%)"))
                    })
                    .await
                    {
                        Ok(package) => package,
                        Err(error) => {
                            outcomes.push(self.record(&id, &version, "failed", error));
                            continue;
                        }
                    }
                }
            };
            // The data first: the plugin's folder in the live folder back to the package's.
            self.step(&[&id], "installing", None, format!("Resetting the data of {id}"));
            if let Err(error) = self.reset_data(&ctx, &id, &version, &package, &work).await {
                outcomes.push(self.record(&id, &version, "failed", error));
                continue;
            }
            self.with_saved(|saved| {
                saved.kept.remove(&id);
                saved.approved.remove(&id);
                saved.merged.remove(&id);
                saved.failed.remove(&id);
            });
            if item.action == reset_actions::INSTALL {
                packages.push((item, package));
            } else {
                outcomes.push(self.record(&id, &version, "reset", "Your changes are dropped; this package was running already."));
            }
        }
        if !packages.is_empty() {
            let names = packages.iter().map(|(item, _)| format!("{} {}", item.plugin, item.release)).collect::<Vec<_>>().join(", ");
            let ids: Vec<&str> = packages.iter().map(|(item, _)| item.plugin.as_str()).collect();
            self.step(&ids, "installing", None, format!("Installing {names}"));
            // Recorded before the install: when the set holds this plugin, its own process is
            // replaced mid-call and the outcome would otherwise be lost.
            for (item, _) in &packages {
                outcomes.push(self.record(&item.plugin, &item.release, "reset", "Your changes are dropped and the package installed."));
            }
            let together = install_many(&ctx, packages.iter().map(|(_, package)| package.as_path()).collect()).await;
            match together {
                Ok(Placed::Now) => {}
                Ok(Placed::AtRestart) => {
                    for (item, _) in &packages {
                        outcomes.push(self.held_for_restart(&item.plugin, &item.release));
                    }
                }
                Err(error) if packages.len() == 1 => outcomes.push(self.record(&packages[0].0.plugin, &packages[0].0.release, "failed", error)),
                Err(_) => {
                    for (item, package) in &packages {
                        match install_many(&ctx, vec![package.as_path()]).await {
                            Ok(Placed::Now) => {}
                            Ok(Placed::AtRestart) => outcomes.push(self.held_for_restart(&item.plugin, &item.release)),
                            Err(error) => outcomes.push(self.record(&item.plugin, &item.release, "failed", error)),
                        }
                    }
                }
            }
        }
        let _ = tokio::fs::remove_dir_all(&work).await;
        Ok(ResetResult { plugins: outcomes })
    }

    /// The package's data over the plugin's folder in the live folder.
    async fn reset_data(&self, ctx: &Ctx, id: &str, version: &str, package: &Path, work: &Path) -> Result<(), String> {
        let unpacked = work.join("unpacked").join(id);
        fetch::untar(package, &unpacked).await?;
        let params = ReleaseResetParams { plugin: id.to_owned(), dir: unpacked.to_string_lossy().into_owned(), version: version.to_owned() };
        match call::<ReleaseResetResult>(ctx, source_methods::RELEASE_RESET, json!(params)).await {
            Ok(_) => Ok(()),
            // Without the source manager the package still installs (its packaged data serves).
            Err(error) => {
                tracing::warn!(plugin = id, error = %error.message, "the plugin's data was not reset in the live folder");
                Ok(())
            }
        }
    }

    fn decide(self: &Arc<Self>, params: DecideParams) -> Result<(), RpcError> {
        let known = self.status().pending.iter().any(|pending| pending.plugin == params.plugin && pending.version == params.version);
        if !known {
            return Err(RpcError::invalid_params(format!("no release {} of {} waits for an answer", params.version, params.plugin)));
        }
        match params.choice.as_str() {
            choices::KEEP => {
                self.with_saved(|saved| saved.kept.insert(params.plugin.clone(), params.version.clone()));
                self.record(&params.plugin, &params.version, "kept", "Your version stays; the next release asks again.");
            }
            choices::USE => {
                self.with_saved(|saved| {
                    saved.approved.insert(params.plugin.clone(), params.version.clone());
                    saved.merged.remove(&params.plugin);
                });
            }
            choices::MERGE => {
                self.with_saved(|saved| {
                    saved.approved.insert(params.plugin.clone(), params.version.clone());
                    saved.merged.insert(params.plugin.clone(), params.version.clone());
                });
            }
            other => return Err(RpcError::invalid_params(format!("choice `{other}`: give `{}`, `{}` or `{}`", choices::USE, choices::KEEP, choices::MERGE))),
        }
        self.with_status(|status| status.pending.retain(|pending| pending.plugin != params.plugin));
        self.wake.notify_one();
        Ok(())
    }

    // ----- the loop -----

    /// Look at start (after a short settle), then every `check_every_s`, more often while
    /// updates wait for runs to end, and whenever asked.
    pub fn spawn_loop(self: &Arc<Self>, stop: CancellationToken) {
        let me = self.clone();
        tokio::spawn(async move {
            tokio::select! {
                _ = tokio::time::sleep(FIRST_CHECK) => {}
                _ = me.wake.notified() => {}
                _ = stop.cancelled() => return,
            }
            loop {
                me.check().await;
                let wait = if me.status().waiting.is_empty() { me.config().every } else { WAITING_CHECK };
                tokio::select! {
                    _ = tokio::time::sleep(wait) => {}
                    _ = me.wake.notified() => {}
                    _ = stop.cancelled() => return,
                }
            }
        });
    }

    /// One look at the catalog; a request while one runs makes it look once more.
    pub async fn check(self: &Arc<Self>) {
        if self.checking.swap(true, Ordering::SeqCst) {
            self.again.store(true, Ordering::SeqCst);
            return;
        }
        loop {
            self.check_once().await;
            if !self.again.swap(false, Ordering::SeqCst) {
                break;
            }
        }
        self.checking.store(false, Ordering::SeqCst);
    }

    async fn check_once(self: &Arc<Self>) {
        let Some(ctx) = self.ctx.get().cloned() else { return };
        let config = self.config();
        if !config.enabled {
            self.with_status(|status| {
                status.enabled = false;
                status.state = "idle".into();
                status.detail = "Updates are off.".into();
            });
            return;
        }
        self.state("checking", "Looking for updates");
        let outcome = self.check_catalog(&ctx, &config).await;
        let detail = match &outcome {
            Ok(()) if !self.status().waiting.is_empty() => "Updates wait until no run is going.".to_owned(),
            _ => String::new(),
        };
        self.with_status(|status| {
            status.last_check_ms = now_ms();
            status.error = outcome.err();
            status.state = "idle".into();
            status.detail = detail;
        });
    }

    async fn check_catalog(self: &Arc<Self>, ctx: &Ctx, config: &Config) -> Result<(), String> {
        let platform = release::platform().unwrap_or("");
        let client = fetch::client()?;
        self.round_open.store(false, Ordering::SeqCst);
        let catalog = self.lookup_catalog(&client, &config.catalog_url).await;
        let running_frame = ctx.frame_version().unwrap_or_default();
        let mut errors = Vec::new();
        let catalog = match catalog {
            Ok(catalog) => {
                if let Err(error) = self.frame_update(ctx, &client, &catalog, platform, &running_frame).await {
                    errors.push(error);
                }
                Some(catalog)
            }
            Err(error) => {
                errors.push(error);
                None
            }
        };
        let bundled: BundledResult = call(ctx, frame_methods::PLUGINS_BUNDLED, json!({})).await.unwrap_or_default();
        let items = self.reference_items(ctx, catalog.as_ref(), &bundled, platform, &running_frame).await.map_err(|e| e.message)?;
        // A released plugin this machine has not installed yet.
        let known: Vec<String> = items.iter().map(|(item, _)| item.plugin.clone()).collect();
        let new: Vec<(PluginEntry, Asset)> = catalog
            .as_ref()
            .map(|catalog| {
                catalog
                    .plugins
                    .iter()
                    .filter(|(id, entry)| {
                        !known.contains(id)
                            && (entry.platforms.is_empty() || entry.platforms.iter().any(|os| os == release::os_name()))
                            && (entry.min_frame.is_empty() || !release::is_newer(&entry.min_frame, &running_frame))
                    })
                    .filter_map(|(_, entry)| entry.file_for(platform).map(|asset| (entry.clone(), asset.clone())))
                    .collect()
            })
            .unwrap_or_default();
        let (mut auto, pending) = self.plan(items);
        let mut new = new;
        {
            let held = self.at_restart.lock().unwrap_or_else(|p| p.into_inner());
            auto.retain(|plan| !held.contains(&(plan.id.clone(), plan.version.clone())));
            let id_of = |entry: &PluginEntry| catalog.as_ref().and_then(|c| c.plugins.iter().find(|(_, e)| *e == entry).map(|(id, _)| id.clone()));
            new.retain(|(entry, _)| id_of(entry).is_none_or(|id| !held.contains(&(id, entry.version.clone()))));
        }
        self.with_status(|status| status.pending = pending);
        if auto.is_empty() && new.is_empty() {
            self.with_status(|status| status.waiting.clear());
        } else if self.run_going(ctx).await {
            let ids = auto.iter().map(|planned| planned.id.clone()).chain(new.iter().map(|(entry, _)| entry.name.clone())).collect();
            self.with_status(|status| status.waiting = ids);
        } else {
            self.with_status(|status| status.waiting.clear());
            let new_rows = new.iter().filter_map(|(entry, _)| {
                let id = catalog.as_ref()?.plugins.iter().find(|(_, e)| *e == entry)?.0.clone();
                Some((id, entry.name.clone(), entry.version.clone()))
            });
            self.enter_round(auto.iter().map(|plan| (plan.id.clone(), plan.name.clone(), plan.version.clone())).chain(new_rows));
            self.apply(ctx, &client, auto).await;
            self.install_new(ctx, &client, new, &catalog).await;
        }
        if errors.is_empty() { Ok(()) } else { Err(errors.join("; ")) }
    }

    /// Install released plugins this machine does not have yet, one by one.
    async fn install_new(&self, ctx: &Ctx, client: &reqwest::Client, new: Vec<(PluginEntry, Asset)>, catalog: &Option<Catalog>) {
        let Some(catalog) = catalog else { return };
        let work = self.data_dir.join("work").join(uuid::Uuid::new_v4().simple().to_string());
        for (entry, asset) in new {
            let Some(id) = catalog.plugins.iter().find(|(_, e)| **e == entry).map(|(id, _)| id.clone()) else { continue };
            let version = entry.version.clone();
            self.step(&[&id], "downloading", Some(0), format!("Downloading {id} {version}"));
            let package = match fetch::asset(client, &asset, &work.join("downloads"), |percent| {
                self.step(&[&id], "downloading", Some(percent), format!("Downloading {id} {version} ({percent}%)"))
            })
            .await
            {
                Ok(package) => package,
                Err(error) => {
                    self.record(&id, &version, "failed", error);
                    continue;
                }
            };
            self.step(&[&id], "installing", None, format!("Installing {id} {version}"));
            if let Err(error) = self.merge_data(ctx, &id, &version, &package, &work).await {
                self.record(&id, &version, "failed", error);
                continue;
            }
            let params = InstallParams { path: package.to_string_lossy().into_owned() };
            match call::<PackageSwapResult>(ctx, frame_methods::PLUGINS_INSTALL, json!(params)).await {
                Ok(result) if result.ok => drop(self.record(&id, &version, "installed", "")),
                Ok(result) => drop(self.record(&id, &version, "failed", result.error.unwrap_or_default())),
                Err(error) if error.code == RESTART_PENDING => drop(self.held_for_restart(&id, &version)),
                Err(error) => drop(self.record(&id, &version, "failed", error.message)),
            }
        }
        let _ = tokio::fs::remove_dir_all(&work).await;
    }

    /// Download the catalog and remember the outcome.
    async fn lookup_catalog(&self, client: &reqwest::Client, url: &str) -> Result<Catalog, String> {
        let outcome = fetch::catalog(client, url).await;
        *self.catalog.lock().unwrap_or_else(|p| p.into_inner()) = Some((Instant::now(), outcome.clone()));
        outcome
    }

    /// The last catalog lookup when it is younger than the check interval.
    fn recent_catalog(&self, max_age: Duration) -> Option<Result<Catalog, String>> {
        let cached = self.catalog.lock().unwrap_or_else(|p| p.into_inner());
        cached.as_ref().filter(|(at, _)| at.elapsed() < max_age).map(|(_, outcome)| outcome.clone())
    }

    /// Whether a conversation run is going in any flow (an update would cut it off).
    async fn run_going(&self, ctx: &Ctx) -> bool {
        let Ok(list) = call::<PluginListResult>(ctx, frame_methods::PLUGINS_LIST, json!({})).await else { return false };
        for plugin in list.plugins.iter().filter(|plugin| plugin.state == "active") {
            let Some(flow) = flows::flow_of(&plugin.category) else { continue };
            if call::<RunningResult>(ctx, &flows::method(flow, chatflow_verbs::RUNNING), json!({})).await.is_ok_and(|running| !running.sessions.is_empty()) {
                return true;
            }
        }
        false
    }

    /// Split what differs from the latest: what applies now, and what asks the user. A build of
    /// the user's own, a version the user keeps, and one that could not start are left alone.
    fn plan(&self, items: Vec<(ResetItem, Option<Package>)>) -> (Vec<Planned>, Vec<Pending>) {
        // A failure is over when a newer version came, when that version or a newer one runs after
        // all (the frame installs the app's packages at start by itself), or when another frame runs
        // (a newer frame may take what this one refused).
        let frame = self.running_frame();
        let stale: Vec<String> = {
            let saved = self.saved.lock().unwrap_or_else(|p| p.into_inner());
            saved
                .failed
                .iter()
                .filter(|(plugin, failed)| {
                    saved.failed_under.get(*plugin) != Some(&frame)
                        || items
                            .iter()
                            .find(|(item, _)| item.plugin == **plugin)
                            .is_none_or(|(item, _)| {
                                item.release != failed.version
                                    || same_version(&item.installed, &failed.version)
                                    || release::is_newer(&item.installed, &failed.version)
                            })
                })
                .map(|(plugin, _)| plugin.clone())
                .collect()
        };
        if !stale.is_empty() {
            let all = self.with_saved(|saved| {
                saved.failed.retain(|plugin, _| !stale.contains(plugin));
                saved.failed_under.retain(|plugin, _| saved.failed.contains_key(plugin));
                saved.failed.values().cloned().collect::<Vec<_>>()
            });
            self.with_status(|status| status.failed = all);
        }
        // Every plugin runs the latest release: whatever differs from it installs, every check (a
        // failure is tried again at the next one). The user's data changes are merged in.
        let mut auto = Vec::new();
        for (item, package) in items {
            let Some(package) = package else { continue };
            if item.action != reset_actions::INSTALL {
                continue;
            }
            auto.push(Planned { id: item.plugin, name: item.name, version: item.release, package, drop_changes: false });
        }
        (auto, Vec::new())
    }

    /// Get the package, put its data in, install: every planned plugin.
    async fn apply(&self, ctx: &Ctx, client: &reqwest::Client, planned: Vec<Planned>) {
        let work = self.data_dir.join("work").join(uuid::Uuid::new_v4().simple().to_string());
        let mut set: Vec<(Planned, PathBuf)> = Vec::new();
        for plan in planned {
            let (id, version) = (plan.id.clone(), plan.version.clone());
            let package = match &plan.package {
                Package::App(path) => path.clone(),
                Package::Release(asset) => {
                    self.step(&[&id], "downloading", Some(0), format!("Downloading {id} {version}"));
                    match fetch::asset(client, asset, &work.join("downloads"), |percent| {
                        self.step(&[&id], "downloading", Some(percent), format!("Downloading {id} {version} ({percent}%)"))
                    })
                    .await
                    {
                        Ok(package) => package,
                        Err(error) => {
                            self.record(&id, &version, "failed", error);
                            continue;
                        }
                    }
                }
            };
            // The data first: the plugin reads its data from the live folder.
            let data = if plan.drop_changes {
                self.step(&[&id], "installing", None, format!("Dropping your changes to {id}"));
                self.reset_data(ctx, &id, &version, &package, &work).await
            } else {
                self.step(&[&id], "installing", None, format!("Installing {id} {version}"));
                self.merge_data(ctx, &id, &version, &package, &work).await
            };
            // Changes that do not merge give way to the release's data: the plugin updates all the same.
            let data = match data {
                Err(_) if !plan.drop_changes => self.reset_data(ctx, &id, &version, &package, &work).await,
                other => other,
            };
            match data {
                Ok(()) => set.push((plan, package)),
                Err(error) => {
                    self.record(&id, &version, "failed", error);
                }
            }
        }
        if !set.is_empty() {
            let ids: Vec<&str> = set.iter().map(|(plan, _)| plan.id.as_str()).collect();
            self.step(&ids, "installing", None, format!("Installing {}", names(&set)));
            self.install_set(ctx, set).await;
        }
        let _ = tokio::fs::remove_dir_all(&work).await;
    }

    fn installed(&self, plan: &Planned) {
        self.with_saved(|saved| {
            saved.approved.remove(&plan.id);
            saved.merged.remove(&plan.id);
            saved.failed.remove(&plan.id);
        });
        self.record(&plan.id, &plan.version, "installed", "");
        self.with_status(|status| status.failed.retain(|failed| failed.plugin != plan.id));
    }

    /// The frame keeps the package for its restart (a frame update is staged).
    fn held_for_restart(&self, plugin: &str, version: &str) -> Applied {
        self.at_restart.lock().unwrap_or_else(|p| p.into_inner()).insert((plugin.to_owned(), version.to_owned()));
        self.record(plugin, version, "at_restart", "Installs when the app restarts.")
    }

    /// A version the frame refused (its check failed, the previous version keeps running): not
    /// tried again; the popup and Settings > Plugins say why.
    fn could_not_start(&self, plan: &Planned, error: String) {
        let failed = Failed { plugin: plan.id.clone(), name: plan.name.clone(), version: plan.version.clone(), error: error.clone(), ms: now_ms(), seen: false };
        let frame = self.running_frame();
        let all = self.with_saved(|saved| {
            saved.approved.remove(&plan.id);
            saved.failed.insert(plan.id.clone(), failed);
            saved.failed_under.insert(plan.id.clone(), frame);
            saved.failed.values().cloned().collect::<Vec<_>>()
        });
        self.record(&plan.id, &plan.version, "failed", error);
        self.with_status(|status| status.failed = all);
    }

    /// All together; when the set fails, one at a time, so one bad package does not hold back the rest.
    async fn install_set(&self, ctx: &Ctx, set: Vec<(Planned, PathBuf)>) {
        let together = install_many(ctx, set.iter().map(|(_, package)| package.as_path()).collect()).await;
        match together {
            Ok(Placed::Now) => set.iter().for_each(|(plan, _)| self.installed(plan)),
            Ok(Placed::AtRestart) => set.iter().for_each(|(plan, _)| drop(self.held_for_restart(&plan.id, &plan.version))),
            Err(error) if set.len() == 1 => self.could_not_start(&set[0].0, error),
            Err(_) => {
                for (plan, package) in &set {
                    match install_many(ctx, vec![package.as_path()]).await {
                        Ok(Placed::Now) => self.installed(plan),
                        Ok(Placed::AtRestart) => drop(self.held_for_restart(&plan.id, &plan.version)),
                        Err(error) => self.could_not_start(plan, error),
                    }
                }
            }
        }
    }

    /// Merge the package's data into the live folder, keeping the user's edits.
    async fn merge_data(&self, ctx: &Ctx, id: &str, version: &str, package: &Path, work: &Path) -> Result<(), String> {
        let unpacked = work.join("unpacked").join(id);
        fetch::untar(package, &unpacked).await?;
        let params = ReleaseMergeParams { plugin: id.to_owned(), dir: unpacked.to_string_lossy().into_owned(), version: version.to_owned() };
        let merged = match call::<UpdateResult>(ctx, source_methods::RELEASE_MERGE, json!(params)).await {
            Ok(merged) => merged,
            // Without the source manager the package still installs (its packaged data serves).
            Err(error) => {
                tracing::warn!(plugin = id, error = %error.message, "the package's data was not merged into the live folder");
                return Ok(());
            }
        };
        if merged.state == "conflicts" {
            return Err(format!("Your change and the newer version clash in {}; both stay as they were (Settings > History).", merged.conflicts.join(", ")));
        }
        Ok(())
    }

    /// Stage a newer frame for the next start.
    async fn frame_update(&self, ctx: &Ctx, client: &reqwest::Client, catalog: &Catalog, platform: &str, running: &str) -> Result<(), String> {
        let Some(frame) = &catalog.frame else { return Ok(()) };
        if !release::is_newer(&frame.version, running) {
            self.with_status(|status| status.restart = None);
            return Ok(());
        }
        let staged: FrameStatus = call(ctx, frame_methods::UPDATE_STATUS, json!({})).await.map_err(|e| e.message)?;
        let restart = Some(FrameUpdate { running: running.to_owned(), staged: frame.version.clone() });
        if staged.staged.as_deref() == Some(frame.version.as_str()) {
            self.with_status(|status| status.restart = restart);
            return Ok(());
        }
        let files = frame.files.get(platform).ok_or_else(|| format!("the frame {} has no release for this machine", frame.version))?;
        let work = self.data_dir.join("work").join(uuid::Uuid::new_v4().simple().to_string());
        self.enter_round([(FRAME.to_owned(), "CortexCode".to_owned(), frame.version.clone())]);
        let outcome = async {
            let what = format!("the frame {}", frame.version);
            let frame_archive =
                fetch::asset(client, &files.frame, &work, |percent| self.step(&[FRAME], "downloading", Some(percent), format!("Downloading {what} ({percent}%)"))).await?;
            let electron_archive = if staged.electron.contains(&frame.electron) {
                String::new()
            } else {
                let electron = fetch::asset(client, &files.electron, &work, |percent| {
                    self.step(&[FRAME], "downloading", Some(percent), format!("Downloading Electron {} ({percent}%)", frame.electron))
                })
                .await?;
                electron.to_string_lossy().into_owned()
            };
            self.step(&[FRAME], "installing", None, format!("Preparing {what}"));
            let params = UpdateStageParams {
                version: frame.version.clone(),
                frame_archive: frame_archive.to_string_lossy().into_owned(),
                electron_version: frame.electron.clone(),
                electron_archive,
            };
            call::<FrameStatus>(ctx, frame_methods::UPDATE_STAGE, json!(params)).await.map_err(|e| e.message)?;
            Ok::<(), String>(())
        }
        .await;
        let _ = tokio::fs::remove_dir_all(&work).await;
        if let Err(error) = &outcome {
            self.finish(FRAME, &frame.version, "failed", error);
        }
        outcome?;
        self.finish(FRAME, &frame.version, "at_restart", "");
        self.with_status(|status| status.restart = restart);
        Ok(())
    }

    pub fn probe(&self) -> ProbeReport {
        let started = std::time::Instant::now();
        let mut checks = Vec::new();
        let writable = std::fs::create_dir_all(&self.data_dir)
            .and_then(|()| std::fs::write(self.data_dir.join(".probe"), b"ok"))
            .and_then(|()| std::fs::read(self.data_dir.join(".probe")))
            .map(|bytes| bytes == b"ok");
        checks.push(ProbeCheck {
            name: "state".into(),
            ok: matches!(writable, Ok(true)),
            detail: format!("{}", self.data_dir.display()),
            ms: started.elapsed().as_millis() as u64,
        });
        checks.push(ProbeCheck {
            name: "platform".into(),
            ok: true,
            detail: release::platform().unwrap_or("no releases for this machine").into(),
            ms: 0,
        });
        ProbeReport::from_checks(checks)
    }
}

/// Where a reset's package comes from.
enum Package {
    /// An official release: downloaded and checked.
    Release(Asset),
    /// The app's own package file.
    App(PathBuf),
}

fn names(set: &[(Planned, PathBuf)]) -> String {
    set.iter().map(|(plan, _)| format!("{} {}", plan.id, plan.version)).collect::<Vec<_>>().join(", ")
}

/// Whether two versions are the same release: the build tag after `+` is not compared (a plugin
/// without a backend reports `0.1.0+b1a2b3`, the catalog names `0.1.0`).
fn same_version(a: &str, b: &str) -> bool {
    match (release::Version::parse(a), release::Version::parse(b)) {
        (Some(a), Some(b)) => a == b,
        _ => a == b,
    }
}

/// The frame's answer when a frame update is staged: it keeps the packages for the restart.
const RESTART_PENDING: &str = "restart_pending";

/// Where an installed package runs.
enum Placed {
    Now,
    /// The frame keeps it; the restart installs it before any plugin starts.
    AtRestart,
}

async fn install_many(ctx: &Ctx, packages: Vec<&Path>) -> Result<Placed, String> {
    let params = InstallManyParams {
        id: format!("update-{}", uuid::Uuid::new_v4().simple()),
        paths: packages.iter().map(|path| path.to_string_lossy().into_owned()).collect(),
    };
    match call::<InstallManyResult>(ctx, frame_methods::PLUGINS_INSTALL_MANY, json!(params)).await {
        Ok(result) if result.ok => Ok(Placed::Now),
        Ok(result) => Err(result.error.unwrap_or_else(|| "the install was rolled back".into())),
        Err(error) if error.code == RESTART_PENDING => Ok(Placed::AtRestart),
        Err(error) => Err(error.message),
    }
}

async fn call<T: DeserializeOwned>(ctx: &Ctx, method: &str, params: Value) -> Result<T, RpcError> {
    let value = ctx.call(method, params).await?;
    serde_json::from_value(value).map_err(|e| RpcError::internal(format!("{method}: unexpected answer: {e}")))
}
