//! Plugins as units: an installed package version holds a backend, UI packages and shell modules,
//! and the frame enables, disables, installs and rolls back all of it together.
//!
//! - The frame runs every installed plugin (`<home>/plugins/<id>/<version>/`) that is not disabled.
//!   Each plugin's folder records its install state in plain files the frame reads before any
//!   plugin runs: `.current`, `.previous` and `.known_good` hold a version, `.disabled` marks a
//!   disabled plugin, `.chosen` the person's choice for its category. Not in the main store: the
//!   frame needs them to start the memory plugin that opens it, and emptying the store (a clean
//!   test slate) must not forget what is installed. A `state/plugins.json` from before is read once
//!   into these files and removed.
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
//! - Alternatives: several plugins may serve one category (two model engines). One serves it;
//!   the others are on standby (installed, not started, their UI packages not served). The one
//!   that serves is the person's choice (`frame.plugins.switch`, kept as the chosen plugin's
//!   `.chosen` marker), else the one whose declared `[backend.machine]` range holds this machine's
//!   memory, else the first enabled one. A switch is a swap of the category's slot to the other
//!   plugin, with the swap engine's drain, probe and restore.

use crate::config::{DEFAULT_HELLO_TIMEOUT_MS, DEFAULT_PROBE_TIMEOUT_MS, PluginConfig, PluginOverride, merge_config};
use crate::home::write_atomic;
use crate::hub::Frame;
use crate::package::{INCOMING, Installed};
use crate::supervisor::{self, ExecSpec};
use cc_protocol::contracts::frame::{
    InstallManyParams, InstallManyResult, PackageSwapResult, PluginToggleResult, PluginUnit, ProviderInfo, ProvidersResult, SwapStep, SwitchResult, UiPlugin, topics,
};
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
            standby: false,
        }
    }
}

/// What the supervisor runs for `version`'s backend, with the frame config's overrides.
pub fn exec_spec(version: &Installed, over: &PluginOverride) -> Option<ExecSpec> {
    let packed = version.manifest.backend.as_ref()?;
    Some(ExecSpec {
        plugin: version.id().to_owned(),
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

/// `<home>/state/plugins.json`, the record before the per-plugin files (read once, then removed).
#[derive(Debug, Default, Serialize, Deserialize)]
struct PluginsFile {
    #[serde(default)]
    disabled: BTreeSet<String>,
    #[serde(default)]
    installed: BTreeMap<String, InstalledRecord>,
    /// The plugin the person chose for a category that has alternatives (`frame.plugins.switch`).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    providers: BTreeMap<String, String>,
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
    /// `<home>/plugins`.
    pub root: PathBuf,
    overrides: BTreeMap<String, PluginOverride>,
    batch_dir: PathBuf,
    /// Serializes enable, disable, install, rollback and switch.
    pub op: tokio::sync::Mutex<()>,
    /// The person's choice per category with alternatives (persisted).
    providers: Mutex<BTreeMap<String, String>>,
    /// The plugin serving each category now, and why (`user`, `machine`, `only`, `first`).
    serving: Mutex<BTreeMap<String, (String, &'static str)>>,
    /// This machine's physical memory (`hw.memsize`), for the declared machine ranges.
    memory_bytes: u64,
}

impl Units {
    /// Build the units from `plugins.json`, then install what waits in `.incoming/` (the frame is
    /// not serving yet, so a newer version simply becomes current).
    pub fn load(root: &Path, state_dir: &Path, overrides: BTreeMap<String, PluginOverride>, memory_bytes: u64) -> Result<Units, String> {
        let legacy = state_dir.join("plugins.json");
        let mut saved: PluginsFile = match std::fs::read(&legacy) {
            Ok(bytes) => serde_json::from_slice(&bytes).map_err(|e| format!("{} is corrupt: {e}", legacy.display()))?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => read_records(root),
            Err(error) => return Err(format!("cannot read {}: {error}", legacy.display())),
        };
        let batch_dir = state_dir.join("plugin-installs");
        recover_batches(root, &batch_dir, &mut saved)?;
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
        let units = Units {
            list: RwLock::new(list),
            root: root.to_path_buf(),
            overrides,
            batch_dir,
            op: tokio::sync::Mutex::new(()),
            providers: Mutex::new(saved.providers.clone()),
            serving: Mutex::new(BTreeMap::new()),
            memory_bytes,
        };
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
        units.choose_serving();
        units.persist()?;
        // Read into the per-plugin files: the old record goes.
        if legacy.exists()
            && let Err(error) = std::fs::remove_file(&legacy)
        {
            tracing::warn!(%error, "cannot remove the old plugins.json");
        }
        Ok(units)
    }

    /// Decide, at startup, which plugin serves each category.
    fn choose_serving(&self) {
        let categories: BTreeSet<String> = self.all().iter().filter_map(|unit| unit.category()).collect();
        let mut serving = BTreeMap::new();
        for category in categories {
            if let Some((unit, why)) = self.pick(&category) {
                if why != "only" {
                    tracing::info!(%category, plugin = %unit.id, chosen_by = why, memory_gb = self.memory_bytes / (1 << 30), "category with alternatives");
                }
                serving.insert(category, (unit.id.clone(), why));
            }
        }
        if let Ok(mut current) = self.serving.lock() {
            *current = serving;
        }
    }

    /// The plugin that should serve `category`: the person's choice, else the one whose declared
    /// machine range holds this machine, else the first enabled one.
    fn pick(&self, category: &str) -> Option<(Arc<Unit>, &'static str)> {
        let units = self.units_of(category);
        if units.len() <= 1 {
            return units.into_iter().next().map(|unit| (unit, "only"));
        }
        let chosen = self.providers.lock().ok().and_then(|providers| providers.get(category).cloned());
        if let Some(unit) = chosen.and_then(|id| units.iter().find(|unit| unit.id == id).cloned()) {
            return Some((unit, "user"));
        }
        if let Some(unit) = units.iter().find(|unit| unit.enabled() && self.fits_machine(unit) == Some(true)) {
            return Some((unit.clone(), "machine"));
        }
        units.iter().find(|unit| unit.enabled()).or(units.first()).cloned().map(|unit| (unit, "first"))
    }

    /// Whether this machine is inside the unit's declared range (None: it declares none).
    fn fits_machine(&self, unit: &Unit) -> Option<bool> {
        unit.current().manifest.backend.as_ref()?.machine.as_ref().map(|fit| fit.holds(self.memory_bytes))
    }

    /// Every unit whose backend serves `category`, in start order.
    fn units_of(&self, category: &str) -> Vec<Arc<Unit>> {
        self.all().into_iter().filter(|unit| unit.category().as_deref() == Some(category)).collect()
    }

    /// The plugin serving `category` now.
    pub fn serving_of(&self, category: &str) -> Option<String> {
        self.serving.lock().ok().and_then(|serving| serving.get(category).map(|(plugin, _)| plugin.clone()))
    }

    /// Another plugin serves this unit's category: it is installed but does not run.
    pub fn is_standby(&self, unit: &Unit) -> bool {
        unit.category().and_then(|category| self.serving_of(&category)).is_some_and(|serving| serving != unit.id)
    }

    /// The swap engine committed `plugin` in `category`'s slot: it serves the category now. A
    /// recorded choice follows it (a crash in the watch window that restored the previous plugin
    /// is not undone at the next start).
    pub fn serving_changed(&self, category: &str, plugin: &str) {
        if let Ok(mut serving) = self.serving.lock() {
            let why = if serving.get(category).is_some_and(|(id, _)| id == plugin) { serving[category].1 } else { "user" };
            serving.insert(category.to_owned(), (plugin.to_owned(), why));
        }
        let recorded = self.providers.lock().map(|mut providers| match providers.get_mut(category) {
            Some(chosen) if chosen != plugin => {
                *chosen = plugin.to_owned();
                true
            }
            _ => false,
        });
        if recorded.unwrap_or(false)
            && let Err(error) = self.persist()
        {
            tracing::error!(%error, "cannot record the serving plugin");
        }
    }

    /// Record the person's choice for `category`.
    fn choose(&self, category: &str, plugin: &str) -> Result<(), String> {
        if let Ok(mut providers) = self.providers.lock() {
            providers.insert(category.to_owned(), plugin.to_owned());
        }
        if let Ok(mut serving) = self.serving.lock() {
            serving.insert(category.to_owned(), (plugin.to_owned(), "user"));
        }
        self.persist()
    }

    /// The slot settings of `plugin`'s backend as it is installed now.
    pub fn backend_config_of(&self, plugin: &str) -> Option<PluginConfig> {
        let unit = self.get(plugin)?;
        backend_config(&unit.current(), &self.override_of(plugin))
    }

    /// A unit as `frame.plugins.list` shows it.
    pub fn info_of(&self, unit: &Unit) -> PluginUnit {
        PluginUnit { standby: self.is_standby(unit), ..unit.info() }
    }

    /// `frame.plugins.providers`.
    pub fn providers(&self, category: &str) -> ProvidersResult {
        let (serving, chosen_by) = self
            .serving
            .lock()
            .ok()
            .and_then(|serving| serving.get(category).map(|(plugin, why)| (plugin.clone(), (*why).to_owned())))
            .unwrap_or_default();
        let providers = self
            .units_of(category)
            .iter()
            .map(|unit| {
                let current = unit.current();
                let meta = &current.manifest.plugin;
                ProviderInfo {
                    plugin: unit.id.clone(),
                    name: if meta.name.is_empty() { unit.id.clone() } else { meta.name.clone() },
                    description: meta.description.clone(),
                    version: meta.version.clone(),
                    enabled: unit.enabled(),
                    serving: unit.id == serving,
                    machine: current.manifest.backend.as_ref().and_then(|backend| backend.machine.clone()),
                    fits_machine: self.fits_machine(unit),
                }
            })
            .collect();
        ProvidersResult { category: category.to_owned(), serving, chosen_by, memory_bytes: self.memory_bytes, providers }
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
        match self.serving_of(category) {
            Some(plugin) => self.get(&plugin),
            None => self.units_of(category).into_iter().next(),
        }
    }

    pub fn override_of(&self, plugin: &str) -> PluginOverride {
        self.overrides.get(plugin).cloned().unwrap_or_default()
    }

    pub fn disabled(&self) -> BTreeSet<String> {
        self.all().iter().filter(|u| !u.enabled()).map(|u| u.id.clone()).collect()
    }

    /// Slot settings of every plugin with a backend that serves its category, in start order
    /// (alternatives on standby get no slot).
    pub fn backends(&self) -> Vec<PluginConfig> {
        self.all()
            .iter()
            .filter(|unit| !self.is_standby(unit))
            .filter_map(|unit| backend_config(&unit.current(), &self.override_of(&unit.id)))
            .collect()
    }

    /// UI bundle directories of every unit's running version (disabled ones included; the UI
    /// plugin table filters them).
    pub fn ui_sources(&self) -> Vec<(String, PathBuf)> {
        // An alternative on standby keeps its UI to itself: the category's panels and cards are the
        // serving plugin's.
        self.all().iter().filter(|unit| !self.is_standby(unit)).flat_map(|unit| unit.ui_of(&unit.current())).collect()
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

    /// Write each plugin's install state into its folder (see the module notes).
    fn persist(&self) -> Result<(), String> {
        let providers = self.providers.lock().map(|providers| providers.clone()).unwrap_or_default();
        let disabled = self.disabled();
        for unit in self.all() {
            let dir = self.root.join(&unit.id);
            let (current, previous, known_good) = {
                let state = unit.lock();
                (state.current.version().to_owned(), state.previous.as_ref().map(|p| p.version().to_owned()), state.known_good.clone())
            };
            let chosen = unit.category().is_some_and(|category| providers.get(&category) == Some(&unit.id));
            write_record(&dir, CURRENT, Some(&current))?;
            write_record(&dir, PREVIOUS, previous.as_deref())?;
            write_record(&dir, KNOWN_GOOD, known_good.as_deref())?;
            write_record(&dir, DISABLED, disabled.contains(&unit.id).then_some(""))?;
            write_record(&dir, CHOSEN, chosen.then_some(""))?;
        }
        Ok(())
    }
}

/// A plugin folder's install-state files.
const CURRENT: &str = ".current";
const PREVIOUS: &str = ".previous";
const KNOWN_GOOD: &str = ".known_good";
const DISABLED: &str = ".disabled";
const CHOSEN: &str = ".chosen";

/// Write one install-state file (`Some`: its text, `None`: removed).
fn write_record(dir: &Path, name: &str, text: Option<&str>) -> Result<(), String> {
    let path = dir.join(name);
    match text {
        Some(text) => {
            if std::fs::read_to_string(&path).is_ok_and(|stored| stored == text) {
                return Ok(());
            }
            write_atomic(&path, text.as_bytes()).map_err(|e| format!("cannot write {}: {e}", path.display()))
        }
        None => match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(format!("cannot remove {}: {error}", path.display())),
        },
    }
}

/// The install state of every plugin folder under `root` that records a current version.
fn read_records(root: &Path) -> PluginsFile {
    let mut saved = PluginsFile::default();
    let Ok(entries) = std::fs::read_dir(root) else { return saved };
    let read = |dir: &Path, name: &str| std::fs::read_to_string(dir.join(name)).ok().map(|text| text.trim().to_owned()).filter(|text| !text.is_empty());
    for entry in entries.filter_map(|entry| entry.ok()) {
        let dir = entry.path();
        let id = entry.file_name().to_string_lossy().into_owned();
        if id.starts_with('.') || !dir.is_dir() {
            continue;
        }
        let Some(current) = read(&dir, CURRENT) else { continue };
        if dir.join(DISABLED).exists() {
            saved.disabled.insert(id.clone());
        }
        if dir.join(CHOSEN).exists()
            && let Ok(installed) = crate::package::load(root, &id, &current)
            && let Some(category) = installed.manifest.backend.as_ref().map(|backend| backend.category.clone())
        {
            saved.providers.insert(category, id.clone());
        }
        saved.installed.insert(id, InstalledRecord { current, previous: read(&dir, PREVIOUS), known_good: read(&dir, KNOWN_GOOD) });
    }
    saved
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
        return Ok(PluginToggleResult { plugin: frame.units.info_of(&unit), changed: false, steps, ui: Vec::new() });
    }
    unit.lock().enabled = enabled;
    if let Err(error) = frame.units.persist() {
        unit.lock().enabled = !enabled;
        return Err(RpcError::internal(error));
    }
    // A standby alternative has no slot of its own: the category's slot runs another plugin.
    let slot = unit.category().and_then(|category| frame.sup.slot(&category)).filter(|slot| slot.plugin() == id);
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
    let info = frame.units.info_of(&unit);
    publish_unit(frame, &info);
    tracing::info!(plugin = id, enabled, "plugin {}", if enabled { "enabled" } else { "disabled" });
    Ok(PluginToggleResult { plugin: info, changed: true, steps, ui: ui_ids })
}

/// The baseline is written before the first activation. A frame restart restores it before any
/// plugin starts; a source-manager replacement can wait for the final result without owning it.
#[derive(Serialize, Deserialize)]
struct InstallBatch {
    id: String,
    previous: BTreeMap<String, InstalledRecord>,
    targets: BTreeMap<String, String>,
    result: Option<InstallManyResult>,
}

fn batch_path(dir: &Path, id: &str) -> Result<PathBuf, RpcError> {
    if id.is_empty() || id.len() > 128 || !id.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-') {
        return Err(RpcError::invalid_params("the install id must contain only letters, digits or hyphens"));
    }
    Ok(dir.join(format!("{id}.json")))
}

fn save_batch(dir: &Path, batch: &InstallBatch) -> Result<(), RpcError> {
    std::fs::create_dir_all(dir).map_err(|e| RpcError::internal(format!("cannot create {}: {e}", dir.display())))?;
    let path = batch_path(dir, &batch.id)?;
    let bytes = serde_json::to_vec(batch).map_err(|e| RpcError::internal(e.to_string()))?;
    write_atomic(&path, &bytes).map_err(|e| RpcError::internal(format!("cannot record install {}: {e}", batch.id)))
}

fn read_batch(dir: &Path, id: &str) -> Result<InstallBatch, RpcError> {
    let path = batch_path(dir, id)?;
    let bytes = std::fs::read(&path).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound { RpcError::not_found(format!("no install `{id}`")) }
        else { RpcError::internal(format!("cannot read {}: {e}", path.display())) }
    })?;
    serde_json::from_slice(&bytes).map_err(|e| RpcError::internal(format!("{} is corrupt: {e}", path.display())))
}

fn recover_batches(root: &Path, dir: &Path, saved: &mut PluginsFile) -> Result<(), String> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(format!("cannot read {}: {error}", dir.display())),
    };
    for entry in entries {
        let path = entry.map_err(|e| e.to_string())?.path();
        if path.extension().and_then(|s| s.to_str()) != Some("json") { continue; }
        let bytes = std::fs::read(&path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        let mut batch: InstallBatch = serde_json::from_slice(&bytes).map_err(|e| format!("{} is corrupt: {e}", path.display()))?;
        if batch.result.is_some() { continue; }
        let mut errors = Vec::new();
        for (id, previous) in &batch.previous {
            if let Err(error) = crate::package::load(root, id, &previous.current) {
                errors.push(format!("{id}: {error}"));
                continue;
            }
            let plugin_dir = root.join(id);
            let restored = write_record(&plugin_dir, CURRENT, Some(&previous.current))
                .and_then(|()| write_record(&plugin_dir, PREVIOUS, previous.previous.as_deref()))
                .and_then(|()| write_record(&plugin_dir, KNOWN_GOOD, previous.known_good.as_deref()));
            match restored {
                Ok(()) => { saved.installed.insert(id.clone(), InstalledRecord {
                    current: previous.current.clone(), previous: previous.previous.clone(), known_good: previous.known_good.clone(),
                }); }
                Err(error) => errors.push(error),
            }
        }
        batch.result = Some(InstallManyResult { id: batch.id.clone(), ok: false, swaps: Vec::new(),
            error: Some("The frame stopped during installation; restoration of the previous versions was attempted before startup.".into()), recovery_errors: errors });
        save_batch(dir, &batch).map_err(|e| e.message)?;
    }
    Ok(())
}

/// Install a prebuilt set under the existing package operation gate. The frame owns this future,
/// so replacing the requesting source plugin cannot interrupt the remaining swaps or rollbacks.
pub async fn install_many(frame: &Arc<Frame>, params: InstallManyParams) -> Result<InstallManyResult, RpcError> {
    let _op = frame.units.op.lock().await;
    let path = batch_path(&frame.units.batch_dir, &params.id)?;
    if path.exists() {
        return read_batch(&frame.units.batch_dir, &params.id)?.result.ok_or_else(|| RpcError::internal("the previous install has no final result; restart the frame to recover it"));
    }
    if params.paths.is_empty() { return Err(RpcError::invalid_params("give the built plugin packages")); }
    let mut prepared = Vec::new();
    let mut previous = BTreeMap::new();
    let mut targets = BTreeMap::new();
    // Validate and unpack everything before replacing any running version.
    for path in params.paths {
        let archive = PathBuf::from(path);
        if !archive.is_absolute() { return Err(RpcError::invalid_params("package paths must be absolute")); }
        let copy = frame.clone();
        let installed = tokio::task::spawn_blocking(move || {
            crate::package::install_archive(&copy.units.root, &archive, &|id, version| copy.units.in_use(id, version))
        }).await.map_err(|e| RpcError::internal(e.to_string()))?.map_err(RpcError::invalid_params)?;
        let id = installed.id().to_owned();
        if targets.contains_key(&id) { return Err(RpcError::invalid_params(format!("duplicate plugin `{id}` in install"))); }
        let unit = frame.units.get(&id).ok_or_else(|| RpcError::invalid_params(format!("{id} has no installed version to restore; install this new plugin separately")))?;
        check_same_category(&unit, &installed).map_err(RpcError::invalid_params)?;
        let state = unit.lock();
        previous.insert(id.clone(), InstalledRecord { current: state.current.version().to_owned(), previous: state.previous.as_ref().map(|p| p.version().to_owned()), known_good: state.known_good.clone() });
        targets.insert(id, installed.version().to_owned());
        drop(state);
        prepared.push(Arc::new(installed));
    }
    // The successor can wait for this operation after activation; no work remains in its parent.
    prepared.sort_by_key(|installed| installed.manifest.backend.as_ref().is_some_and(|b| b.category == "source"));
    let mut batch = InstallBatch { id: params.id.clone(), previous, targets, result: None };
    save_batch(&frame.units.batch_dir, &batch)?;
    let mut result = InstallManyResult { id: params.id, ok: true, swaps: Vec::new(), error: None, recovery_errors: Vec::new() };
    let mut attempted = Vec::new();
    for installed in &prepared {
        let Some(unit) = frame.units.get(installed.id()) else {
            result.ok = false;
            result.error = Some(format!("{} disappeared during installation", installed.id()));
            break;
        };
        attempted.push(installed.id().to_owned());
        let from_version = unit.current().version().to_owned();
        let mut steps = Vec::new();
        let outcome = switch(frame, &unit, installed.clone(), "install", &mut steps).await;
        match finish(frame, &unit, from_version, steps, outcome) {
            Ok(swapped) => {
                let persistence_error = swapped.steps.iter().find(|step| step.step == "persist" && !step.ok).map(|step| step.detail.clone());
                let failed = !swapped.ok || persistence_error.is_some();
                if failed { result.ok = false; result.error = persistence_error.or_else(|| swapped.error.clone()); }
                result.swaps.push(swapped);
            }
            Err(error) => { result.ok = false; result.error = Some(error.message); }
        }
        if !result.ok { break; }
    }
    if result.ok && let Err(error) = frame.units.persist() {
        result.ok = false;
        result.error = Some(format!("The installed versions could not be recorded: {error}"));
    }
    if !result.ok {
        result.recovery_errors = restore_batch(frame, &batch, &attempted, &result.swaps).await;
    }
    batch.result = Some(result.clone());
    if let Err(error) = save_batch(&frame.units.batch_dir, &batch) {
        if result.ok {
            result.ok = false;
            result.error = Some(error.message.clone());
            result.recovery_errors = restore_batch(frame, &batch, &attempted, &result.swaps).await;
            batch.result = Some(result.clone());
        }
        if let Err(record) = save_batch(&frame.units.batch_dir, &batch) {
            return Err(RpcError::internal(format!("{}; recovery details: {}", record.message, result.recovery_errors.join("; ")))
                .with_data(serde_json::to_value(&result).unwrap_or(serde_json::Value::Null)));
        }
    }
    Ok(result)
}

async fn restore_batch(frame: &Arc<Frame>, batch: &InstallBatch, attempted: &[String], swaps: &[PackageSwapResult]) -> Vec<String> {
    let mut errors = Vec::new();
    // Include the failing plugin: a backend may also have failed its own automatic restore.
    for id in attempted.iter().rev() {
        let Some(unit) = frame.units.get(id) else { errors.push(format!("{id} disappeared during rollback")); continue; };
        let baseline = &batch.previous[id];
        let already_restored = unit.current().version() == baseline.current
            && swaps.iter().find(|swap| swap.plugin == *id).is_some_and(|swap| swap.ok || swap.rolled_back);
        if !already_restored {
            let prior = match crate::package::load(&frame.units.root, id, &baseline.current) {
                Ok(prior) => Arc::new(prior),
                Err(error) => { errors.push(error); continue; }
            };
            let mut steps = Vec::new();
            let from = unit.current().version().to_owned();
            let outcome = switch(frame, &unit, prior, "batch rollback", &mut steps).await;
            match finish(frame, &unit, from, steps, outcome) {
                Ok(restored) if restored.ok => {},
                Ok(restored) => { errors.push(format!("{id}: {}", restored.error.unwrap_or_else(|| "the previous version did not start".into()))); continue; }
                Err(error) => { errors.push(format!("{id}: {}", error.message)); continue; }
            }
        }
        // Preserve the baseline's rollback and known-good pointers, not the rejected package.
        let previous = match baseline.previous.as_deref().map(|version| crate::package::load(&frame.units.root, id, version)).transpose() {
            Ok(previous) => previous.map(Arc::new),
            Err(error) => { errors.push(error); continue; }
        };
        let mut state = unit.lock();
        state.previous = previous;
        state.known_good = baseline.known_good.clone();
    }
    if let Err(error) = frame.units.persist() {
        errors.push(format!("The previous plugin versions could not be recorded: {error}"));
    }
    errors
}

/// A successor source plugin waits on the existing operation gate, then reads the durable result.
pub async fn install_wait(frame: &Arc<Frame>, id: &str) -> Result<InstallManyResult, RpcError> {
    let _op = frame.units.op.lock().await;
    read_batch(&frame.units.batch_dir, id)?.result.ok_or_else(|| RpcError::internal("the install has no final result; restart the frame to recover it"))
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
    let unit = Arc::new(Unit::new(installed, true));
    if let Some(backend) = &backend {
        if let Some(existing) = frame.sup.slot(&backend.category) {
            // Another plugin serves the category: this one waits as its alternative.
            tracing::info!(category = %backend.category, plugin = %unit.id, serving = %existing.plugin(), "installed an alternative; it is on standby");
        } else {
            frame.sup.add_slot(supervisor::Slot::new(backend, false)).map_err(RpcError::invalid_params)?;
            if let Ok(mut serving) = frame.units.serving.lock() {
                serving.insert(backend.category.clone(), (unit.id.clone(), "only"));
            }
        }
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
    if let Some(category) = unit.category().filter(|_| frame.units.is_standby(unit)) {
        let serving = frame.units.serving_of(&category).unwrap_or_default();
        steps.push(SwapStep { step: "standby".into(), ok: true, ms: 0, detail: format!("`{category}` is served by {serving}; switch to {} with frame.plugins.switch", unit.id) });
        return Ok(None);
    }
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
    if frame.units.is_standby(unit) {
        steps.push(SwapStep { step: "backend".into(), ok: true, ms: 0, detail: "the plugin is on standby; the new version starts when it is switched to".into() });
    } else if let Some(category) = unit.category() {
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
    let info = frame.units.info_of(unit);
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
    publish_unit(frame, &frame.units.info_of(&unit));
}

/// Publish `plugin.unit` for every unit (a switch changes which ones are on standby).
pub fn publish_units(frame: &Frame) {
    for unit in frame.units.all() {
        publish_unit(frame, &frame.units.info_of(&unit));
    }
}

/// `frame.plugins.switch`: serve `category` with `plugin`, another installed plugin of it.
pub async fn switch_provider(frame: &Arc<Frame>, category: &str, plugin: &str) -> Result<SwitchResult, RpcError> {
    let _op = frame.units.op.lock().await;
    let unit = frame.units.get(plugin).ok_or_else(|| RpcError::not_found(format!("no plugin `{plugin}`")))?;
    if unit.category().as_deref() != Some(category) {
        return Err(RpcError::invalid_params(format!("plugin `{plugin}` does not serve `{category}`")));
    }
    if !unit.enabled() {
        return Err(RpcError::invalid_params(format!("plugin `{plugin}` is disabled; enable it before switching to it")));
    }
    let slot = frame.sup.slot(category).ok_or_else(|| RpcError::not_found(format!("`{category}` has no slot")))?;
    let from = slot.plugin();
    let unchanged = |ok: bool, error: Option<String>| SwitchResult {
        ok,
        category: category.to_owned(),
        from_plugin: from.clone(),
        to_plugin: plugin.to_owned(),
        changed: false,
        backend: None,
        steps: Vec::new(),
        error,
    };
    if from == plugin {
        frame.units.choose(category, plugin).map_err(RpcError::internal)?;
        return Ok(unchanged(true, None));
    }
    // A switch happens only while nothing runs: a running request would be cut off, and a
    // conversation is never continued on another engine.
    if !slot.is_disabled()
        && let Some(running) = supervisor::in_flight(&slot).await
        && running > 0
    {
        return Err(RpcError::new(
            cc_protocol::codes::BUSY,
            format!("{from} is serving {running} request(s) now: switch the engine when nothing runs"),
        ));
    }
    let spec = frame.units.current_spec(plugin).ok_or_else(|| RpcError::not_found(format!("`{plugin}` has no installed backend")))?;
    let mut steps = Vec::new();
    let started = Instant::now();
    let result = if slot.is_disabled() {
        // The serving plugin is disabled: nothing runs to drain; the chosen one starts.
        if let Some(cfg) = frame.units.backend_config_of(plugin) {
            slot.serve_with(cfg);
        }
        frame.units.serving_changed(category, plugin);
        steps.extend(supervisor::enable(frame, &slot, spec).await);
        None
    } else {
        let note = format!("switch {category} from {from} to {plugin}");
        Some(supervisor::swap_for_package(frame, &slot, spec, &note).await)
    };
    let ok = result.as_ref().is_none_or(|swap| swap.ok);
    step(&mut steps, "backend", ok, started, if ok { format!("{plugin} serves `{category}`") } else { format!("{from} keeps serving `{category}`") });
    if ok {
        frame.units.choose(category, plugin).map_err(RpcError::internal)?;
    }
    let started = Instant::now();
    let changes = frame.refresh_ui().await;
    step(&mut steps, "ui", true, started, format!("{} UI package(s) changed", changes.len()));
    publish_units(frame);
    let error = result.as_ref().and_then(|swap| swap.error.clone());
    tracing::info!(%category, %from, to = plugin, ok, "switch");
    Ok(SwitchResult { ok, category: category.to_owned(), from_plugin: from, to_plugin: plugin.to_owned(), changed: ok, backend: result, steps, error })
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

/// The record of the app version a plugin was last installed from (`<root>/<id>/.bundled`).
const BUNDLED: &str = ".bundled";

/// Queue the app's prebuilt packages (`<packages>/<id>@<version>.ccplugin`) in `.incoming/` when
/// this home has not installed that version from the app yet. A plugin the user rebuilt from the
/// code folder keeps its own version until the app brings a new one. Returns what was queued.
pub fn queue_bundled(root: &Path, packages: &Path) -> Vec<(String, String)> {
    let mut queued = Vec::new();
    let Ok(entries) = std::fs::read_dir(packages) else {
        tracing::warn!(dir = %packages.display(), "the app's plugin packages are missing");
        return queued;
    };
    let incoming = root.join(crate::package::INCOMING);
    for entry in entries.filter_map(|entry| entry.ok()) {
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some((id, version)) = name.strip_suffix(".ccplugin").and_then(|stem| stem.split_once('@')) else { continue };
        if !cc_protocol::contracts::frame::package::valid_id(id) || !crate::package::valid_version(version) {
            continue;
        }
        let taken = std::fs::read_to_string(root.join(id).join(BUNDLED)).map(|text| text.trim() == version).unwrap_or(false);
        if taken {
            continue;
        }
        // An update installed a newer release than the app carries: the app's older one stays out.
        let current = std::fs::read_to_string(root.join(id).join(CURRENT)).map(|text| text.trim().to_owned()).unwrap_or_default();
        if cc_protocol::version::is_newer(&current, version) {
            if let Err(error) = write_record(&root.join(id), BUNDLED, Some(version)) {
                tracing::warn!(plugin = %id, %error, "cannot record the app version passed over");
            }
            continue;
        }
        if let Err(error) = std::fs::create_dir_all(&incoming).and_then(|()| std::fs::copy(entry.path(), incoming.join(&name)).map(|_| ())) {
            tracing::error!(package = %name, %error, "cannot queue the app's plugin package");
            continue;
        }
        queued.push((id.to_owned(), version.to_owned()));
    }
    queued
}

/// The app's prebuilt packages, for `frame.plugins.bundled`.
pub fn bundled(packages: Option<&Path>) -> cc_protocol::contracts::frame::BundledResult {
    use cc_protocol::contracts::frame::{BundledPackage, BundledResult};
    let Some(packages) = packages else { return BundledResult::default() };
    let mut list = Vec::new();
    if let Ok(entries) = std::fs::read_dir(packages) {
        for entry in entries.filter_map(|entry| entry.ok()) {
            let name = entry.file_name().to_string_lossy().into_owned();
            let Some((id, version)) = name.strip_suffix(".ccplugin").and_then(|stem| stem.split_once('@')) else { continue };
            if !cc_protocol::contracts::frame::package::valid_id(id) || !crate::package::valid_version(version) {
                continue;
            }
            list.push(BundledPackage { id: id.to_owned(), version: version.to_owned(), path: entry.path().to_string_lossy().into_owned() });
        }
    }
    list.sort_by(|a, b| a.id.cmp(&b.id));
    BundledResult { dir: packages.to_string_lossy().into_owned(), packages: list }
}

/// Record the app versions that installed (a failed install is queued again at the next start).
pub fn mark_bundled(root: &Path, queued: &[(String, String)]) {
    for (id, version) in queued {
        if crate::package::versions(root, id).iter().any(|installed| installed == version)
            && let Err(error) = write_record(&root.join(id), BUNDLED, Some(version))
        {
            tracing::warn!(plugin = %id, %error, "cannot record the app version installed");
        }
    }
}
