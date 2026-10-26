//! The updater: what the catalog offers against what runs here, and applying it.
//!
//! - A plugin release the user did not change installs by itself when no run is going: its source
//!   goes into the code folder first (plugins read their live data there), then the package is
//!   installed and swapped in (`frame.plugins.install_many`: all or none).
//! - A plugin the user changed waits for their answer (the popup): merge the release into their
//!   version (built here when they changed its code), or keep theirs until the next release.
//! - A newer frame is downloaded and staged; it runs from the next start ("Restart to update").

use crate::fetch;
use cc_protocol::contracts::chatflow::{RunningResult, methods as chatflow_methods};
use cc_protocol::contracts::frame::{
    InstallManyParams, InstallManyResult, InstallParams, PackageSwapResult, PluginListResult, UpdateStageParams, UpdateStatus as FrameStatus,
    methods as frame_methods,
};
use cc_protocol::contracts::lifecycle::{ProbeCheck, ProbeReport};
use cc_protocol::contracts::source::{ReleaseMergeParams, ReleaseStatus, ReleaseStatusParams, UpdateResult, methods as source_methods};
use cc_protocol::contracts::update::{Applied, DecideParams, FrameUpdate, Pending, Status, choices, methods, topics};
use cc_protocol::release::{self, Asset, Catalog, PluginEntry};
use cc_protocol::{Ctx, RpcError, parse_params, to_value};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;
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
    /// Plugin -> the release the user chose to merge, until it is applied.
    #[serde(default)]
    approved: BTreeMap<String, String>,
    #[serde(default)]
    recent: Vec<Applied>,
}

/// A plugin release to apply now.
struct Planned {
    id: String,
    entry: PluginEntry,
    asset: Asset,
    installed: Option<String>,
}

pub struct Updater {
    data_dir: PathBuf,
    config: Mutex<Config>,
    status: Mutex<Status>,
    saved: Mutex<Saved>,
    ctx: OnceLock<Ctx>,
    /// A check is running; a second request only asks it to look once more.
    checking: AtomicBool,
    again: AtomicBool,
    wake: Notify,
}

fn now_ms() -> u64 {
    cc_protocol::contracts::now_ms()
}

impl Updater {
    pub fn new(data_dir: PathBuf) -> Arc<Updater> {
        let saved: Saved = std::fs::read(data_dir.join(SAVED)).ok().and_then(|bytes| serde_json::from_slice(&bytes).ok()).unwrap_or_default();
        let status = Status { enabled: true, state: "idle".into(), recent: saved.recent.clone(), ..Status::default() };
        Arc::new(Updater {
            data_dir,
            config: Mutex::new(Config::from_value(&Value::Null)),
            status: Mutex::new(status),
            saved: Mutex::new(saved),
            ctx: OnceLock::new(),
            checking: AtomicBool::new(false),
            again: AtomicBool::new(false),
            wake: Notify::new(),
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

    fn record(&self, plugin: &str, version: &str, outcome: &str, detail: impl Into<String>) {
        let applied = Applied { plugin: plugin.into(), version: version.into(), ms: now_ms(), outcome: outcome.into(), detail: detail.into() };
        tracing::info!(plugin, version, outcome, detail = %applied.detail, "update");
        let recent = self.with_saved(|saved| {
            saved.recent.insert(0, applied);
            saved.recent.truncate(RECENT);
            saved.recent.clone()
        });
        self.with_status(|status| status.recent = recent);
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
            other => Err(RpcError::unknown_method(other)),
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
            choices::MERGE => {
                self.with_saved(|saved| saved.approved.insert(params.plugin.clone(), params.version.clone()));
            }
            other => return Err(RpcError::invalid_params(format!("choice `{other}`: give `{}` or `{}`", choices::MERGE, choices::KEEP))),
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
        let platform = release::platform().ok_or("CortexCode releases are not built for this machine")?;
        let client = fetch::client()?;
        let catalog = fetch::catalog(&client, &config.catalog_url).await?;
        let running_frame = ctx.frame_version().unwrap_or_default();
        let mut errors = Vec::new();
        if let Err(error) = self.frame_update(ctx, &client, &catalog, platform, &running_frame).await {
            errors.push(error);
        }
        let (auto, pending) = self.plan(ctx, &catalog, platform, &running_frame).await?;
        self.with_status(|status| status.pending = pending);
        if auto.is_empty() {
            self.with_status(|status| status.waiting.clear());
        } else if self.run_going(ctx).await {
            let ids = auto.iter().map(|planned| planned.id.clone()).collect();
            self.with_status(|status| status.waiting = ids);
        } else {
            self.with_status(|status| status.waiting.clear());
            self.apply(ctx, &client, auto).await;
        }
        if errors.is_empty() { Ok(()) } else { Err(errors.join("; ")) }
    }

    /// Whether a conversation run is going (an update would cut it off).
    async fn run_going(&self, ctx: &Ctx) -> bool {
        match call::<RunningResult>(ctx, chatflow_methods::RUNNING, json!({})).await {
            Ok(running) => !running.sessions.is_empty(),
            Err(_) => false,
        }
    }

    /// Split the catalog's newer plugin releases: what applies now, and what asks the user.
    async fn plan(&self, ctx: &Ctx, catalog: &Catalog, platform: &str, running_frame: &str) -> Result<(Vec<Planned>, Vec<Pending>), String> {
        let list: PluginListResult = call(ctx, frame_methods::PLUGINS_LIST, json!({})).await.map_err(|e| e.message)?;
        let saved = self.saved.lock().unwrap_or_else(|p| p.into_inner()).clone();
        let (mut auto, mut pending) = (Vec::new(), Vec::new());
        for (id, entry) in &catalog.plugins {
            if !entry.platforms.is_empty() && !entry.platforms.iter().any(|os| os == release::os_name()) {
                continue;
            }
            let Some(asset) = entry.file_for(platform) else { continue };
            let installed = list.units.iter().find(|unit| unit.id == *id).map(|unit| unit.version.clone());
            if installed.as_deref().is_some_and(|installed| !release::is_newer(&entry.version, installed)) {
                continue;
            }
            // Built with a newer frame: it waits for the frame update.
            if !entry.min_frame.is_empty() && release::is_newer(&entry.min_frame, running_frame) {
                continue;
            }
            if saved.kept.get(id) == Some(&entry.version) {
                continue;
            }
            let approved = saved.approved.get(id) == Some(&entry.version);
            if installed.is_some() && !approved {
                let edited = call::<ReleaseStatus>(ctx, source_methods::RELEASE_STATUS, json!(ReleaseStatusParams { plugin: id.clone() })).await;
                if let Ok(status) = edited
                    && status.edited
                {
                    pending.push(Pending {
                        plugin: id.clone(),
                        name: entry.name.clone(),
                        version: entry.version.clone(),
                        installed: installed.clone().unwrap_or_default(),
                        files: status.files,
                    });
                    continue;
                }
            }
            auto.push(Planned { id: id.clone(), entry: entry.clone(), asset: asset.clone(), installed });
        }
        Ok((auto, pending))
    }

    /// Download, merge the source, install: every planned plugin.
    async fn apply(&self, ctx: &Ctx, client: &reqwest::Client, planned: Vec<Planned>) {
        let work = self.data_dir.join("work").join(uuid::Uuid::new_v4().simple().to_string());
        let mut existing: Vec<(Planned, PathBuf)> = Vec::new();
        let mut new: Vec<(Planned, PathBuf)> = Vec::new();
        for plan in planned {
            let (id, version) = (plan.id.clone(), plan.entry.version.clone());
            self.state("downloading", format!("Downloading {id} {version}"));
            let package = match fetch::asset(client, &plan.asset, &work.join("downloads"), |percent| {
                self.state("downloading", format!("Downloading {id} {version} ({percent}%)"))
            })
            .await
            {
                Ok(package) => package,
                Err(error) => {
                    self.record(&id, &version, "failed", error);
                    continue;
                }
            };
            // The source first: the plugin reads its live data from the code folder.
            match self.merge_source(ctx, &plan, &package, &work).await {
                Ok(Merged::Install) => {}
                Ok(Merged::Built) => {
                    self.with_saved(|saved| saved.approved.remove(&id));
                    self.record(&id, &version, "built", "Your change and the release are merged and built here.");
                    continue;
                }
                Err(error) => {
                    // A clash asks no more for this release; the user sees why.
                    self.with_saved(|saved| {
                        saved.approved.remove(&id);
                        saved.kept.insert(id.clone(), version.clone());
                    });
                    self.record(&id, &version, "failed", error);
                    continue;
                }
            }
            if plan.installed.is_some() { existing.push((plan, package)) } else { new.push((plan, package)) }
        }
        if !existing.is_empty() {
            self.state("installing", format!("Installing {}", names(&existing)));
            self.install_existing(ctx, existing).await;
        }
        for (plan, package) in new {
            self.state("installing", format!("Installing {} {}", plan.id, plan.entry.version));
            let params = InstallParams { path: package.to_string_lossy().into_owned() };
            match call::<PackageSwapResult>(ctx, frame_methods::PLUGINS_INSTALL, json!(params)).await {
                Ok(result) if result.ok => self.installed(&plan),
                Ok(result) => self.record(&plan.id, &plan.entry.version, "failed", result.error.unwrap_or_default()),
                Err(error) => self.record(&plan.id, &plan.entry.version, "failed", error.message),
            }
        }
        let _ = tokio::fs::remove_dir_all(&work).await;
    }

    fn installed(&self, plan: &Planned) {
        self.with_saved(|saved| saved.approved.remove(&plan.id));
        self.record(&plan.id, &plan.entry.version, "installed", "");
    }

    /// All together; when the set fails, one at a time, so one bad package does not hold back the rest.
    async fn install_existing(&self, ctx: &Ctx, set: Vec<(Planned, PathBuf)>) {
        let together = install_many(ctx, set.iter().map(|(_, package)| package.as_path()).collect()).await;
        match together {
            Ok(()) => set.iter().for_each(|(plan, _)| self.installed(plan)),
            Err(error) if set.len() == 1 => self.record(&set[0].0.id, &set[0].0.entry.version, "failed", error),
            Err(_) => {
                for (plan, package) in &set {
                    match install_many(ctx, vec![package.as_path()]).await {
                        Ok(()) => self.installed(plan),
                        Err(error) => self.record(&plan.id, &plan.entry.version, "failed", error),
                    }
                }
            }
        }
    }

    /// Merge the package's source into the code folder.
    async fn merge_source(&self, ctx: &Ctx, plan: &Planned, package: &Path, work: &Path) -> Result<Merged, String> {
        let unpacked = work.join("unpacked").join(&plan.id);
        fetch::untar(package, &unpacked).await?;
        let source = unpacked.join("source");
        if !source.is_dir() {
            return Ok(Merged::Install);
        }
        let params = ReleaseMergeParams {
            scope: "plugin".into(),
            plugin: plan.id.clone(),
            dir: source.to_string_lossy().into_owned(),
            version: plan.entry.version.clone(),
        };
        let merged = match call::<UpdateResult>(ctx, source_methods::RELEASE_MERGE, json!(params)).await {
            Ok(merged) => merged,
            // Without the source manager the package still installs (its packaged data serves).
            Err(error) => {
                tracing::warn!(plugin = %plan.id, error = %error.message, "the release's source was not merged into the code folder");
                return Ok(Merged::Install);
            }
        };
        if merged.state == "conflicts" {
            return Err(format!(
                "Your change and the release clash in {}; both stay as they were (Settings > History).",
                merged.conflicts.join(", ")
            ));
        }
        Ok(if merged.rebuilt.contains(&plan.id) { Merged::Built } else { Merged::Install })
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
        let outcome = async {
            let what = format!("the frame {}", frame.version);
            let frame_archive = fetch::asset(client, &files.frame, &work, |percent| self.state("downloading", format!("Downloading {what} ({percent}%)"))).await?;
            let electron_archive = if staged.electron.contains(&frame.electron) {
                String::new()
            } else {
                let electron = fetch::asset(client, &files.electron, &work, |percent| {
                    self.state("downloading", format!("Downloading Electron {} ({percent}%)", frame.electron))
                })
                .await?;
                electron.to_string_lossy().into_owned()
            };
            self.state("installing", format!("Preparing {what}"));
            let params = UpdateStageParams {
                version: frame.version.clone(),
                frame_archive: frame_archive.to_string_lossy().into_owned(),
                electron_version: frame.electron.clone(),
                electron_archive,
            };
            call::<FrameStatus>(ctx, frame_methods::UPDATE_STAGE, json!(params)).await.map_err(|e| e.message)?;
            // The frame's source into the code folder (everything outside plugins/).
            let unpacked = work.join("frame");
            fetch::untar(&frame_archive, &unpacked).await?;
            let source = unpacked.join("source");
            if source.is_dir() {
                let params = ReleaseMergeParams {
                    scope: "frame".into(),
                    plugin: String::new(),
                    dir: source.to_string_lossy().into_owned(),
                    version: frame.version.clone(),
                };
                if let Err(error) = call::<UpdateResult>(ctx, source_methods::RELEASE_MERGE, json!(params)).await {
                    tracing::warn!(error = %error.message, "the frame's source was not merged into the code folder");
                }
            }
            Ok::<(), String>(())
        }
        .await;
        let _ = tokio::fs::remove_dir_all(&work).await;
        outcome?;
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

enum Merged {
    /// Install the release package.
    Install,
    /// The user's change was merged and built here (installed already).
    Built,
}

fn names(set: &[(Planned, PathBuf)]) -> String {
    set.iter().map(|(plan, _)| format!("{} {}", plan.id, plan.entry.version)).collect::<Vec<_>>().join(", ")
}

async fn install_many(ctx: &Ctx, packages: Vec<&Path>) -> Result<(), String> {
    let params = InstallManyParams {
        id: format!("update-{}", uuid::Uuid::new_v4().simple()),
        paths: packages.iter().map(|path| path.to_string_lossy().into_owned()).collect(),
    };
    match call::<InstallManyResult>(ctx, frame_methods::PLUGINS_INSTALL_MANY, json!(params)).await {
        Ok(result) if result.ok => Ok(()),
        Ok(result) => Err(result.error.unwrap_or_else(|| "the install was rolled back".into())),
        Err(error) => Err(error.message),
    }
}

async fn call<T: DeserializeOwned>(ctx: &Ctx, method: &str, params: Value) -> Result<T, RpcError> {
    let value = ctx.call(method, params).await?;
    serde_json::from_value(value).map_err(|e| RpcError::internal(format!("{method}: unexpected answer: {e}")))
}
