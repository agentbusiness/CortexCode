//! Plugin supervision and the hot-swap engine.
//!
//! Every category has a [`Slot`]. Bring-up, swaps, rollbacks and restarts of a slot are
//! serialized by its operation lock. A version runs as an [`Instance`]: a child process started
//! from an installed package version, admitted after `frame.hello` validation, a passing `plugin.probe`, the
//! state handoff and `plugin.activate`, and then committed by atomically switching the route.
//!
//! - blue_green: start, probe and hand over to the new instance while the old one serves; commit;
//!   then drain, shut down and retire the old one.
//! - drain_first (declared in the old instance's hello): unroute, drain, export and stop the old
//!   instance, then start the new one; if it fails, restore the old one with its exported state.
//! - Watch window: a committed swap whose new instance crashes within [`WATCH_WINDOW`] is rolled
//!   back to the previous version. Failure at any step before commit kills the new instance.
//! - Crashes restart with exponential backoff; [`CRASH_LOOP_COUNT`] crashes within
//!   [`CRASH_LOOP_WINDOW`] fall back to the plugin's last known good version.

use crate::config::PluginConfig;
use crate::hub::{FRAME_VERSION, Frame, Peer, Route};
use cc_protocol::contracts::frame::{PluginInfo, SwapParams, SwapResult, SwapStep, topics};
use cc_protocol::contracts::lifecycle::{
    DrainParams, DrainResult, Health, Hello, HelloAck, ProbeReport, SwapPolicy, methods as lifecycle,
};
use cc_protocol::contracts::{contract_of, now_ms};
use cc_protocol::{RpcError, codes, to_value};
use serde_json::Value;
use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, oneshot, watch};

/// A committed swap whose new instance crashes within this window is rolled back.
pub const WATCH_WINDOW: Duration = Duration::from_secs(60);
/// A committed instance that survives this long without crashing is the known good version.
pub const STABLE_AFTER: Duration = Duration::from_secs(60);
pub const CRASH_LOOP_COUNT: usize = 5;
pub const CRASH_LOOP_WINDOW: Duration = Duration::from_secs(60);
const BACKOFF_BASE: Duration = Duration::from_millis(250);
const BACKOFF_MAX: Duration = Duration::from_secs(30);
const DRAIN_TIMEOUT: Duration = Duration::from_secs(30);
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);
const EXIT_GRACE: Duration = Duration::from_secs(5);

/// A runnable version of a plugin's backend: an installed package version.
#[derive(Debug, Clone, PartialEq)]
pub struct ExecSpec {
    /// The plugin (unit) this version belongs to. A category's slot runs one plugin at a time,
    /// and `frame.plugins.switch` moves it to another plugin of the same category.
    pub plugin: String,
    /// The executable the process runs (the package's [`crate::package::PROCESS_NAME`] link).
    pub exec: PathBuf,
    /// The plugin's data folder name under `<home>/plugins-data` (the packed executable's name).
    pub data_name: String,
    pub args: Vec<String>,
    /// The `[backend.config]` table (with the frame config's keys) delivered in the hello ack.
    pub config: Value,
    /// The package version.
    pub version: String,
    /// The git commit the package was built from.
    pub commit: Option<String>,
    /// The plugin's folder inside the code folder (`[source] subdir`), when it has source.
    pub source_subdir: Option<String>,
}

// ----- instances -----

pub struct Instance {
    pub id: String,
    pub category: String,
    /// The plugin (unit) the instance belongs to.
    pub plugin: String,
    pub spec: ExecSpec,
    pub pid: u32,
    pub started_ms: u64,
    state: Mutex<&'static str>,
    pub hello: OnceLock<Hello>,
    peer: OnceLock<Arc<Peer>>,
    hello_tx: Mutex<Option<oneshot::Sender<Result<(), String>>>>,
    hello_rx: Mutex<Option<oneshot::Receiver<Result<(), String>>>>,
    exit: watch::Receiver<Option<String>>,
    kill_tx: mpsc::UnboundedSender<()>,
    retiring: AtomicBool,
    committed: AtomicBool,
    crashed: AtomicBool,
}

impl Instance {
    pub fn is_retiring(&self) -> bool {
        self.retiring.load(Ordering::SeqCst)
    }

    pub fn has_exited(&self) -> bool {
        self.exit.borrow().is_some()
    }

    pub fn exit_description(&self) -> Option<String> {
        self.exit.borrow().clone()
    }

    pub fn kill(&self) {
        let _ = self.kill_tx.send(());
    }

    pub fn state(&self) -> &'static str {
        self.state.lock().map(|state| *state).unwrap_or("unknown")
    }

    fn version(&self) -> String {
        self.hello.get().map(|hello| hello.version.clone()).unwrap_or_default()
    }

    fn policy(&self) -> SwapPolicy {
        self.hello.get().map(|hello| hello.swap).unwrap_or_default()
    }

    async fn wait_exit(&self, limit: Duration) -> bool {
        let mut exit = self.exit.clone();
        tokio::time::timeout(limit, async {
            while exit.borrow_and_update().is_none() {
                if exit.changed().await.is_err() {
                    return;
                }
            }
        })
        .await
        .is_ok()
    }

    /// Call a lifecycle method on the instance with a deadline.
    async fn call(&self, method: &str, params: Value, limit: Duration) -> Result<Value, String> {
        let conn = self.peer.get().and_then(|peer| peer.conn().cloned()).ok_or_else(|| "instance is not connected".to_owned())?;
        conn.call_timeout(method, params, limit).await.map_err(|error| error.to_string())
    }

    pub(crate) fn route(&self) -> Option<Route> {
        let peer = self.peer.get()?;
        Some(Route { conn: peer.conn()?.clone(), instance: self.id.clone(), peer: peer.id })
    }
}

/// Validate a hello's contract against the categories this frame knows (structural check of
/// `<category>@<major>`).
pub fn check_contract(category: &str, contract: &str) -> Result<(), String> {
    let known = contract_of(category).ok_or_else(|| format!("unknown category `{category}`"))?;
    let (contract_category, major) =
        contract.split_once('@').ok_or_else(|| format!("malformed contract `{contract}` (expected <category>@<major>)"))?;
    // A family's members (`lang-go`) speak the family's contract (`lang@1`).
    let expected = known.split_once('@').map(|(name, _)| name).unwrap_or(category);
    if contract_category != expected {
        return Err(format!("contract `{contract}` does not belong to category `{category}`"));
    }
    let known_major = known.split_once('@').map(|(_, major)| major).unwrap_or("");
    if major != known_major {
        return Err(format!("contract `{contract}` is not supported; this frame speaks `{known}`"));
    }
    Ok(())
}

// ----- slots -----

struct WatchWindow {
    instance: String,
    until: Instant,
    rollback_to: ExecSpec,
}

#[derive(Default)]
struct SlotInner {
    /// The committed instance (it may have crashed).
    current: Option<Arc<Instance>>,
    /// Instances being staged or retired.
    others: Vec<Arc<Instance>>,
    /// N-1: the version the current one replaced.
    previous: Option<ExecSpec>,
    window: Option<WatchWindow>,
    crashes: VecDeque<Instant>,
    restarts: u32,
    generation: u64,
    /// A human-readable reason the slot has no active instance.
    status: String,
}

pub struct Slot {
    pub category: String,
    /// The plugin (unit) serving this category; a switch to another plugin of the category
    /// changes it when the new plugin's instance is committed.
    plugin: Mutex<String>,
    pub cfg: Mutex<PluginConfig>,
    pub(crate) op: tokio::sync::Mutex<()>,
    inner: Mutex<SlotInner>,
    /// The plugin is disabled: nothing starts, restarts or swaps this slot until it is enabled.
    disabled: AtomicBool,
}

impl Slot {
    pub fn new(cfg: &PluginConfig, disabled: bool) -> Arc<Slot> {
        let status = if disabled { "disabled" } else { "waiting to start" };
        Arc::new(Slot {
            category: cfg.category.clone(),
            plugin: Mutex::new(cfg.plugin.clone()),
            cfg: Mutex::new(cfg.clone()),
            op: tokio::sync::Mutex::new(()),
            inner: Mutex::new(SlotInner { status: status.into(), ..SlotInner::default() }),
            disabled: AtomicBool::new(disabled),
        })
    }

    pub fn is_disabled(&self) -> bool {
        self.disabled.load(Ordering::SeqCst)
    }

    /// The plugin serving this category.
    pub fn plugin(&self) -> String {
        self.plugin.lock().map(|plugin| plugin.clone()).unwrap_or_else(|poisoned| poisoned.into_inner().clone())
    }

    /// Move the slot to another plugin of its category (its config comes with it).
    pub(crate) fn serve_with(&self, cfg: PluginConfig) {
        if let Ok(mut plugin) = self.plugin.lock() {
            *plugin = cfg.plugin.clone();
        }
        if let Ok(mut current) = self.cfg.lock() {
            *current = cfg;
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, SlotInner> {
        self.inner.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    pub fn config(&self) -> PluginConfig {
        self.cfg.lock().map(|cfg| cfg.clone()).unwrap_or_else(|poisoned| poisoned.into_inner().clone())
    }

    fn current(&self) -> Option<Arc<Instance>> {
        self.lock().current.clone()
    }

    /// The committed instance if it is still running.
    pub fn live_current(&self) -> Option<Arc<Instance>> {
        self.current().filter(|instance| !instance.has_exited())
    }

    fn generation(&self) -> u64 {
        self.lock().generation
    }

    fn bump(&self) -> u64 {
        let mut inner = self.lock();
        inner.generation += 1;
        inner.generation
    }

    fn set_status(&self, status: impl Into<String>) {
        self.lock().status = status.into();
    }

    pub fn describe_state(&self) -> String {
        if self.is_disabled() {
            return "disabled".into();
        }
        let inner = self.lock();
        if let Some(staged) = inner.others.iter().find(|i| !i.has_exited() && !i.is_retiring()) {
            return staged.state().to_owned();
        }
        if let Some(current) = &inner.current {
            return current.state().to_owned();
        }
        if inner.status.is_empty() { "stopped".into() } else { inner.status.clone() }
    }

    fn record_crash(&self) -> usize {
        let mut inner = self.lock();
        let now = Instant::now();
        inner.crashes.push_back(now);
        while inner.crashes.front().is_some_and(|t| now.duration_since(*t) > CRASH_LOOP_WINDOW) {
            inner.crashes.pop_front();
        }
        inner.crashes.len()
    }

    fn recent_crashes(&self) -> usize {
        let inner = self.lock();
        let now = Instant::now();
        inner.crashes.iter().filter(|t| now.duration_since(**t) <= CRASH_LOOP_WINDOW).count()
    }
}

pub struct Supervisor {
    /// One slot per backend category, in start order (installed plugins add slots at runtime).
    slots: std::sync::RwLock<Vec<Arc<Slot>>>,
    pending: Mutex<HashMap<String, Arc<Instance>>>,
    next_instance: AtomicU64,
}

impl Supervisor {
    pub fn new(backends: &[PluginConfig], disabled: &std::collections::BTreeSet<String>) -> Self {
        let slots = backends.iter().map(|cfg| Slot::new(cfg, disabled.contains(&cfg.plugin))).collect();
        Self { slots: std::sync::RwLock::new(slots), pending: Mutex::new(HashMap::new()), next_instance: AtomicU64::new(1) }
    }

    pub fn slots(&self) -> Vec<Arc<Slot>> {
        self.slots.read().map(|slots| slots.clone()).unwrap_or_default()
    }

    pub fn slot(&self, category: &str) -> Option<Arc<Slot>> {
        self.slots.read().ok().and_then(|slots| slots.iter().find(|slot| slot.category == category).cloned())
    }

    /// Add the slot of a newly installed plugin's backend. Fails when the category has a slot.
    pub fn add_slot(&self, slot: Arc<Slot>) -> Result<(), String> {
        let mut slots = self.slots.write().map_err(|_| "slot table poisoned".to_owned())?;
        if let Some(existing) = slots.iter().find(|s| s.category == slot.category) {
            return Err(format!("category `{}` is already served by plugin `{}`", slot.category, existing.plugin()));
        }
        slots.push(slot);
        Ok(())
    }

    /// Validate a plugin's hello and bind its connection to the instance the frame started.
    pub fn accept_hello(&self, frame: &Arc<Frame>, peer: &Arc<Peer>, hello: Hello) -> HelloAck {
        let ack = |accepted: bool, reason: String, config: Value| HelloAck {
            accepted,
            reason,
            instance: hello.instance.clone(),
            frame_version: FRAME_VERSION.into(),
            config,
        };
        let instance = self.pending.lock().ok().and_then(|pending| pending.get(&hello.instance).cloned());
        let Some(instance) = instance else {
            tracing::warn!(instance = %hello.instance, plugin = %hello.plugin_id, "hello from an instance this frame did not start");
            return ack(false, format!("unknown instance `{}`", hello.instance), Value::Null);
        };
        let verdict = if instance.peer.get().is_some() || peer.hello.get().is_some() {
            Err("this instance already said hello".to_owned())
        } else if hello.category != instance.category {
            Err(format!("this process was started for category `{}` but says `{}`", instance.category, hello.category))
        } else {
            check_contract(&hello.category, &hello.contract)
        };
        let waiter = instance.hello_tx.lock().ok().and_then(|mut tx| tx.take());
        match verdict {
            Ok(()) => {
                let _ = instance.peer.set(peer.clone());
                let _ = instance.hello.set(hello.clone());
                let _ = peer.instance.set(instance.clone());
                let _ = peer.hello.set(hello.clone());
                peer.add_subscriptions(&hello.subscribes);
                set_state(frame, &instance, "probing");
                if let Some(waiter) = waiter {
                    let _ = waiter.send(Ok(()));
                }
                tracing::info!(instance = %instance.id, plugin = %hello.plugin_id, version = %hello.version, swap = ?hello.swap, "hello accepted");
                ack(true, String::new(), instance.spec.config.clone())
            }
            Err(reason) => {
                tracing::warn!(instance = %instance.id, %reason, "hello rejected");
                if let Some(waiter) = waiter {
                    let _ = waiter.send(Err(reason.clone()));
                }
                ack(false, reason, Value::Null)
            }
        }
    }

    pub fn list(&self, frame: &Frame) -> Vec<PluginInfo> {
        let mut list = Vec::new();
        for slot in self.slots() {
            let inner = slot.lock();
            let restarts = inner.restarts;
            let disabled = slot.is_disabled();
            if let Some(current) = inner.current.as_ref().filter(|current| !(disabled && current.has_exited())) {
                list.push(info(frame, current, restarts));
            }
            for other in &inner.others {
                let duplicate = inner.current.as_ref().is_some_and(|current| current.id == other.id);
                if !duplicate && !other.has_exited() {
                    list.push(info(frame, other, restarts));
                }
            }
            let listed = list.iter().any(|p: &PluginInfo| p.category == slot.category);
            if !listed {
                let spec = frame.units.current_spec(&slot.plugin());
                list.push(PluginInfo {
                    plugin_id: String::new(),
                    version: String::new(),
                    category: slot.category.clone(),
                    contract: String::new(),
                    instance: String::new(),
                    state: if disabled { "disabled".into() } else { "stopped".into() },
                    pid: 0,
                    exec: spec.as_ref().map(|s| s.exec.display().to_string()).unwrap_or_default(),
                    started_ms: 0,
                    restarts,
                    commit: spec.as_ref().and_then(|s| s.commit.clone()),
                    build: spec.map(|s| s.version).unwrap_or_default(),
                    known_good: false,
                    transport: "process".into(),
                    plugin: slot.plugin(),
                });
            }
        }
        list
    }
}

fn info(frame: &Frame, instance: &Instance, restarts: u32) -> PluginInfo {
    let hello = instance.hello.get();
    PluginInfo {
        plugin_id: hello.map(|h| h.plugin_id.clone()).unwrap_or_default(),
        version: hello.map(|h| h.version.clone()).unwrap_or_default(),
        category: instance.category.clone(),
        contract: hello.map(|h| h.contract.clone()).unwrap_or_default(),
        instance: instance.id.clone(),
        state: instance.state().into(),
        pid: instance.pid,
        exec: instance.spec.exec.display().to_string(),
        started_ms: instance.started_ms,
        restarts,
        build: instance.spec.version.clone(),
        commit: instance.spec.commit.clone(),
        known_good: frame.units.is_known_good(&instance.plugin, &instance.spec.version),
        transport: "process".into(),
        plugin: instance.plugin.clone(),
    }
}

fn set_state(frame: &Frame, instance: &Arc<Instance>, state: &'static str) {
    if let Ok(mut current) = instance.state.lock() {
        if *current == state {
            return;
        }
        *current = state;
    }
    let restarts = frame.sup.slot(&instance.category).map(|slot| slot.lock().restarts).unwrap_or(0);
    let info = info(frame, instance, restarts);
    tracing::info!(instance = %instance.id, state, "plugin state");
    if let Ok(value) = to_value(&info) {
        frame.publish(topics::PLUGIN_STATE, &value);
    }
}

// ----- process start -----

async fn spawn_instance(frame: &Arc<Frame>, slot: &Arc<Slot>, spec: ExecSpec) -> Result<Arc<Instance>, String> {
    if frame.is_shutting_down() {
        return Err("the frame is shutting down".into());
    }
    let category = slot.category.clone();
    let number = frame.sup.next_instance.fetch_add(1, Ordering::Relaxed);
    let id = format!("{category}-{number}-{}", &uuid::Uuid::new_v4().simple().to_string()[..6]);
    let data_dir = frame.home.plugins_data().join(&spec.data_name);
    std::fs::create_dir_all(&data_dir).map_err(|e| format!("cannot create {}: {e}", data_dir.display()))?;
    let log_path = frame.home.logs().join(format!("{category}.log"));
    let log = open_log(&log_path, &format!("--- {} instance {id} version {} exec {} args {:?}\n", now_ms(), spec.version, spec.exec.display(), spec.args))
        .map_err(|e| format!("cannot open {}: {e}", log_path.display()))?;
    let log_err = log.try_clone().map_err(|e| e.to_string())?;
    let mut command = tokio::process::Command::new(&spec.exec);
    command
        .args(&spec.args)
        .env("CORTEX_FRAME_SOCKET", &frame.socket)
        .env("CORTEX_PLUGIN_INSTANCE", &id)
        .env("CORTEX_HOME", &frame.home.root)
        .env("CORTEX_PLUGIN_DATA", &data_dir)
        .env("CORTEX_CODE", &frame.code)
        .current_dir(&frame.config.base)
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(log_err))
        // Own process group: a terminal Ctrl-C reaches only the frame, which then stops plugins in order.
        .process_group(0)
        .kill_on_drop(true);
    // Live data: the plugin's folder in the code folder (it may not exist yet; plugins check on
    // each read). Safe mode leaves it out, so plugins read their packaged data only.
    if !frame.safe
        && let Some(subdir) = &spec.source_subdir
    {
        command.env("CORTEX_PLUGIN_SOURCE", frame.code.join(subdir));
    }
    let mut child = command.spawn().map_err(|e| format!("cannot start {}: {e}", spec.exec.display()))?;
    let pid = child.id().unwrap_or(0);
    let (exit_tx, exit_rx) = watch::channel(None);
    let (kill_tx, mut kill_rx) = mpsc::unbounded_channel();
    let (hello_tx, hello_rx) = oneshot::channel();
    let instance = Arc::new(Instance {
        id: id.clone(),
        plugin: spec.plugin.clone(),
        category,
        spec,
        pid,
        started_ms: now_ms(),
        state: Mutex::new("new"),
        hello: OnceLock::new(),
        peer: OnceLock::new(),
        hello_tx: Mutex::new(Some(hello_tx)),
        hello_rx: Mutex::new(Some(hello_rx)),
        exit: exit_rx,
        kill_tx,
        retiring: AtomicBool::new(false),
        committed: AtomicBool::new(false),
        crashed: AtomicBool::new(false),
    });
    if let Ok(mut pending) = frame.sup.pending.lock() {
        pending.insert(id.clone(), instance.clone());
    }
    slot.lock().others.push(instance.clone());
    crate::children::record(&frame.home, pid, &instance.spec.exec, "plugin");
    tracing::info!(instance = %id, pid, exec = %instance.spec.exec.display(), "plugin process started");
    set_state(frame, &instance, "starting");
    let monitor_frame = frame.clone();
    let monitor_slot = slot.clone();
    let monitor_instance = instance.clone();
    tokio::spawn(async move {
        let status = loop {
            tokio::select! {
                status = child.wait() => break status,
                Some(()) = kill_rx.recv() => {
                    kill_group(pid);
                    let _ = child.start_kill();
                }
            }
        };
        // The plugin leads its own process group; take down anything it left behind.
        kill_group(pid);
        crate::children::forget(&monitor_frame.home, pid);
        let description = match status {
            Ok(status) => match (status.code(), status.signal()) {
                (Some(code), _) => format!("exited with code {code}"),
                (None, Some(signal)) => format!("killed by signal {signal}"),
                _ => "exited".to_owned(),
            },
            Err(error) => format!("wait failed: {error}"),
        };
        on_exit(monitor_frame, monitor_slot, monitor_instance, description, exit_tx);
    });
    Ok(instance)
}

/// SIGKILL a plugin's process group (the plugin is the group leader, see `process_group(0)`).
fn kill_group(pid: u32) {
    if let Ok(pgid) = i32::try_from(pid)
        && pgid > 1
    {
        // SAFETY: signalling a process group this frame created; ESRCH (already gone) is fine.
        unsafe {
            libc::killpg(pgid, libc::SIGKILL);
        }
    }
}

fn open_log(path: &Path, header: &str) -> std::io::Result<std::fs::File> {
    use std::io::Write;
    let mut file = std::fs::OpenOptions::new().create(true).append(true).open(path)?;
    file.write_all(header.as_bytes())?;
    Ok(file)
}

fn on_exit(frame: Arc<Frame>, slot: Arc<Slot>, instance: Arc<Instance>, description: String, exit_tx: watch::Sender<Option<String>>) {
    if let Ok(mut pending) = frame.sup.pending.lock() {
        pending.remove(&instance.id);
    }
    if let Some(waiter) = instance.hello_tx.lock().ok().and_then(|mut tx| tx.take()) {
        let _ = waiter.send(Err(format!("process {description} before hello")));
    }
    let routed = frame.clear_route(&instance.category, &instance.id);
    exit_tx.send_replace(Some(description.clone()));
    {
        let mut inner = slot.lock();
        inner.others.retain(|other| other.id != instance.id);
    }
    if instance.is_retiring() || frame.is_shutting_down() {
        tracing::info!(instance = %instance.id, %description, "plugin process stopped");
        set_state(&frame, &instance, "stopped");
        return;
    }
    instance.crashed.store(true, Ordering::SeqCst);
    tracing::warn!(instance = %instance.id, %description, routed, "plugin process crashed");
    set_state(&frame, &instance, "crashed");
    if instance.committed.load(Ordering::SeqCst) {
        tokio::spawn(handle_crash(frame, slot, instance, description));
    }
}

// ----- swap engine -----

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SwapKind {
    /// `frame.plugins.swap` (a restart) or an install.
    Swap,
    /// `frame.plugins.rollback` or the watch window rolling back.
    Rollback,
}

struct Steps {
    list: Vec<SwapStep>,
}

impl Steps {
    fn new() -> Self {
        Self { list: Vec::new() }
    }

    async fn run<T, F>(&mut self, name: &str, future: F) -> Result<T, String>
    where
        F: Future<Output = Result<(T, String), String>>,
    {
        let started = Instant::now();
        let outcome = future.await;
        let ms = started.elapsed().as_millis() as u64;
        match outcome {
            Ok((value, detail)) => {
                self.list.push(SwapStep { step: name.into(), ok: true, ms, detail });
                Ok(value)
            }
            Err(error) => {
                self.list.push(SwapStep { step: name.into(), ok: false, ms, detail: error.clone() });
                Err(format!("{name}: {error}"))
            }
        }
    }

    fn note(&mut self, name: &str, ok: bool, detail: impl Into<String>) {
        self.list.push(SwapStep { step: name.into(), ok, ms: 0, detail: detail.into() });
    }
}

enum Handoff {
    None,
    /// Export from the old (still serving) instance after the new one passed probe.
    From(Arc<Instance>),
    /// State the old instance exported before it stopped (drain_first).
    Import(Value),
}

/// The version's executable must still be installed.
async fn locate(spec: &ExecSpec) -> Result<(ExecSpec, String), String> {
    if !spec.exec.is_file() {
        return Err(format!("{} {} is not installed ({} is missing)", spec.data_name, spec.version, spec.exec.display()));
    }
    Ok((spec.clone(), format!("{} {}", spec.data_name, spec.version)))
}

/// Start an instance and take it up to (not including) commit: hello, probe, handoff, activate.
/// On failure the new instance is killed and the error returned.
async fn admit(
    frame: &Arc<Frame>,
    slot: &Arc<Slot>,
    spec: ExecSpec,
    handoff: Handoff,
    steps: &mut Steps,
) -> Result<Arc<Instance>, String> {
    // The timeouts are the starting plugin's own (a switch starts another plugin in this slot).
    let cfg = if spec.plugin == slot.plugin() { slot.config() } else { frame.units.backend_config_of(&spec.plugin).unwrap_or_else(|| slot.config()) };
    let instance = steps
        .run("start", async {
            let instance = spawn_instance(frame, slot, spec).await?;
            let detail = format!("pid {} instance {}", instance.pid, instance.id);
            Ok((instance, detail))
        })
        .await?;
    let outcome = admit_started(&instance, &cfg, handoff, steps).await;
    match outcome {
        Ok(()) => Ok(instance),
        Err(error) => {
            instance.retiring.store(true, Ordering::SeqCst);
            instance.kill();
            let exited = instance.wait_exit(EXIT_GRACE).await;
            steps.note("abort", exited, format!("killed new instance {} ({})", instance.id, error));
            Err(error)
        }
    }
}

async fn admit_started(
    instance: &Arc<Instance>,
    cfg: &PluginConfig,
    handoff: Handoff,
    steps: &mut Steps,
) -> Result<(), String> {
    let hello_rx = instance.hello_rx.lock().ok().and_then(|mut rx| rx.take()).ok_or("hello already awaited")?;
    let hello_limit = Duration::from_millis(cfg.hello_timeout_ms);
    steps
        .run("hello", async {
            match tokio::time::timeout(hello_limit, hello_rx).await {
                Ok(Ok(Ok(()))) => {
                    let hello = instance.hello.get().ok_or("hello missing")?;
                    Ok(((), format!("{} {} ({}, swap {:?})", hello.plugin_id, hello.version, hello.contract, hello.swap)))
                }
                Ok(Ok(Err(reason))) => Err(reason),
                Ok(Err(_)) => Err("process exited before hello".into()),
                Err(_) => Err(format!("no hello within {hello_limit:?}")),
            }
        })
        .await?;
    let probe_limit = Duration::from_millis(cfg.probe_timeout_ms);
    steps
        .run("probe", async {
            let value = instance.call(lifecycle::PROBE, Value::Null, probe_limit).await?;
            let report: ProbeReport = serde_json::from_value(value).map_err(|e| format!("malformed probe report: {e}"))?;
            let summary = report
                .checks
                .iter()
                .map(|check| format!("{}={}{}", check.name, if check.ok { "ok" } else { "FAIL" }, if check.ok { String::new() } else { format!(" ({})", check.detail) }))
                .collect::<Vec<_>>()
                .join(", ");
            if report.ok { Ok(((), summary)) } else { Err(format!("probe failed: {summary}")) }
        })
        .await?;
    match handoff {
        Handoff::None => {}
        Handoff::From(old) => {
            steps
                .run("handoff", async {
                    let state = old.call(lifecycle::EXPORT_STATE, Value::Null, probe_limit).await.map_err(|e| format!("export_state: {e}"))?;
                    let size = state.to_string().len();
                    instance.call(lifecycle::IMPORT_STATE, state, probe_limit).await.map_err(|e| format!("import_state: {e}"))?;
                    Ok(((), format!("{size} bytes of state from {}", old.id)))
                })
                .await?;
        }
        Handoff::Import(state) => {
            steps
                .run("import", async {
                    let size = state.to_string().len();
                    instance.call(lifecycle::IMPORT_STATE, state, probe_limit).await.map_err(|e| format!("import_state: {e}"))?;
                    Ok(((), format!("{size} bytes of state")))
                })
                .await?;
        }
    }
    steps
        .run("activate", async {
            instance.call(lifecycle::ACTIVATE, Value::Null, probe_limit).await?;
            Ok(((), String::new()))
        })
        .await?;
    if instance.has_exited() {
        return Err(format!("process {} during admission", instance.exit_description().unwrap_or_default()));
    }
    Ok(())
}

/// Atomically route the category to `new` and record it as the committed instance.
fn commit(frame: &Arc<Frame>, slot: &Arc<Slot>, new: &Arc<Instance>, previous: Option<ExecSpec>, window: bool) -> Result<String, String> {
    let route = new.route().ok_or("new instance has no connection")?;
    new.committed.store(true, Ordering::SeqCst);
    frame.set_route(&slot.category, route);
    let moved_from = (slot.plugin() != new.plugin).then(|| slot.plugin());
    if let Some(from) = &moved_from {
        // Another plugin of the category serves it now: the slot takes its settings, and its UI
        // packages replace the previous plugin's.
        if let Some(cfg) = frame.units.backend_config_of(&new.plugin) {
            slot.serve_with(cfg);
        }
        frame.units.serving_changed(&slot.category, &new.plugin);
        tracing::info!(category = %slot.category, from = %from, to = %new.plugin, "the category is served by another plugin");
        let frame = frame.clone();
        tokio::spawn(async move {
            frame.refresh_ui().await;
            crate::units::publish_units(&frame);
        });
    }
    {
        let mut inner = slot.lock();
        inner.others.retain(|other| other.id != new.id);
        if let Some(old) = inner.current.take()
            && old.id != new.id
            && !old.has_exited()
        {
            inner.others.push(old);
        }
        inner.current = Some(new.clone());
        inner.generation += 1;
        inner.status.clear();
        inner.window = match (&previous, window) {
            (Some(rollback_to), true) => {
                Some(WatchWindow { instance: new.id.clone(), until: Instant::now() + WATCH_WINDOW, rollback_to: rollback_to.clone() })
            }
            _ => None,
        };
        if previous.is_some() {
            inner.previous = previous;
        }
    }
    set_state(frame, new, "active");
    // Known good after surviving the stability period.
    let frame_for_timer = frame.clone();
    let slot_for_timer = slot.clone();
    let instance = new.clone();
    tokio::spawn(async move {
        tokio::time::sleep(STABLE_AFTER).await;
        let still_current = slot_for_timer.current().is_some_and(|current| current.id == instance.id);
        if still_current && !instance.has_exited() {
            mark_known_good(&frame_for_timer, &instance);
        }
    });
    Ok(format!("routing {} -> {}", slot.category, new.id))
}

pub fn mark_known_good(frame: &Frame, instance: &Instance) {
    if instance.crashed.load(Ordering::SeqCst) || !instance.committed.load(Ordering::SeqCst) {
        return;
    }
    frame.units.mark_known_good(&instance.plugin, &instance.spec.version);
}

/// Drain, shut down and retire an instance that no longer receives traffic.
async fn retire(frame: &Arc<Frame>, old: &Arc<Instance>, steps: &mut Steps) {
    old.retiring.store(true, Ordering::SeqCst);
    if !old.has_exited() {
        set_state(frame, old, "draining");
        let _ = steps
            .run("drain", async {
                let params = to_value(&DrainParams { timeout_ms: DRAIN_TIMEOUT.as_millis() as u64 }).map_err(|e| e.to_string())?;
                let value = old.call(lifecycle::DRAIN, params, DRAIN_TIMEOUT + Duration::from_secs(5)).await?;
                let result: DrainResult = serde_json::from_value(value).map_err(|e| e.to_string())?;
                if result.remaining == 0 {
                    Ok(((), format!("{} drained", old.id)))
                } else {
                    Err(format!("{} requests still running after {DRAIN_TIMEOUT:?}", result.remaining))
                }
            })
            .await;
    }
    stop(frame, old, steps).await;
}

/// `plugin.shutdown`, then wait for the exit, then kill.
async fn stop(frame: &Arc<Frame>, old: &Arc<Instance>, steps: &mut Steps) {
    old.retiring.store(true, Ordering::SeqCst);
    let mut clean = false;
    if !old.has_exited() {
        clean = steps
            .run("shutdown", async {
                old.call(lifecycle::SHUTDOWN, Value::Null, SHUTDOWN_TIMEOUT).await?;
                Ok(((), String::new()))
            })
            .await
            .is_ok();
    }
    let exited_by_itself = steps
        .run("retire", async {
            if old.wait_exit(EXIT_GRACE).await {
                Ok((true, format!("{} {}", old.id, old.exit_description().unwrap_or_default())))
            } else {
                old.kill();
                if old.wait_exit(EXIT_GRACE).await {
                    Ok((false, format!("{} killed after {EXIT_GRACE:?} grace", old.id)))
                } else {
                    Err(format!("{} did not exit", old.id))
                }
            }
        })
        .await
        .unwrap_or(false);
    // A version that served and then shut down cleanly when asked is known good.
    if clean && exited_by_itself {
        mark_known_good(frame, old);
    }
    set_state(frame, old, "stopped");
}

fn swap_result(category: &str, from: String, to: String, steps: Steps, outcome: Result<(), String>, rolled_back: bool) -> SwapResult {
    SwapResult {
        ok: outcome.is_ok(),
        category: category.into(),
        from_version: from,
        to_version: to,
        steps: steps.list,
        error: outcome.err(),
        rolled_back,
    }
}

/// Swap a slot to `target`. The caller must hold the slot's operation lock.
async fn swap_locked(frame: &Arc<Frame>, slot: &Arc<Slot>, target: ExecSpec, kind: SwapKind, mut steps: Steps) -> SwapResult {
    slot.bump();
    let old = slot.live_current();
    let crashed_spec = slot.current().map(|c| c.spec.clone());
    let from_version = old.as_ref().map(|o| o.version()).unwrap_or_default();
    let policy = old.as_ref().map(|o| o.policy()).unwrap_or_default();
    let window = kind == SwapKind::Swap;
    let category = slot.category.clone();
    match (policy, old) {
        (SwapPolicy::DrainFirst, Some(old)) => drain_first(frame, slot, old, target, window, from_version, steps).await,
        (_, old) => {
            let spec = match steps.run("locate", locate(&target)).await {
                Ok(spec) => spec,
                Err(error) => return swap_result(&category, from_version, String::new(), steps, Err(error), old.is_some()),
            };
            let handoff = old.clone().map(Handoff::From).unwrap_or(Handoff::None);
            match admit(frame, slot, spec, handoff, &mut steps).await {
                Err(error) => {
                    let has_old = old.is_some();
                    if has_old {
                        steps.note("keep", true, "previous version keeps serving");
                    }
                    swap_result(&category, from_version, String::new(), steps, Err(error), has_old)
                }
                Ok(new) => {
                    let to_version = new.version();
                    let previous = old.as_ref().map(|o| o.spec.clone()).or(crashed_spec);
                    let committed = steps.run("commit", async { commit(frame, slot, &new, previous, window).map(|d| ((), d)) }).await;
                    if let Err(error) = committed {
                        new.retiring.store(true, Ordering::SeqCst);
                        new.kill();
                        return swap_result(&category, from_version, to_version, steps, Err(error), old.is_some());
                    }
                    if let Some(old) = old {
                        retire(frame, &old, &mut steps).await;
                    }
                    swap_result(&category, from_version, to_version, steps, Ok(()), false)
                }
            }
        }
    }
}

async fn drain_first(
    frame: &Arc<Frame>,
    slot: &Arc<Slot>,
    old: Arc<Instance>,
    target: ExecSpec,
    window: bool,
    from_version: String,
    mut steps: Steps,
) -> SwapResult {
    let category = slot.category.clone();
    // Validate the target before taking the old version out of service.
    let spec = match steps.run("locate", locate(&target)).await {
        Ok(spec) => spec,
        Err(error) => return swap_result(&category, from_version, String::new(), steps, Err(error), true),
    };
    steps.note("unroute", frame.clear_route(&category, &old.id), format!("{} no longer receives calls", old.id));
    old.retiring.store(true, Ordering::SeqCst);
    set_state(frame, &old, "draining");
    let _ = steps
        .run("drain", async {
            let params = to_value(&DrainParams { timeout_ms: DRAIN_TIMEOUT.as_millis() as u64 }).map_err(|e| e.to_string())?;
            let value = old.call(lifecycle::DRAIN, params, DRAIN_TIMEOUT + Duration::from_secs(5)).await?;
            let drained: DrainResult = serde_json::from_value(value).map_err(|e| e.to_string())?;
            Ok(((), format!("{} remaining", drained.remaining)))
        })
        .await;
    let exported = steps
        .run("export", async {
            let state = old.call(lifecycle::EXPORT_STATE, Value::Null, DRAIN_TIMEOUT).await?;
            let size = state.to_string().len();
            Ok((state, format!("{size} bytes of state")))
        })
        .await
        .unwrap_or(Value::Null);
    stop(frame, &old, &mut steps).await;
    match admit(frame, slot, spec, Handoff::Import(exported.clone()), &mut steps).await {
        Ok(new) => {
            let to_version = new.version();
            match steps.run("commit", async { commit(frame, slot, &new, Some(old.spec.clone()), window).map(|d| ((), d)) }).await {
                Ok(()) => swap_result(&category, from_version, to_version, steps, Ok(()), false),
                Err(error) => {
                    new.retiring.store(true, Ordering::SeqCst);
                    new.kill();
                    restore(frame, slot, &old, exported, &mut steps).await;
                    swap_result(&category, from_version, to_version, steps, Err(error), true)
                }
            }
        }
        Err(error) => {
            let restored = restore(frame, slot, &old, exported, &mut steps).await;
            swap_result(&category, from_version, String::new(), steps, Err(error), restored)
        }
    }
}

/// drain_first failure path: start the old version again with the state it exported.
async fn restore(frame: &Arc<Frame>, slot: &Arc<Slot>, old: &Arc<Instance>, state: Value, steps: &mut Steps) -> bool {
    steps.note("restore", true, format!("restarting the previous version {}", old.spec.version));
    match admit(frame, slot, old.spec.clone(), Handoff::Import(state), steps).await {
        Ok(restored) => {
            let committed = steps.run("commit", async { commit(frame, slot, &restored, None, false).map(|d| ((), d)) }).await;
            committed.is_ok()
        }
        Err(error) => {
            tracing::error!(category = %slot.category, %error, "could not restore the previous version; supervising restarts");
            slot.set_status(format!("restore failed: {error}"));
            tokio::spawn(ensure_running(frame.clone(), slot.clone(), old.spec.clone(), Duration::from_millis(0)));
            false
        }
    }
}

fn publish_swap(frame: &Frame, swap: &SwapResult) {
    if swap.ok {
        tracing::info!(category = %swap.category, from = %swap.from_version, to = %swap.to_version, "swap committed");
    } else {
        tracing::warn!(category = %swap.category, error = ?swap.error, rolled_back = swap.rolled_back, "swap failed");
    }
    if let Ok(value) = to_value(swap) {
        frame.publish(topics::PLUGIN_SWAPPED, &value);
    }
}

/// Run a swap of `category` to `target` (acquires the slot's operation lock).
pub async fn swap(frame: &Arc<Frame>, slot: &Arc<Slot>, target: ExecSpec, kind: SwapKind) -> SwapResult {
    let _op = slot.op.lock().await;
    let result = swap_locked(frame, slot, target, kind, Steps::new()).await;
    publish_swap(frame, &result);
    result
}

/// `frame.plugins.swap`.
pub async fn swap_request(frame: &Arc<Frame>, params: SwapParams) -> Result<SwapResult, RpcError> {
    let slot = frame
        .sup
        .slot(&params.category)
        .ok_or_else(|| RpcError::not_found(format!("`{}` is not an installed plugin category", params.category)))?;
    if frame.is_shutting_down() {
        return Err(RpcError::new(codes::UNAVAILABLE, "the frame is shutting down"));
    }
    if slot.is_disabled() {
        return Err(frame.disabled_error(&slot));
    }
    // A restart of the version the plugin runs (blue/green or drain-first, like any swap).
    let target = frame
        .units
        .current_spec(&slot.plugin())
        .ok_or_else(|| RpcError::not_found(format!("`{}` has no installed backend", params.category)))?;
    Ok(swap(frame, &slot, target, SwapKind::Swap).await)
}

// ----- crash supervision -----

fn backoff(crashes: usize) -> Duration {
    let exponent = crashes.saturating_sub(1).min(16) as u32;
    BACKOFF_BASE.saturating_mul(2_u32.saturating_pow(exponent)).min(BACKOFF_MAX)
}

async fn handle_crash(frame: Arc<Frame>, slot: Arc<Slot>, instance: Arc<Instance>, description: String) {
    let op = slot.op.lock().await;
    if frame.is_shutting_down() || slot.is_disabled() || slot.current().is_none_or(|current| current.id != instance.id) {
        return;
    }
    let crashes = slot.record_crash();
    slot.lock().restarts += 1;
    let window = {
        let mut inner = slot.lock();
        match inner.window.take() {
            Some(window) if window.instance == instance.id && Instant::now() < window.until => Some(window),
            _ => None,
        }
    };
    if let Some(window) = window {
        let mut steps = Steps::new();
        steps.note(
            "crash",
            false,
            format!("{} {description} within the {WATCH_WINDOW:?} watch window; rolling back", instance.id),
        );
        let to_version = window.rollback_to.version.clone();
        let mut rollback = swap_locked(&frame, &slot, window.rollback_to, SwapKind::Rollback, steps).await;
        let restored = rollback.ok;
        if restored {
            // Never offer the version that just crashed as the "previous" one.
            slot.lock().previous = None;
        }
        rollback.ok = false;
        rollback.rolled_back = restored;
        rollback.from_version = instance.version();
        if rollback.error.is_none() {
            rollback.error = Some(format!("{} {description} within the watch window", instance.version()));
        }
        publish_swap(&frame, &rollback);
        if restored {
            drop(op);
            // The plugin goes back as a whole (UI included).
            crate::units::backend_moved(&frame, &slot.plugin(), &to_version).await;
            return;
        }
    }
    drop(op);
    // Falling back to a different, known-good version needs no backoff.
    let (target, delay) = match fallback_target(&frame, &slot, &instance.spec) {
        Some(known) => (known, Duration::ZERO),
        None => (instance.spec.clone(), backoff(crashes)),
    };
    ensure_running(frame, slot, target, delay).await;
}

/// The last known good version, when the slot is crash-looping and it differs from `failing`.
fn fallback_target(frame: &Frame, slot: &Slot, failing: &ExecSpec) -> Option<ExecSpec> {
    if slot.recent_crashes() < CRASH_LOOP_COUNT {
        return None;
    }
    let known = frame.units.known_good_spec(&slot.plugin())?;
    (known.version != failing.version).then(|| {
        tracing::warn!(category = %slot.category, crashes = slot.recent_crashes(), version = %known.version, "crash loop: falling back to the last known good version");
        slot.lock().crashes.clear();
        known
    })
}

/// Keep trying to bring `target` up until it is committed, the frame shuts down, or another
/// operation (a swap) takes over the slot. Failures back off exponentially; a crash loop or a
/// deterministic rejection (failed probe, rejected hello) falls back to the last known good
/// version, and the plugin's UI follows it.
pub async fn ensure_running(frame: Arc<Frame>, slot: Arc<Slot>, mut target: ExecSpec, first_delay: Duration) {
    let mut delay = first_delay;
    let mut generation: Option<u64> = None;
    let mut last_error: Option<String> = None;
    loop {
        if !delay.is_zero() {
            slot.set_status(match &last_error {
                Some(error) => format!("restarting in {} ms; last attempt failed: {error}", delay.as_millis()),
                None => format!("restarting in {} ms", delay.as_millis()),
            });
            tokio::select! {
                _ = tokio::time::sleep(delay) => {}
                _ = frame.shutdown.cancelled() => return,
            }
        }
        if frame.is_shutting_down() {
            return;
        }
        let _op = slot.op.lock().await;
        if slot.is_disabled() {
            return;
        }
        match generation {
            Some(seen) if slot.generation() != seen => return,
            Some(_) => {}
            None => {
                if slot.live_current().is_some() {
                    return;
                }
            }
        }
        let mut steps = Steps::new();
        let outcome = async {
            let spec = steps.run("locate", locate(&target)).await?;
            let new = admit(&frame, &slot, spec, Handoff::None, &mut steps).await?;
            commit(&frame, &slot, &new, None, false)?;
            Ok::<Arc<Instance>, String>(new)
        }
        .await;
        match outcome {
            Ok(new) => {
                tracing::info!(category = %slot.category, instance = %new.id, version = %new.spec.version, "plugin is active");
                if frame.units.current_spec(&slot.plugin()).is_some_and(|current| current.version != new.spec.version) {
                    let (frame, plugin, version) = (frame.clone(), slot.plugin(), new.spec.version.clone());
                    tokio::spawn(async move { crate::units::backend_moved(&frame, &plugin, &version).await });
                }
                return;
            }
            Err(error) => {
                // A rejected hello or a failed probe will fail the same way again.
                let deterministic = steps.list.iter().any(|s| !s.ok && (s.step == "probe" || s.step == "hello" || s.step == "locate"));
                let crashes = slot.record_crash();
                slot.set_status(format!("failed: {error}"));
                last_error = Some(error.clone());
                tracing::warn!(category = %slot.category, %error, crashes, "plugin bring-up failed");
                let known = frame.units.known_good_spec(&slot.plugin()).filter(|known| known.version != target.version);
                match known {
                    Some(known) if deterministic || crashes >= CRASH_LOOP_COUNT => {
                        tracing::warn!(category = %slot.category, version = %known.version, "falling back to the last known good version");
                        slot.lock().crashes.clear();
                        target = known;
                        delay = Duration::ZERO;
                    }
                    _ => delay = backoff(crashes),
                }
                slot.lock().restarts += 1;
                generation = Some(slot.generation());
            }
        }
    }
}

/// Start every installed plugin in order. Each process is started after the previous one has
/// said hello (or failed to); probes run concurrently so one slow plugin never blocks the rest.
pub async fn start_all(frame: Arc<Frame>) {
    for slot in frame.sup.slots() {
        if frame.is_shutting_down() {
            return;
        }
        if slot.is_disabled() {
            tracing::info!(category = %slot.category, plugin = %slot.plugin(), "plugin is disabled; not starting it");
            continue;
        }
        let cfg = slot.config();
        let Some(target) = frame.units.current_spec(&slot.plugin()) else {
            slot.set_status("no installed backend");
            continue;
        };
        let hello_limit = Duration::from_millis(cfg.hello_timeout_ms);
        tokio::spawn(ensure_running(frame.clone(), slot.clone(), target, Duration::ZERO));
        if slot.category == crate::store::MEMORY {
            // The frame's records come from the memory plugin's store. They load beside the other
            // plugins and the UI, which start now; calls on the records wait until they are in.
            let load_frame = frame.clone();
            tokio::spawn(async move {
                load_records(&load_frame).await;
                load_frame.records_ready();
            });
        }
        // Wait until this plugin's process said hello (or failed) before starting the next one.
        let deadline = Instant::now() + hello_limit + Duration::from_secs(1);
        loop {
            let said_hello = {
                let inner = slot.lock();
                inner.others.iter().chain(inner.current.iter()).any(|i| i.hello.get().is_some())
            };
            let state = slot.describe_state();
            let failed = state.starts_with("failed") || state.starts_with("restarting");
            if said_hello || failed || Instant::now() > deadline || frame.is_shutting_down() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
}

/// Load the projects, sessions and slot state from the memory plugin's store.
async fn load_records(frame: &Arc<Frame>) {
    let load_frame = frame.clone();
    let loaded = tokio::spawn(async move { crate::store::load(&load_frame.memory).await.map(|(projects, sessions, ui)| load_frame.state.load(projects, sessions, ui)) });
    match tokio::time::timeout(Duration::from_secs(180), loaded).await {
        Ok(Ok(Ok(()))) => {
            tracing::info!(projects = frame.state.projects().len(), sessions = frame.state.sessions(None).len(), "records loaded from the store");
            // Journals an earlier version kept in the store's streams move into the frame's
            // files once; nothing else reads them.
            let ids: Vec<String> = frame.state.sessions(None).into_iter().map(|session| session.id).collect();
            match tokio::time::timeout(Duration::from_secs(180), frame.journal.migrate_streams(&frame.memory, &ids)).await {
                Ok((0, 0)) => {}
                Ok((moved, failed)) => tracing::info!(moved, failed, "session journals moved from the store's streams into files"),
                Err(_) => tracing::warn!("moving the session journals out of the store's streams did not finish within 180 s; the rest moves at the next start"),
            }
        }
        Ok(Ok(Err(error))) => tracing::error!(%error.message, "the store's records could not be read"),
        Ok(Err(error)) => tracing::error!(%error, "reading the store's records failed"),
        Err(_) => tracing::error!("the memory plugin did not answer within 180 s: starting without its records"),
    }
}

/// Stop every plugin in reverse start order (frame shutdown).
pub async fn stop_all(frame: &Arc<Frame>) {
    stop_slots(frame, |_| true).await;
}

/// Stop every plugin but the one serving `category`, in reverse start order.
pub async fn stop_all_except(frame: &Arc<Frame>, category: &str) {
    stop_slots(frame, |slot| slot.category != category).await;
}

async fn stop_slots(frame: &Arc<Frame>, chosen: impl Fn(&Slot) -> bool) {
    for slot in frame.sup.slots().iter().rev().filter(|slot| chosen(slot)) {
        let instances: Vec<Arc<Instance>> = {
            let inner = slot.lock();
            inner.current.iter().chain(inner.others.iter()).filter(|i| !i.has_exited()).cloned().collect()
        };
        for instance in instances {
            let mut steps = Steps::new();
            frame.clear_route(&slot.category, &instance.id);
            stop(frame, &instance, &mut steps).await;
            tracing::info!(instance = %instance.id, steps = ?steps.list.iter().map(|s| format!("{}:{}:{}ms", s.step, s.ok, s.ms)).collect::<Vec<_>>(), "plugin stopped");
        }
    }
}

/// Kill every plugin process still alive (after `stop_all`, or on a forced exit). Covers instances
/// a concurrent operation started while the frame was stopping.
pub async fn kill_remaining(frame: &Arc<Frame>) {
    let mut live: Vec<Arc<Instance>> = Vec::new();
    for slot in &frame.sup.slots() {
        let inner = slot.lock();
        live.extend(inner.current.iter().chain(inner.others.iter()).filter(|i| !i.has_exited()).cloned());
    }
    if let Ok(pending) = frame.sup.pending.lock() {
        live.extend(pending.values().filter(|i| !i.has_exited()).cloned());
    }
    for instance in &live {
        instance.retiring.store(true, Ordering::SeqCst);
        tracing::warn!(instance = %instance.id, "killing a plugin process that is still running");
        instance.kill();
    }
    for instance in &live {
        instance.wait_exit(EXIT_GRACE).await;
    }
}

/// Disable a slot: unroute it, stop every instance and keep it stopped (no restarts, no swaps)
/// until [`enable`]. Returns what was done.
pub async fn disable(frame: &Arc<Frame>, slot: &Arc<Slot>) -> Vec<SwapStep> {
    let _op = slot.op.lock().await;
    let mut steps = Steps::new();
    slot.disabled.store(true, Ordering::SeqCst);
    slot.bump();
    let instances: Vec<Arc<Instance>> = {
        let inner = slot.lock();
        inner.current.iter().chain(inner.others.iter()).filter(|i| !i.has_exited()).cloned().collect()
    };
    for instance in &instances {
        steps.note("unroute", frame.clear_route(&slot.category, &instance.id), format!("{} no longer receives calls", instance.id));
    }
    for instance in &instances {
        retire(frame, instance, &mut steps).await;
    }
    {
        let mut inner = slot.lock();
        inner.window = None;
        inner.crashes.clear();
        inner.status = "disabled".into();
    }
    if instances.is_empty() {
        steps.note("stop", true, format!("`{}` was not running", slot.category));
    }
    frame.publish_slot_state(slot);
    steps.list
}

/// Enable a disabled slot and bring `target` up (in the background, like a first start).
pub async fn enable(frame: &Arc<Frame>, slot: &Arc<Slot>, target: ExecSpec) -> Vec<SwapStep> {
    let mut steps = Steps::new();
    {
        let _op = slot.op.lock().await;
        slot.disabled.store(false, Ordering::SeqCst);
        slot.bump();
        slot.set_status("starting (enabled)");
    }
    steps.note("start", true, format!("starting `{}` {}", slot.category, target.version));
    tokio::spawn(ensure_running(frame.clone(), slot.clone(), target, Duration::ZERO));
    frame.publish_slot_state(slot);
    steps.list
}

/// Category requests the slot's serving instance runs now (its `plugin.health`), when one serves.
pub async fn in_flight(slot: &Slot) -> Option<usize> {
    let instance = slot.live_current()?;
    let value = instance.call(lifecycle::HEALTH, Value::Null, Duration::from_secs(5)).await.ok()?;
    serde_json::from_value::<Health>(value).ok().map(|health| health.in_flight)
}

/// Swap a slot to `target` as part of a whole-plugin operation (install, plugin rollback).
pub async fn swap_for_package(frame: &Arc<Frame>, slot: &Arc<Slot>, target: ExecSpec, note: &str) -> SwapResult {
    let _op = slot.op.lock().await;
    let mut steps = Steps::new();
    steps.note("package", true, note);
    let result = swap_locked(frame, slot, target, SwapKind::Swap, steps).await;
    publish_swap(frame, &result);
    result
}
