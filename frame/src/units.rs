//! Plugins as units: an installed package version holds a backend, UI packages and shell modules,
//! and the frame enables, disables, installs and rolls back all of it together.
//!
//! - The frame runs every installed plugin (`<home>/plugins/<id>/<version>/`) that is not disabled;
//!   `<home>/state/plugins.json` records each plugin's current, previous and last known good
//!   version and the disabled plugins.
//! - install: the package's version becomes the plugin's: the backend is hot-swapped to it
//!   (blue/green or drain-first, with the swap engine's probe and rollback), then its UI packages
//!   and shell modules switch. A plugin new to the frame is added and started. The version it
//!   replaced is kept, and `frame.plugins.rollback {plugin}` swaps both back. A backend that
//!   crashes within the swap engine's watch window takes the UI back with it.
//! - disable: the plugin's UI packages stop being served (`ui_plugin.changed` with `removed`), its
//!   shell modules are unloaded (`plugin.unit`), its backend is unrouted and stopped and stays
//!   stopped; calls to its category answer a retryable `unavailable` error. enable reverses it.
//! - Packages dropped into `<home>/plugins/.incoming/` are installed at startup and whenever one
//!   appears; while the frame runs, the outcome is written beside it as `<file>.result.json` for
//!   the packer that waits for it.

use crate::config::{DEFAULT_HELLO_TIMEOUT_MS, DEFAULT_PROBE_TIMEOUT_MS, PluginConfig, PluginOverride, merge_config};
use crate::home::write_atomic;
use crate::hub::Frame;
use crate::package::{INCOMING, Installed};
use crate::supervisor::{self, ExecSpec};
use cc_protocol::contracts::frame::{PackageSwapResult, PluginToggleResult, PluginUnit, SwapStep, UiPlugin, topics};
use cc_protocol::contracts::{CATEGORIES, FAMILIES};
use cc_protocol::{RpcError, to_value};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Instant;

struct UnitState {
    enabled: bool,
    current: Arc<Installed>,
    previous: Option<Arc<Installed>>,
    /// The version that last served without crashing (crash-loop fallback).
    known_good: Option<String>,
}

pub struct Unit {
    pub id: String,
    state: Mutex<UnitState>,
}

impl Unit {
    fn new(current: Arc<Installed>, enabled: bool) -> Unit {
        Unit { id: current.id().to_owned(), state: Mutex::new(UnitState { enabled, current, previous: None, known_good: None }) }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, UnitState> {
        self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    pub fn enabled(&self) -> bool {
        self.lock().enabled
    }

    pub fn current(&self) -> Arc<Installed> {
        self.lock().current.clone()
    }

    /// The backend's category (it never changes between versions; install checks it).
    pub fn category(&self) -> Option<String> {
        self.current().manifest.backend.as_ref().map(|b| b.category.clone())
    }

    /// The UI bundle directories of `version`, with this plugin as their owner.
    fn ui_of(&self, version: &Installed) -> Vec<(String, PathBuf)> {
        version.manifest.ui.iter().map(|ui| (self.id.clone(), version.dir.join(&ui.dir))).collect()
    }

    fn ui_ids(version: &Installed) -> Vec<String> {
        version.manifest.ui.iter().map(|ui| ui.id.clone()).collect()
    }

    fn shell_of(version: &Installed) -> Vec<(String, PathBuf)> {
        version.manifest.shell.iter().map(|s| (s.id.clone(), version.dir.join(&s.dir))).collect()
    }

    /// Shell module folders to load (none while disabled).
    pub fn shell_dirs(&self) -> Vec<PathBuf> {
        let state = self.lock();
        if !state.enabled {
            return Vec::new();
        }
        Self::shell_of(&state.current).into_iter().map(|(_, dir)| dir).collect()
    }

    pub fn info(&self) -> PluginUnit {
        let state = self.lock();
        let current = &state.current;
        let meta = &current.manifest.plugin;
        PluginUnit {
            id: self.id.clone(),
            name: if meta.name.is_empty() { self.id.clone() } else { meta.name.clone() },
            version: meta.version.clone(),
            enabled: state.enabled,
            dir: current.dir.display().to_string(),
            category: current.manifest.backend.as_ref().map(|b| b.category.clone()),
            ui: Self::ui_ids(current),
            shell: Self::shell_of(current).into_iter().map(|(id, _)| id).collect(),
            shell_dirs: if state.enabled { Self::shell_of(current).into_iter().map(|(_, d)| d.display().to_string()).collect() } else { Vec::new() },
            previous_version: state.previous.as_ref().map(|v| v.version().to_owned()),
        }
    }
}

/// What the supervisor runs for `version`'s backend, with the frame config's overrides.
pub fn exec_spec(version: &Installed, over: &PluginOverride) -> Option<ExecSpec> {
    let packed = version.manifest.backend.as_ref()?;
    Some(ExecSpec {
        exec: version.process_exec()?,
        data_name: version.exec_name()?,
        args: over.args.clone().unwrap_or_else(|| packed.args.clone()),
        config: merge_config(&packed.config, &over.config),
        version: version.version().to_owned(),
        commit: version.manifest.build.git_commit.clone(),
        source_subdir: version.manifest.source.as_ref().map(|source| source.subdir.clone()).filter(|subdir| !subdir.is_empty()),
    })
}

/// The supervisor's slot settings for `version`'s backend.
pub fn backend_config(version: &Installed, over: &PluginOverride) -> Option<PluginConfig> {
    let packed = version.manifest.backend.as_ref()?;
    Some(PluginConfig {
        plugin: version.id().to_owned(),
        category: packed.category.clone(),
        hello_timeout_ms: over.hello_timeout_ms.or(packed.hello_timeout_ms).unwrap_or(DEFAULT_HELLO_TIMEOUT_MS).max(1),
        probe_timeout_ms: over.probe_timeout_ms.or(packed.probe_timeout_ms).unwrap_or(DEFAULT_PROBE_TIMEOUT_MS).max(1),
    })
}

/// Start order: the core categories in contract order (the model first), then the language and
/// skills families, then UI-only plugins.
fn start_rank(installed: &Installed) -> (usize, String) {
    let rank = match installed.manifest.backend.as_ref().map(|b| b.category.as_str()) {
        Some(category) => CATEGORIES.iter().position(|(c, _)| *c == category).unwrap_or_else(|| {
            FAMILIES.iter().position(|(family, _)| category.starts_with(&format!("{family}-"))).map(|i| CATEGORIES.len() + i).unwrap_or(usize::MAX - 1)
        }),
        None => usize::MAX,
    };
    (rank, installed.id().to_owned())
}

/// `<home>/state/plugins.json`.
#[derive(Debug, Default, Serialize, Deserialize)]
struct PluginsFile {
    #[serde(default)]
    disabled: BTreeSet<String>,
    #[serde(default)]
    installed: BTreeMap<String, InstalledRecord>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct InstalledRecord {
    current: String,
    #[serde(default)]
    previous: Option<String>,
    #[serde(default)]
    known_good: Option<String>,
}

pub struct Units {
    list: RwLock<Vec<Arc<Unit>>>,
    file: PathBuf,
    /// `<home>/plugins`.
    pub root: PathBuf,
    overrides: BTreeMap<String, PluginOverride>,
    /// Serializes enable, disable, install and rollback.
    pub op: tokio::sync::Mutex<()>,
}

impl Units {
    /// Build the units from `plugins.json`, then install what waits in `.incoming/` (the frame is
    /// not serving yet, so a newer version simply becomes current).
    pub fn load(root: &Path, state_dir: &Path, overrides: BTreeMap<String, PluginOverride>) -> Result<Units, String> {
        let file = state_dir.join("plugins.json");
        let saved: PluginsFile = match std::fs::read(&file) {
            Ok(bytes) => serde_json::from_slice(&bytes).map_err(|e| format!("{} is corrupt: {e}", file.display()))?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => PluginsFile::default(),
            Err(error) => return Err(format!("cannot read {}: {error}", file.display())),
        };
        let resolve = |id: &str, version: &str| match crate::package::load(root, id, version) {
            Ok(installed) => Some(Arc::new(installed)),
            Err(error) => {
                tracing::warn!(plugin = id, %version, %error, "installed version unavailable");
                None
            }
        };
        let mut list: Vec<Arc<Unit>> = Vec::new();
        for (id, record) in &saved.installed {
            let previous = record.previous.as_deref().and_then(|v| resolve(id, v));
            let Some(current) = resolve(id, &record.current).or_else(|| previous.clone()) else { continue };
            let unit = Unit::new(current.clone(), !saved.disabled.contains(id));
            {
                let mut state = unit.lock();
                state.previous = previous.filter(|p| p.version() != current.version());
                state.known_good = record.known_good.clone();
            }
            list.push(Arc::new(unit));
        }
        let units = Units { list: RwLock::new(list), file, root: root.to_path_buf(), overrides, op: tokio::sync::Mutex::new(()) };
        for archive in units.incoming() {
            let outcome = units.install_offline(&archive, &saved.disabled);
            // Nobody waits for a startup install (a packer waits only while a frame runs): the
            // outcome goes to the log.
            match &outcome {
                Ok(installed) => tracing::info!(plugin = installed.id(), version = installed.version(), "installed a waiting package"),
                Err(error) => tracing::error!(archive = %archive.display(), %error, "a waiting package was not installed"),
            }
            let _ = std::fs::remove_file(&archive);
        }
        units.sort();
        units.persist()?;
        Ok(units)
    }

    /// `.ccplugin` files waiting in `.incoming/` (oldest first).
    pub fn incoming(&self) -> Vec<PathBuf> {
        let dir = self.root.join(INCOMING);
        let _ = std::fs::create_dir_all(&dir);
        let Ok(entries) = std::fs::read_dir(&dir) else { return Vec::new() };
        let mut files: Vec<(std::time::SystemTime, PathBuf)> = entries
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.path())
            .filter(|path| {
                let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                !name.starts_with('.') && name.ends_with(".ccplugin") && path.is_file()
            })
            .map(|path| (std::fs::metadata(&path).and_then(|m| m.modified()).unwrap_or(std::time::UNIX_EPOCH), path))
            .collect();
        files.sort();
        files.into_iter().map(|(_, path)| path).collect()
    }

    /// A plugin disabled before it was installed (recorded in `disabled`) stays disabled.
    fn install_offline(&self, archive: &Path, disabled: &BTreeSet<String>) -> Result<Arc<Installed>, String> {
        let installed = Arc::new(crate::package::install_archive(&self.root, archive, &|id, version| self.in_use(id, version))?);
        match self.get(installed.id()) {
            Some(unit) => {
                check_same_category(&unit, &installed)?;
                let mut state = unit.lock();
                let old = std::mem::replace(&mut state.current, installed.clone());
                if old.version() != installed.version() {
                    state.previous = Some(old);
                }
            }
            None => {
                if let Ok(mut list) = self.list.write() {
                    list.push(Arc::new(Unit::new(installed.clone(), !disabled.contains(installed.id()))));
                }
            }
        }
        self.prune(installed.id());
        Ok(installed)
    }

    fn sort(&self) {
        if let Ok(mut list) = self.list.write() {
            list.sort_by_key(|unit| start_rank(&unit.current()));
        }
    }

    pub fn all(&self) -> Vec<Arc<Unit>> {
        self.list.read().map(|list| list.clone()).unwrap_or_default()
    }

    pub fn get(&self, id: &str) -> Option<Arc<Unit>> {
        self.all().into_iter().find(|unit| unit.id == id)
    }

    /// The plugin whose backend serves `category`.
    pub fn of_category(&self, category: &str) -> Option<Arc<Unit>> {
        self.all().into_iter().find(|unit| unit.category().as_deref() == Some(category))
    }

    pub fn override_of(&self, plugin: &str) -> PluginOverride {
        self.overrides.get(plugin).cloned().unwrap_or_default()
    }

    pub fn disabled(&self) -> BTreeSet<String> {
        self.all().iter().filter(|u| !u.enabled()).map(|u| u.id.clone()).collect()
    }

    /// Slot settings of every plugin with a backend, in start order.
    pub fn backends(&self) -> Vec<PluginConfig> {
        self.all().iter().filter_map(|unit| backend_config(&unit.current(), &self.override_of(&unit.id))).collect()
    }

    /// UI bundle directories of every unit's running version (disabled ones included; the UI
    /// plugin table filters them).
    pub fn ui_sources(&self) -> Vec<(String, PathBuf)> {
        self.all().iter().flat_map(|unit| unit.ui_of(&unit.current())).collect()
    }

    /// The disabled plugin whose shell module serves `category`, if any (its category must not be
    /// served while it is disabled).
    pub fn disabled_server_of(&self, category: &str) -> Option<String> {
        self.all()
            .into_iter()
            .find(|unit| !unit.enabled() && unit.current().manifest.shell.iter().any(|s| s.serves.as_deref() == Some(category)))
            .map(|unit| unit.id.clone())
    }

    /// Shell module folders of every enabled unit.
    pub fn shell_dirs(&self) -> Vec<PathBuf> {
        self.all().iter().flat_map(|unit| unit.shell_dirs()).collect()
    }

    /// What the backend of `plugin` runs now.
    pub fn current_spec(&self, plugin: &str) -> Option<ExecSpec> {
        let unit = self.get(plugin)?;
        exec_spec(&unit.current(), &self.override_of(plugin))
    }

    /// The last known good version of `plugin`'s backend, when it is still installed.
    pub fn known_good_spec(&self, plugin: &str) -> Option<ExecSpec> {
        let unit = self.get(plugin)?;
        let version = unit.lock().known_good.clone()?;
        let installed = crate::package::load(&self.root, plugin, &version).ok()?;
        exec_spec(&installed, &self.override_of(plugin))
    }

    pub fn is_known_good(&self, plugin: &str, version: &str) -> bool {
        self.get(plugin).is_some_and(|unit| unit.lock().known_good.as_deref() == Some(version))
    }

    /// Record `version` as `plugin`'s last known good version. It prunes nothing: a drain_first
    /// swap marks the old version while the new one is not yet current, so a prune here would
    /// delete the version being started. Installs, rollbacks and fallbacks prune when they finish.
    pub fn mark_known_good(&self, plugin: &str, version: &str) {
        let Some(unit) = self.get(plugin) else { return };
        {
            let mut state = unit.lock();
            if state.known_good.as_deref() == Some(version) {
                return;
            }
            state.known_good = Some(version.to_owned());
        }
        match self.persist() {
            Ok(()) => tracing::info!(plugin, version, "recorded last known good version"),
            Err(error) => tracing::error!(plugin, %error, "cannot record the known good version"),
        }
    }

    /// Versions of `id` the frame may run (current, previous, known good): never replaced.
    fn in_use(&self, id: &str, version: &str) -> bool {
        self.get(id).is_some_and(|unit| {
            let state = unit.lock();
            state.current.version() == version
                || state.previous.as_ref().is_some_and(|p| p.version() == version)
                || state.known_good.as_deref() == Some(version)
        })
    }

    /// Remove stored versions of `id` the frame no longer refers to.
    fn prune(&self, id: &str) {
        let Some(unit) = self.get(id) else { return };
        let keep: Vec<String> = {
            let state = unit.lock();
            [Some(state.current.version().to_owned()), state.previous.as_ref().map(|p| p.version().to_owned()), state.known_good.clone()]
                .into_iter()
                .flatten()
                .collect()
        };
        crate::package::prune(&self.root, id, &keep.iter().map(String::as_str).collect::<Vec<_>>());
    }

    fn persist(&self) -> Result<(), String> {
        let mut file = PluginsFile { disabled: self.disabled(), installed: BTreeMap::new() };
        for unit in self.all() {
            let state = unit.lock();
            file.installed.insert(
                unit.id.clone(),
                InstalledRecord {
                    current: state.current.version().to_owned(),
                    previous: state.previous.as_ref().map(|p| p.version().to_owned()),
                    known_good: state.known_good.clone(),
                },
            );
        }
        let bytes = serde_json::to_vec_pretty(&file).map_err(|e| e.to_string())?;
        write_atomic(&self.file, &bytes).map_err(|e| format!("cannot write {}: {e}", self.file.display()))
    }
}

fn check_same_category(unit: &Unit, installed: &Installed) -> Result<(), String> {
    let category = unit.category();
    let packed = installed.manifest.backend.as_ref().map(|b| b.category.clone());
    if category == packed {
        return Ok(());
    }
    let describe = |c: &Option<String>| c.as_deref().map(|c| format!("category `{c}`")).unwrap_or_else(|| "no category".into());
    Err(format!("plugin `{}` serves {}, the package serves {}", unit.id, describe(&category), describe(&packed)))
}

/// Write the outcome beside the archive and remove the archive.
fn finish_incoming(archive: &Path, result: &serde_json::Value) {
    let mut name = archive.file_name().map(|n| n.to_os_string()).unwrap_or_default();
    name.push(".result.json");
    let path = archive.with_file_name(name);
    if let Ok(bytes) = serde_json::to_vec_pretty(result) {
        let _ = write_atomic(&path, &bytes);
    }
    let _ = std::fs::remove_file(archive);
}

fn step(list: &mut Vec<SwapStep>, name: &str, ok: bool, started: Instant, detail: impl Into<String>) {
    list.push(SwapStep { step: name.into(), ok, ms: started.elapsed().as_millis() as u64, detail: detail.into() });
}

fn publish_unit(frame: &Frame, unit: &PluginUnit) {
    if let Ok(value) = to_value(unit) {
        frame.publish(topics::PLUGIN_UNIT, &value);
    }
}

/// The UI packages of `ids` as served now.
fn served(frame: &Frame, ids: &[String]) -> Vec<UiPlugin> {
    frame.ui_plugins.list().into_iter().filter(|p| ids.contains(&p.id)).collect()
}

/// `frame.plugins.disable` / `frame.plugins.enable`.
pub async fn set_enabled(frame: &Arc<Frame>, id: &str, enabled: bool) -> Result<PluginToggleResult, RpcError> {
    let _op = frame.units.op.lock().await;
    let mut steps = Vec::new();
    let unit = frame.units.get(id).ok_or_else(|| RpcError::not_found(format!("no plugin `{id}`")))?;
    let current = unit.current();
    let ui_ids = Unit::ui_ids(&current);
    if unit.enabled() == enabled {
        return Ok(PluginToggleResult { plugin: unit.info(), changed: false, steps, ui: Vec::new() });
    }
    unit.lock().enabled = enabled;
    if let Err(error) = frame.units.persist() {
        unit.lock().enabled = !enabled;
        return Err(RpcError::internal(error));
    }
    let slot = unit.category().and_then(|category| frame.sup.slot(&category));
    if enabled {
        if let Some(slot) = &slot {
            match exec_spec(&current, &frame.units.override_of(id)) {
                Some(spec) => steps.extend(supervisor::enable(frame, slot, spec).await),
                None => steps.push(SwapStep { step: "start".into(), ok: false, ms: 0, detail: "no runnable backend version".into() }),
            }
        }
        let started = Instant::now();
        frame.ui_plugins.set_disabled(id, false);
        let changes = frame.refresh_ui().await;
        step(&mut steps, "serve-ui", true, started, format!("{} UI package(s) served", changes.len()));
    } else {
        let started = Instant::now();
        frame.ui_plugins.set_disabled(id, true);
        let changes = frame.refresh_ui().await;
        step(&mut steps, "unserve-ui", true, started, format!("{} UI package(s) no longer served", changes.len()));
        // Categories its shell modules serve stop being routed now (the shell unloads the modules
        // when it sees the plugin.unit event below).
        for category in current.manifest.shell.iter().filter_map(|s| s.serves.as_deref()) {
            let removed = frame.unroute_ws(category);
            steps.push(SwapStep { step: "unroute".into(), ok: true, ms: 0, detail: format!("`{category}` {}", if removed { "no longer routed" } else { "was not served" }) });
        }
        if let Some(slot) = &slot {
            steps.extend(supervisor::disable(frame, slot).await);
        }
    }
    let info = unit.info();
    publish_unit(frame, &info);
    tracing::info!(plugin = id, enabled, "plugin {}", if enabled { "enabled" } else { "disabled" });
    Ok(PluginToggleResult { plugin: info, changed: true, steps, ui: ui_ids })
}

/// `frame.plugins.install`.
pub async fn install(frame: &Arc<Frame>, path: &str) -> Result<PackageSwapResult, RpcError> {
    let archive = PathBuf::from(path);
    if !archive.is_absolute() {
        return Err(RpcError::invalid_params(format!("`{path}` must be an absolute path")));
    }
    let _op = frame.units.op.lock().await;
    let mut steps = Vec::new();
    let started = Instant::now();
    let installed = {
        let frame = frame.clone();
        tokio::task::spawn_blocking(move || {
            crate::package::install_archive(&frame.units.root, &archive, &|id, version| frame.units.in_use(id, version))
        })
        .await
        .map_err(|e| RpcError::internal(e.to_string()))?
        .map_err(|message| RpcError::invalid_params(format!("the package was not installed: {message}")).with_data(serde_json::json!({"reason": message})))?
    };
    let installed = Arc::new(installed);
    step(&mut steps, "unpack", true, started, format!("{} {} -> {}", installed.id(), installed.version(), installed.dir.display()));
    let id = installed.id().to_owned();
    let (unit, from_version, outcome) = match frame.units.get(&id) {
        Some(unit) => {
            check_same_category(&unit, &installed).map_err(RpcError::invalid_params)?;
            let from_version = unit.current().version().to_owned();
            let outcome = switch(frame, &unit, installed.clone(), "install", &mut steps).await;
            (unit, from_version, outcome)
        }
        None => {
            let unit = add_unit(frame, installed.clone())?;
            let outcome = start_new(frame, &unit, &mut steps).await;
            (unit, String::new(), outcome)
        }
    };
    frame.units.prune(&id);
    finish(frame, &unit, from_version, steps, outcome)
}

/// A plugin new to this frame: add its unit (and its backend's slot).
fn add_unit(frame: &Arc<Frame>, installed: Arc<Installed>) -> Result<Arc<Unit>, RpcError> {
    let backend = backend_config(&installed, &frame.units.override_of(installed.id()));
    if let Some(backend) = &backend
        && let Some(existing) = frame.sup.slot(&backend.category)
    {
        return Err(RpcError::invalid_params(format!("category `{}` is already served by plugin `{}`", backend.category, existing.plugin)));
    }
    let unit = Arc::new(Unit::new(installed, true));
    if let Some(backend) = &backend {
        frame.sup.add_slot(supervisor::Slot::new(backend, false)).map_err(RpcError::invalid_params)?;
    }
    if let Ok(mut list) = frame.units.list.write() {
        list.push(unit.clone());
    }
    frame.units.sort();
    Ok(unit)
}

type Outcome = Result<Option<cc_protocol::contracts::frame::SwapResult>, (String, Option<cc_protocol::contracts::frame::SwapResult>)>;

/// Start a newly added plugin: its backend, then its UI packages and shell modules.
async fn start_new(frame: &Arc<Frame>, unit: &Arc<Unit>, steps: &mut Vec<SwapStep>) -> Outcome {
    let mut backend_swap = None;
    if let Some(category) = unit.category() {
        let slot = frame.sup.slot(&category).ok_or_else(|| (format!("no slot for `{category}`"), None))?;
        let spec = exec_spec(&unit.current(), &frame.units.override_of(&unit.id)).ok_or_else(|| ("the package has no runnable backend".to_owned(), None))?;
        let note = format!("install {} {}", unit.id, spec.version);
        let result = supervisor::swap_for_package(frame, &slot, spec, &note).await;
        steps.push(SwapStep { step: "backend".into(), ok: result.ok, ms: result.steps.iter().map(|s| s.ms).sum(), detail: format!("{category} started{}", result.error.as_ref().map(|e| format!(": {e}")).unwrap_or_default()) });
        if !result.ok {
            let error = result.error.clone().unwrap_or_else(|| "the backend did not start".into());
            return Err((error, Some(result)));
        }
        backend_swap = Some(result);
    }
    let started = Instant::now();
    let changes = frame.refresh_ui().await;
    step(steps, "ui", true, started, format!("{} UI package(s) served", changes.len()));
    Ok(backend_swap)
}

/// Swap `unit` to `to`: the backend first (the swap engine probes it and keeps the old version on
/// failure), then the UI packages and shell modules. Returns the backend swap, or the error.
async fn switch(frame: &Arc<Frame>, unit: &Arc<Unit>, to: Arc<Installed>, why: &str, steps: &mut Vec<SwapStep>) -> Outcome {
    let enabled = unit.enabled();
    let mut backend_swap = None;
    if let Some(category) = unit.category() {
        let slot = frame.sup.slot(&category).ok_or_else(|| (format!("no slot for `{category}`"), None))?;
        let spec = exec_spec(&to, &frame.units.override_of(&unit.id)).ok_or_else(|| ("the version has no runnable backend".to_owned(), None))?;
        if enabled {
            let note = format!("{why} {} {}", unit.id, to.version());
            let result = supervisor::swap_for_package(frame, &slot, spec, &note).await;
            let ok = result.ok;
            steps.push(SwapStep {
                step: "backend".into(),
                ok,
                ms: result.steps.iter().map(|s| s.ms).sum(),
                detail: format!("{category} {} -> {}{}", unit.current().version(), to.version(), result.error.as_ref().map(|e| format!(": {e}")).unwrap_or_default()),
            });
            if !ok {
                let error = result.error.clone().unwrap_or_else(|| "the backend swap failed".into());
                return Err((error, Some(result)));
            }
            backend_swap = Some(result);
        } else {
            steps.push(SwapStep { step: "backend".into(), ok: true, ms: 0, detail: "the plugin is disabled; the new version starts when it is enabled".into() });
        }
    }
    let started = Instant::now();
    {
        let mut state = unit.lock();
        let old = std::mem::replace(&mut state.current, to);
        state.previous = Some(old);
    }
    let changes = frame.refresh_ui().await;
    step(steps, "ui", true, started, format!("{} UI package(s) switched", changes.len()));
    Ok(backend_swap)
}

fn finish(frame: &Arc<Frame>, unit: &Arc<Unit>, from_version: String, mut steps: Vec<SwapStep>, outcome: Outcome) -> Result<PackageSwapResult, RpcError> {
    if let Err(error) = frame.units.persist() {
        steps.push(SwapStep { step: "persist".into(), ok: false, ms: 0, detail: error });
    }
    let info = unit.info();
    publish_unit(frame, &info);
    let ui = served(frame, &info.ui);
    Ok(match outcome {
        Ok(backend) => PackageSwapResult {
            ok: true,
            plugin: info.id.clone(),
            from_version,
            to_version: info.version.clone(),
            backend,
            ui,
            steps,
            error: None,
            rolled_back: false,
            unit: info,
        },
        Err((error, backend)) => PackageSwapResult {
            ok: false,
            plugin: info.id.clone(),
            from_version,
            to_version: String::new(),
            rolled_back: backend.as_ref().is_some_and(|b| b.rolled_back),
            backend,
            ui,
            steps,
            error: Some(error),
            unit: info,
        },
    })
}

/// `frame.plugins.rollback`: back to the version the last install (or rollback) replaced.
pub async fn rollback(frame: &Arc<Frame>, id: &str) -> Result<PackageSwapResult, RpcError> {
    let _op = frame.units.op.lock().await;
    let unit = frame.units.get(id).ok_or_else(|| RpcError::not_found(format!("no plugin `{id}`")))?;
    let previous = unit.lock().previous.clone().ok_or_else(|| RpcError::not_found(format!("plugin `{id}` has no previous version")))?;
    let from_version = unit.current().version().to_owned();
    let mut steps = vec![SwapStep { step: "rollback".into(), ok: true, ms: 0, detail: format!("to {}", previous.version()) }];
    let outcome = switch(frame, &unit, previous, "rollback", &mut steps).await;
    finish(frame, &unit, from_version, steps, outcome)
}

/// The swap engine moved `slot`'s backend to another version on its own (a crash in the watch
/// window rolled it back, or a crash loop fell back to the known good version): the plugin's UI
/// follows, so backend and UI always run the same package version.
pub async fn backend_moved(frame: &Arc<Frame>, plugin: &str, to_version: &str) {
    let _op = frame.units.op.lock().await;
    let Some(unit) = frame.units.get(plugin) else { return };
    {
        let mut state = unit.lock();
        if state.current.version() == to_version {
            return;
        }
        let to = match &state.previous {
            Some(previous) if previous.version() == to_version => previous.clone(),
            _ => match crate::package::load(&frame.units.root, plugin, to_version) {
                Ok(installed) => Arc::new(installed),
                Err(error) => {
                    tracing::error!(plugin, %error, "the backend runs a version whose package is gone");
                    return;
                }
            },
        };
        // The version that crashed is not offered as the one to roll back to.
        state.previous = None;
        state.current = to;
    }
    tracing::warn!(plugin, version = to_version, "the backend fell back to another version; its UI goes with it");
    frame.refresh_ui().await;
    if let Err(error) = frame.units.persist() {
        tracing::error!(%error, "cannot record the fallback");
    }
    frame.units.prune(plugin);
    publish_unit(frame, &unit.info());
}

/// Watch `.incoming/` and install every package that appears (debounced; a packer writes a
/// hidden temp file and renames it into place, so a listed file is complete).
pub fn watch_incoming(frame: &Arc<Frame>) {
    let dir = frame.units.root.join(INCOMING);
    let _ = std::fs::create_dir_all(&dir);
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let watcher = match crate::fswatch::watch(&dir, tx) {
        Ok(watcher) => watcher,
        Err(error) => {
            tracing::error!(dir = %dir.display(), %error, "cannot watch the incoming packages folder");
            return;
        }
    };
    let frame = frame.clone();
    tokio::spawn(async move {
        let _watcher = watcher;
        loop {
            for archive in frame.units.incoming() {
                if frame.is_shutting_down() {
                    return;
                }
                tracing::info!(archive = %archive.display(), "installing a dropped-in package");
                let result = match install(&frame, &archive.display().to_string()).await {
                    Ok(result) => serde_json::to_value(&result).unwrap_or_default(),
                    Err(error) => serde_json::json!({"ok": false, "error": error.message}),
                };
                finish_incoming(&archive, &result);
            }
            if rx.recv().await.is_none() {
                return;
            }
            while let Ok(Some(())) = tokio::time::timeout(std::time::Duration::from_millis(200), rx.recv()).await {}
        }
    });
}
