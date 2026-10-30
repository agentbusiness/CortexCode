//! The notifier: events in, notifications out.
//!
//! Event -> [`derive`] trigger -> names looked up through the frame -> notification -> preference
//! and focus gate -> history (dedup by key) -> delivery through the CortexCode notification app
//! (clickable, shows the CortexCode icon) when macOS allows it; otherwise the notification stays in
//! CortexCode's own list. The app asks macOS for permission when it starts.
//!
//! The notifications live in the app's graph (WORKSPACE layer); the newest [`history::CAPACITY`]
//! are held in memory for dedup and the list, rebuilt from the graph when the plugin connects:
//!
//! ```text
//! (:Notification {key: id, kind, title, body, dedup_key, subtitle, actions, session_id, project_id,
//!                 ts_ms, delivery, detail, source, method, updates, updated_ms, activated_ms,
//!                 activated_action, target_<name> for each scalar member of target})
//!     -[:ABOUT]->(:Session)      when it has a session
//!     -[:ABOUT]->(:Project)      else when it has a project
//!     -[:ON]->(:Machine)         else
//! stream `notifications`         every notification as posted (a dedup update too)
//! ```
//!
//! An item that falls out of the newest [`history::CAPACITY`] is retired. The history kept before as
//! one setting (`notify.history`) and before that in `history.json` is moved into the graph once,
//! then deleted. Preferences stay settings (section `notify`).
//!
//! Clicks made while no plugin listens travel on the memory queue `notify.activations`
//! ([`ACTIVATIONS_QUEUE`]). The separate notification app (Swift) runs without the app and cannot
//! reach the queue, so it appends them to `activations.jsonl` (plumbing, not a record). At
//! activation that spool is published to the queue and removed only once the queue took it, so a
//! click survives the memory plugin being down ([`Notifier::drain_spool`]). The queue is consumed
//! (consumer `notify`) only after the history is loaded, each message acked after it was handled;
//! one naming a notification the loaded history does not know is acked with a log line.

use crate::derive::{self, Context, Trigger};
use crate::graph::{self, Graph};
use crate::history::{self, History, Recorded, same_content};
use crate::native::NativeDelivery;
use crate::native::helper::NativeActivation;
use crate::prefs::{self, Prefs, kind_info};
use cc_protocol::contracts::chatflow::{kinds as chatflow_kinds, verbs as chatflow_verbs};
use cc_protocol::contracts::frame::{JournalEntry, JournalReadResult, Project, Session, SessionKind, flows, kinds as entry_kinds, methods as frame_methods};
use cc_protocol::contracts::lifecycle::{ProbeCheck, ProbeReport};
use cc_protocol::contracts::memory::methods as memory_methods;
use cc_protocol::contracts::notify::{
    Activate, ActivatedParams, ActivatedResult, DelivererInfo, DelivererParams, DelivererResult, FocusState,
    HistoryParams, HistoryResult, Notification, Posted, PostedEvent, PrefsResult, PrefsSetParams, SpooledActivation, StatusResult, delivery, kinds,
    methods, topics, ACTIVATIONS_QUEUE,
};
use cc_protocol::contracts::memory::{QueueTakeResult, methods as queue_methods};
use cc_protocol::contracts::{agents, now_ms, tools};
use cc_protocol::{BoxFuture, Ctx, RpcError, codes, parse_params, to_value};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

/// The entry a finished run's notification points at (structural: entry kinds and fields): for
/// `done`, the run's last non-empty answer; for `failed`, its last `error` entry, else its last
/// failed tool result. `None`: the status entry itself.
pub fn focus_of_run(entries: &[JournalEntry], state: &str, run_id: Option<&str>) -> Option<u64> {
    // With a run id, only that run's entries count (an answer of another run is not this one's).
    let in_run = |entry: &&JournalEntry| run_id.is_none_or(|run| entry.data.get("run_id").and_then(Value::as_str) == Some(run));
    // The run starts after the last user message (a foreground turn) when run ids are absent.
    let start = entries.iter().rposition(|entry| entry.kind == entry_kinds::USER_MESSAGE).unwrap_or(0);
    let run = &entries[start..];
    match state {
        "done" => run
            .iter()
            .filter(in_run)
            .rfind(|entry| entry.kind == entry_kinds::ASSISTANT_MESSAGE && entry.data.get("text").and_then(Value::as_str).is_some_and(|t| !t.trim().is_empty()))
            .map(|entry| entry.seq),
        _ => run
            .iter()
            .filter(in_run)
            .rfind(|entry| entry.kind == entry_kinds::ERROR)
            .or_else(|| run.iter().filter(in_run).rfind(|entry| entry.kind == entry_kinds::TOOL_RESULT && entry.data.get("ok").and_then(Value::as_bool) == Some(false)))
            .map(|entry| entry.seq),
    }
}

/// The entry that blocked an agent (structural): a pending approval, else its last error, else its
/// last failed / blocked / cancelled status, else its last entry.
pub fn blocking_entry(entries: &[JournalEntry]) -> Option<u64> {
    let final_status = |entry: &&JournalEntry| {
        entry.kind == entry_kinds::STATUS && matches!(entry.data.get("state").and_then(Value::as_str), Some("failed" | "blocked" | "cancelled" | "stopped"))
    };
    entries
        .iter()
        .rfind(|entry| entry.kind == chatflow_kinds::APPROVAL && entry.data.get("state").and_then(Value::as_str) == Some("pending"))
        .or_else(|| entries.iter().rfind(|entry| entry.kind == entry_kinds::ERROR))
        .or_else(|| entries.iter().rfind(final_status))
        .or_else(|| entries.last())
        .map(|entry| entry.seq)
}

/// Methods this plugin serves.
/// How far back the place a notification is about is looked for (entries before the trigger).
const FOCUS_LOOKBACK: u64 = 400;

/// `Posted.method` of a notification shown in the CortexCode window (a mark on its session in the
/// session list) instead of macOS, because the window was in front. Delivery state `ui`; the mark
/// stands while `activated_ms` is empty and is cleared (`activated_action` = `viewed`) once the
/// user opens the session. A notification macOS did not show (`failed`) marks its session the same
/// way (see `history::marks_session`).
pub const IN_APP_METHOD: &str = "window";
const IN_APP_DETAIL: &str = "CortexCode was in front: marked on its session in the list instead of a macOS notification";

pub const PROVIDES: &[&str] = &[
    methods::SEND,
    methods::HISTORY,
    methods::ACTIVATED,
    methods::DELIVERER,
    methods::PREFS_GET,
    methods::PREFS_SET,
    methods::STATUS,
];

/// Event topic prefixes this plugin subscribes to.
pub const SUBSCRIBES: &[&str] = &["journal.", "agents.changed", "plugin.swapped", "plugin.state", "tools.job", "schedule.ran", topics::FOCUS, "memory.settings.changed"];

/// Calls into the rest of the system (the frame in production, a table in unit tests).
pub trait Bus: Send + Sync + 'static {
    fn call(&self, method: &str, params: Value) -> BoxFuture<'static, Result<Value, RpcError>>;
    fn publish(&self, topic: &str, data: Value);
}

/// The frame connection as a [`Bus`].
pub struct CtxBus(pub Ctx);

impl Bus for CtxBus {
    fn call(&self, method: &str, params: Value) -> BoxFuture<'static, Result<Value, RpcError>> {
        let ctx = self.0.clone();
        let method = method.to_owned();
        Box::pin(async move { ctx.call(&method, params).await })
    }

    fn publish(&self, topic: &str, data: Value) {
        self.0.publish(topic, data);
    }
}

/// `[plugin.config]` of the notify plugin.
#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    /// The notification app bundle (default: beside the executable).
    pub helper_app: Option<PathBuf>,
    /// Ask macOS for permission for the notification app at startup when it is undecided.
    pub request_authorization: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self { helper_app: None, request_authorization: true }
    }
}

impl Config {
    pub fn from_value(value: &Value) -> Self {
        let mut config = Config::default();
        if let Some(path) = value.get("helper_app").and_then(Value::as_str).filter(|p| !p.is_empty()) {
            config.helper_app = Some(PathBuf::from(path));
        }
        if let Some(flag) = value.get("request_authorization").and_then(Value::as_bool) {
            config.request_authorization = flag;
        }
        config
    }
}

#[derive(Debug, Clone)]
struct Lease {
    id: String,
    kind: String,
    ttl_ms: u64,
    renewed: Instant,
    since: Instant,
}

impl Lease {
    fn live(&self) -> bool {
        self.renewed.elapsed() < Duration::from_millis(self.ttl_ms)
    }
}

#[derive(Debug, Clone)]
enum Source {
    Settings,
    Defaults(String),
}

struct PrefsState {
    prefs: Prefs,
    source: Source,
    problems: Vec<String>,
    loaded: Option<Instant>,
}

impl PrefsState {
    /// What every notification's detail says about the preferences, when anything is off.
    fn note(&self) -> Option<String> {
        match &self.source {
            Source::Defaults(reason) => Some(format!("preferences: built-in defaults ({reason})")),
            Source::Settings if !self.problems.is_empty() => Some(format!("ignored settings: {}", self.problems.join("; "))),
            Source::Settings => None,
        }
    }

    fn source_name(&self) -> (&'static str, String) {
        match &self.source {
            Source::Settings => ("settings", self.problems.join("; ")),
            Source::Defaults(reason) => ("defaults", reason.clone()),
        }
    }
}

/// How long preferences read from the settings database are trusted without a change event.
const PREFS_FRESH: Duration = Duration::from_secs(60);
/// How soon an unavailable settings database is tried again.
const PREFS_RETRY: Duration = Duration::from_secs(5);
/// Lookups of names (session, project, task) must not hold up the event stream for long.
const LOOKUP_TIMEOUT: Duration = Duration::from_secs(5);
/// A native click whose UI was not connected is re-sent to the next deliverer within this time.
const PENDING_ACTIVATION: Duration = Duration::from_secs(120);

enum Plan {
    Deliver { replaces: Option<String> },
    Suppress(String),
    Keep,
}

/// How long a stop waits for the graph to take the last changes.
const FLUSH_WAIT: Duration = Duration::from_secs(5);
/// The graph's label of a notification, and the stream of posts.
const NOTIFICATION: &str = "Notification";
const POSTS: &str = "notifications";
/// The history's setting of earlier versions, and the file before it (moved into the graph once).
const OLD_SETTING: &str = "notify.history";
const OLD_FILE: &str = "history.json";
/// The notification app's spool of clicks, and the suffix of a part taken for publishing.
const SPOOL: &str = "activations.jsonl";
const TAKEN: &str = ".draining";
/// Who consumes [`ACTIVATIONS_QUEUE`], and how many messages at a time.
const CONSUMER: &str = "notify";
const TAKE_MAX: u32 = 50;
/// Between two publish attempts while the memory plugin does not serve.
const SPOOL_RETRY: Duration = Duration::from_secs(5);

#[derive(Serialize, Deserialize)]
struct Exported {
    history: History,
    #[serde(default)]
    focus: Option<FocusState>,
    #[serde(default)]
    focus_age_ms: u64,
    #[serde(default)]
    lease: Option<(String, String, u64, u64)>,
}

pub struct Notifier {
    bus: OnceLock<Arc<dyn Bus>>,
    native: Arc<dyn NativeDelivery>,
    history: Mutex<History>,
    prefs: tokio::sync::Mutex<PrefsState>,
    focus: Mutex<Option<(FocusState, Instant)>>,
    lease: Mutex<Option<Lease>>,
    delivering: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    pending_activation: Mutex<Option<(Activate, Instant)>>,
    data_dir: PathBuf,
    /// Where the notifications are kept.
    graph: Graph,
    /// Per item, the target members its node keeps (`target_<name>`), so a changed target removes
    /// the ones gone.
    targets: Mutex<HashMap<String, Vec<String>>>,
    /// The history was read from the graph: queued clicks can be matched to their items.
    loaded: AtomicBool,
    /// One spool publisher and one queue consumer at a time.
    spooling: tokio::sync::Mutex<()>,
    consuming: tokio::sync::Mutex<()>,
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn join_notes(parts: &[String]) -> String {
    parts.iter().filter(|p| !p.is_empty()).cloned().collect::<Vec<_>>().join("; ")
}

impl Notifier {
    /// Create the notifier. Must run inside a Tokio runtime (it starts the history writer and the
    /// native activation listener).
    pub fn new(native: Arc<dyn NativeDelivery>, activations: mpsc::UnboundedReceiver<NativeActivation>, data_dir: PathBuf) -> Arc<Self> {
        let notifier = Arc::new(Self {
            bus: OnceLock::new(),
            native,
            history: Mutex::new(History::default()),
            prefs: tokio::sync::Mutex::new(PrefsState { prefs: Prefs::default(), source: Source::Defaults("not loaded yet".into()), problems: Vec::new(), loaded: None }),
            focus: Mutex::new(None),
            lease: Mutex::new(None),
            delivering: Mutex::new(HashMap::new()),
            pending_activation: Mutex::new(None),
            data_dir: data_dir.clone(),
            graph: Graph::new(),
            targets: Mutex::new(HashMap::new()),
            loaded: AtomicBool::new(false),
            spooling: tokio::sync::Mutex::new(()),
            consuming: tokio::sync::Mutex::new(()),
        });
        let weak = Arc::downgrade(&notifier);
        let mut activations = activations;
        tokio::spawn(async move {
            while let Some(activation) = activations.recv().await {
                let Some(notifier) = weak.upgrade() else { return };
                let params = ActivatedParams { id: activation.id, action: activation.action, via: Some("helper".into()) };
                if let Err(error) = notifier.activated(params).await {
                    tracing::warn!(%error, "a click on a notification could not be routed");
                }
            }
        });
        notifier
    }

    /// Connect to the system (once), then move the old history into the graph (once) and read
    /// the notifications back. Items posted meanwhile stay as they are.
    pub fn attach(self: &Arc<Self>, bus: Arc<dyn Bus>) {
        if self.bus.set(bus.clone()).is_err() {
            return;
        }
        let caller = bus.clone();
        self.graph.start(Arc::new(move |method: &str, params: Value| caller.call(method, params)));
        let notifier = self.clone();
        tokio::spawn(async move {
            notifier.migrate().await;
            notifier.load().await;
        });
    }

    /// The history of earlier versions (the setting, and the file before it) into the graph,
    /// once; both are deleted when the graph took it.
    async fn migrate(&self) {
        let file = self.data_dir.join(OLD_FILE);
        let mut old = Vec::new();
        let from_setting = match self.graph.ask::<Value>(memory_methods::SETTINGS_GET, json!({ "path": OLD_SETTING })).await {
            Ok(setting) => {
                match setting.get("value").cloned().map(serde_json::from_value::<History>) {
                    Some(Ok(history)) => old.extend(history.items().cloned()),
                    _ => tracing::warn!("the old notification history setting is unreadable; it is deleted"),
                }
                true
            }
            Err(error) if error.code == codes::NOT_FOUND => false,
            Err(error) => {
                tracing::warn!(error = %error.message, "the old notification history was not read; it is moved at the next start");
                return;
            }
        };
        let from_file = match std::fs::read(&file) {
            Ok(bytes) => {
                match serde_json::from_slice::<History>(&bytes) {
                    Ok(history) => old.extend(history.items().cloned()),
                    Err(error) => tracing::warn!(%error, file = %file.display(), "the old notification history file is unreadable; it is deleted"),
                }
                true
            }
            Err(_) => false,
        };
        if !from_setting && !from_file {
            return;
        }
        let mut history = History::from_items(Vec::new());
        history.merge(History::from_items(old));
        history.drain_evicted();
        for item in history.items() {
            self.write_item(item);
        }
        self.graph.append(POSTS, history.items().filter_map(|item| to_value(item).ok()).collect(), None);
        if !self.graph.flush().await {
            tracing::warn!("the old notification history was not moved into the graph; it is kept and tried again at the next start");
            return;
        }
        if from_setting && let Err(error) = self.graph.ask::<Value>(memory_methods::SETTINGS_DELETE, json!({ "path": OLD_SETTING })).await {
            tracing::warn!(error = %error.message, "the old notification history setting was moved into the graph but not deleted");
        }
        if from_file {
            let _ = std::fs::remove_file(&file);
        }
        tracing::info!(items = history.len(), "the notification history moved into the graph");
    }

    /// The notifications the graph keeps, into the list (and dedup), then the clicks queued for
    /// them. Those past the capacity are retired.
    async fn load(self: &Arc<Self>) {
        self.graph.flush().await;
        let cypher = format!("MATCH (n:{NOTIFICATION}) WHERE n.ended_ms IS NULL RETURN n ORDER BY n.ts_ms DESC");
        let nodes = match self.graph.nodes(&cypher, serde_json::Map::new(), 5000).await {
            Ok(nodes) => nodes,
            Err(error) => {
                tracing::warn!(error = %error.message, "the notifications were not read from the graph; new ones are still kept");
                return;
            }
        };
        let items: Vec<_> = nodes.iter().filter_map(history::from_props).collect();
        {
            let mut targets = lock(&self.targets);
            for item in &items {
                targets.entry(item.id.clone()).or_insert_with(|| history::target_names(&item.notification.target));
            }
        }
        lock(&self.history).merge(History::from_items(items));
        self.retire_evicted();
        self.loaded.store(true, Ordering::SeqCst);
        self.consume().await;
    }

    /// Write an item's node with its edge.
    fn write_item(&self, item: &Posted) {
        let names = history::target_names(&item.notification.target);
        let stale: Vec<String> = {
            let mut targets = lock(&self.targets);
            let before = targets.insert(item.id.clone(), names.clone()).unwrap_or_default();
            before.into_iter().filter(|name| !names.contains(name)).collect()
        };
        let n = &item.notification;
        let node = vec![graph::node(NOTIFICATION, &item.id, history::props(item, &stale))];
        let on_machine = vec![graph::edge((NOTIFICATION, &item.id), "ON", graph::machine())];
        let about = match (&n.session_id, &n.project_id) {
            (Some(session), _) => graph::at("Session", session),
            (None, Some(project)) => graph::at("Project", project),
            (None, None) => return self.graph.put(node, on_machine),
        };
        // A session or project the graph does not know (a deleted one): the machine holds it.
        self.graph.put_or(node, vec![graph::edge((NOTIFICATION, &item.id), "ABOUT", about)], on_machine);
    }

    /// Retire the items that fell out of the list.
    fn retire_evicted(&self) {
        let evicted = lock(&self.history).drain_evicted();
        if evicted.is_empty() {
            return;
        }
        let mut targets = lock(&self.targets);
        for id in &evicted {
            targets.remove(id);
        }
        drop(targets);
        self.graph.retire(evicted.iter().map(|id| graph::at(NOTIFICATION, id)).collect(), "capacity");
    }

    fn publish(&self, topic: &str, data: Value) {
        if let Some(bus) = self.bus.get() {
            bus.publish(topic, data);
        }
    }

    async fn call(&self, method: &str, params: Value) -> Result<Value, RpcError> {
        let bus = self.bus.get().ok_or_else(|| RpcError::new(codes::UNAVAILABLE, "not connected to the frame yet"))?.clone();
        match tokio::time::timeout(LOOKUP_TIMEOUT, bus.call(method, params)).await {
            Ok(outcome) => outcome,
            Err(_) => Err(RpcError::new(codes::TIMEOUT, format!("{method} did not answer within {LOOKUP_TIMEOUT:?}"))),
        }
    }

    /// An item changed: its node is written, listeners are told.
    fn changed(&self, posted: &Posted) {
        self.write_item(posted);
        self.retire_evicted();
        if let Ok(value) = to_value(posted) {
            self.publish(topics::CHANGED, value);
        }
    }

    // ----- requests -----

    pub async fn handle(self: &Arc<Self>, method: &str, params: Value) -> Result<Value, RpcError> {
        match method {
            methods::SEND => {
                let notification: Notification = parse_params(params)?;
                if notification.kind.trim().is_empty() || notification.title.trim().is_empty() {
                    return Err(RpcError::invalid_params("a notification needs a kind and a title"));
                }
                to_value(&self.post(notification, "notify.send", true).await)
            }
            methods::HISTORY => {
                let params: HistoryParams = if params.is_null() { HistoryParams::default() } else { parse_params(params)? };
                let limit = params.limit.map(|l| l as usize).unwrap_or(50);
                to_value(&HistoryResult { items: lock(&self.history).list(limit) })
            }
            methods::ACTIVATED => to_value(&self.activated(parse_params(params)?).await?),
            methods::DELIVERER => to_value(&self.deliverer(parse_params(params)?)),
            methods::PREFS_GET => to_value(&self.prefs_result(false).await),
            methods::PREFS_SET => to_value(&self.prefs_set(parse_params(if params.is_null() { json!({}) } else { params })?).await?),
            methods::STATUS => to_value(&self.status().await),
            other => Err(RpcError::unknown_method(other)),
        }
    }

    /// Post a notification through preferences, focus, dedup and delivery. With `wait`, returns once
    /// delivery has finished; otherwise delivery continues in the background.
    pub async fn post(self: &Arc<Self>, notification: Notification, source: &str, wait: bool) -> Posted {
        let (prefs, note) = self.prefs_now().await;
        let suppression = self.suppression(&notification, &prefs);
        let now = now_ms();
        let (posted, plan) = {
            let mut history = lock(&self.history);
            let recorded = history.record(notification, source, now, prefs.dedup_window_ms);
            let id = recorded.posted().id.clone();
            let plan = match &recorded {
                Recorded::New(_) => match suppression {
                    Some(reason) => Plan::Suppress(reason),
                    None => Plan::Deliver { replaces: None },
                },
                Recorded::Updated { posted, previous } => {
                    // Out already: shown by macOS, on its way, or listed in CortexCode without macOS's
                    // permission (a repeat stays one item there too).
                    let shown = [delivery::OS, delivery::UI, delivery::PENDING, delivery::FAILED].contains(&previous.delivery.as_str());
                    match (suppression, shown) {
                        (Some(reason), false) => Plan::Suppress(reason),
                        // Already on screen: a repeat stays one notification.
                        (Some(_), true) => Plan::Keep,
                        (None, true) if same_content(&previous.notification, &posted.notification) => Plan::Keep,
                        (None, true) if previous.delivery == delivery::PENDING => Plan::Deliver { replaces: None },
                        (None, true) => Plan::Deliver { replaces: Some(previous.method.clone()) },
                        (None, false) => Plan::Deliver { replaces: None },
                    }
                }
            };
            let posted = match &plan {
                Plan::Suppress(reason) => {
                    let detail = join_notes(&[reason.clone(), note.clone().unwrap_or_default()]);
                    history.set_delivery(&id, delivery::SUPPRESSED, "", &detail)
                }
                Plan::Deliver { .. } => history.set_delivery(&id, delivery::PENDING, "", ""),
                Plan::Keep => history.get(&id).cloned(),
            };
            (posted.unwrap_or_else(|| recorded.posted().clone()), plan)
        };
        self.changed(&posted);
        if let Ok(value) = to_value(&posted) {
            self.graph.append(POSTS, vec![value], None);
        }
        match plan {
            Plan::Deliver { replaces } => {
                let work = self.clone().deliver(posted.clone(), replaces, note);
                if wait {
                    work.await
                } else {
                    tokio::spawn(work);
                    posted
                }
            }
            Plan::Suppress(_) | Plan::Keep => posted,
        }
    }

    /// Why a notification must not be shown now, if it must not.
    fn suppression(&self, notification: &Notification, prefs: &Prefs) -> Option<String> {
        if !prefs.enabled(&notification.kind) {
            let label = kind_info(&notification.kind).map(|info| info.label).unwrap_or(notification.kind.as_str());
            return Some(format!("“{label}” notifications are off (setting {}.kinds.{} = false)", prefs::SECTION, notification.kind));
        }
        if prefs.only_when_away
            && let Some(session) = notification.session_id.as_deref()
            && self.looking_at(session)
        {
            return Some("you were looking at this session (only when not looking is on)".into());
        }
        None
    }

    /// Whether the UI says the user is looking at `session` right now (fresh, focused, visible).
    pub fn looking_at(&self, session: &str) -> bool {
        lock(&self.focus).as_ref().is_some_and(|(focus, at)| {
            at.elapsed() < Duration::from_millis(focus.ttl_ms) && focus.window_focused && focus.visible && focus.session_id.as_deref() == Some(session)
        })
    }

    /// Whether a CortexCode window is in front right now: the latest focus report is fresh and says
    /// the window has focus and is on screen. No report (no window), a stale one, a hidden, minimized
    /// or unfocused window: not in front.
    pub fn app_in_front(&self) -> bool {
        lock(&self.focus)
            .as_ref()
            .is_some_and(|(focus, at)| at.elapsed() < Duration::from_millis(focus.ttl_ms) && focus.window_focused && focus.visible)
    }

    fn live_lease(&self) -> Option<Lease> {
        lock(&self.lease).clone().filter(Lease::live)
    }

    fn id_lock(&self, id: &str) -> Arc<tokio::sync::Mutex<()>> {
        let mut map = lock(&self.delivering);
        map.retain(|_, gate| Arc::strong_count(gate) > 1);
        map.entry(id.to_owned()).or_default().clone()
    }

    async fn deliver(self: Arc<Self>, posted: Posted, replaces: Option<String>, prefs_note: Option<String>) -> Posted {
        let gate = self.id_lock(&posted.id);
        let _serial = gate.lock().await;
        let id = posted.id.clone();
        let mut notes = Vec::new();
        let n = &posted.notification;
        // 0. A CortexCode window is in front (decided now, at delivery): macOS is not asked. The
        // notification marks its session in the session list until the user opens that session.
        // Without a session there is no row to mark, so it goes to macOS as below.
        if let Some(session) = n.session_id.clone()
            && self.app_in_front()
        {
            let event = PostedEvent { id: id.clone(), notification: posted.notification.clone(), ts_ms: posted.ts_ms, update: replaces.is_some(), deliver_to: None };
            if let Ok(value) = to_value(&event) {
                self.publish(topics::POSTED, value);
            }
            let detail = join_notes(&[IN_APP_DETAIL.to_owned(), prefs_note.unwrap_or_default()]);
            let marked = lock(&self.history).mark_in_app(&id, IN_APP_METHOD, &detail);
            let result = match marked {
                Some(posted) => {
                    self.changed(&posted);
                    posted
                }
                None => self.finish(&id, delivery::UI, IN_APP_METHOD, &detail),
            };
            // On screen already: nothing left to point at.
            if self.looking_at(&session) {
                self.viewed(&session);
                return lock(&self.history).get(&id).cloned().unwrap_or(result);
            }
            return result;
        }
        let request = crate::native::NativeRequest {
            id: id.clone(),
            title: n.title.clone(),
            subtitle: n.subtitle.clone().unwrap_or_default(),
            body: n.body.clone(),
            thread: n.project_id.clone().unwrap_or_else(|| "cortexcode".into()),
            actions: n.actions.clone(),
            replaces: replaces.clone().filter(|method| method == "helper"),
        };
        // 1. The CortexCode notification app, when macOS allows it: it carries the icon and its
        // clicks (and actions) come back. A UI deliverer then only lists the notification.
        match self.native.deliver_app(request.clone()).await {
            Ok(Some(outcome)) => {
                let event = PostedEvent { id: id.clone(), notification: posted.notification.clone(), ts_ms: posted.ts_ms, update: replaces.is_some(), deliver_to: None };
                if let Ok(value) = to_value(&event) {
                    self.publish(topics::POSTED, value);
                }
                return self.finish(&id, delivery::OS, &outcome.method, &join_notes(&[outcome.detail, prefs_note.unwrap_or_default()]));
            }
            Ok(None) => notes.push(self.native.unavailable_reason().await),
            Err(note) => notes.push(note),
        }
        // Only the CortexCode notification app posts to macOS (owner): without macOS's permission
        // the notification stays in CortexCode's own list.
        let event = PostedEvent { id: id.clone(), notification: posted.notification.clone(), ts_ms: posted.ts_ms, update: replaces.is_some(), deliver_to: None };
        if let Ok(value) = to_value(&event) {
            self.publish(topics::POSTED, value);
        }
        notes.push(prefs_note.unwrap_or_default());
        self.finish(&id, delivery::FAILED, "", &join_notes(&notes))
    }

    fn finish(&self, id: &str, state: &str, method: &str, detail: &str) -> Posted {
        let updated = lock(&self.history).set_delivery(id, state, method, detail);
        match updated {
            Some(posted) => {
                self.changed(&posted);
                posted
            }
            // The item fell out of the bounded history meanwhile; report what happened anyway.
            None => Posted {
                id: id.to_owned(),
                notification: Notification {
                    kind: kinds::INFO.into(),
                    title: String::new(),
                    body: String::new(),
                    key: None,
                    project_id: None,
                    session_id: None,
                    target: Value::Null,
                    actions: vec![],
                    subtitle: None,
                },
                ts_ms: now_ms(),
                delivery: state.into(),
                detail: detail.into(),
                source: String::new(),
                method: method.into(),
                updates: 0,
                updated_ms: now_ms(),
                activated_ms: None,
                activated_action: None,
            },
        }
    }

    fn deliverer(&self, params: DelivererParams) -> DelivererResult {
        if params.deliverer_id.is_empty() {
            return DelivererResult { active: false, lease_ms: 0 };
        }
        if params.detach {
            let mut lease = lock(&self.lease);
            if lease.as_ref().is_some_and(|l| l.id == params.deliverer_id) {
                *lease = None;
                *lock(&self.focus) = None;
            }
            return DelivererResult { active: false, lease_ms: 0 };
        }
        let ttl_ms = params.ttl_ms.clamp(1_000, 600_000);
        let fresh = {
            let mut lease = lock(&self.lease);
            let fresh = !lease.as_ref().is_some_and(|l| l.id == params.deliverer_id && l.live());
            let since = lease.as_ref().filter(|l| !fresh && l.id == params.deliverer_id).map(|l| l.since).unwrap_or_else(Instant::now);
            *lease = Some(Lease { id: params.deliverer_id.clone(), kind: if params.kind.is_empty() { "ui".into() } else { params.kind.clone() }, ttl_ms, renewed: Instant::now(), since });
            fresh
        };
        if fresh {
            tracing::info!(deliverer = %params.deliverer_id, kind = %params.kind, "a UI deliverer attached");
            let pending = lock(&self.pending_activation).take();
            if let Some((activate, at)) = pending
                && at.elapsed() < PENDING_ACTIVATION
                && let Ok(value) = to_value(&activate)
            {
                self.publish(topics::ACTIVATE, value);
            }
        }
        DelivererResult { active: true, lease_ms: ttl_ms }
    }

    fn set_focus(&self, data: Value) {
        match serde_json::from_value::<FocusState>(data) {
            Ok(focus) => {
                let shown = focus.session_id.clone().filter(|_| focus.window_focused && focus.visible);
                *lock(&self.focus) = Some((focus, Instant::now()));
                // The user opened the session: its in-window marks are seen.
                if let Some(session) = shown {
                    self.viewed(&session);
                }
            }
            Err(error) => tracing::debug!(%error, "unreadable focus state"),
        }
    }

    /// Clear the session-list marks of `session` (in-window notifications not yet seen).
    fn viewed(&self, session: &str) {
        let seen = lock(&self.history).viewed(session, IN_APP_METHOD, now_ms());
        for posted in &seen {
            self.changed(posted);
        }
    }

    /// Route a click back: mark the item and publish `notify.activate` for the UI.
    pub async fn activated(self: &Arc<Self>, params: ActivatedParams) -> Result<ActivatedResult, RpcError> {
        let posted = lock(&self.history).activate(&params.id, params.action.as_deref(), now_ms());
        let Some(posted) = posted else {
            return Err(RpcError::not_found(format!("no notification {} in the recent history", params.id)));
        };
        self.changed(&posted);
        let n = &posted.notification;
        let activate = Activate {
            id: posted.id.clone(),
            action: params.action.clone(),
            target: n.target.clone(),
            kind: n.kind.clone(),
            project_id: n.project_id.clone(),
            session_id: n.session_id.clone(),
            via: params.via.clone(),
        };
        // The pending approval's Approve action answers it right away (chatflow.approve); the UI
        // still gets the activation and shows the answered card.
        if params.action.as_deref() == Some(derive::APPROVE_ACTION)
            && n.kind == kinds::APPROVAL
            && let (Some(session_id), Some(seq)) = (n.target.get("session_id").and_then(Value::as_str), n.target.get("approval_seq").and_then(Value::as_u64))
        {
            // The session's own flow answers it.
            let flow = match self.call(frame_methods::SESSION_GET, json!({"id": session_id})).await {
                Ok(session) => session.get("flow").and_then(Value::as_str).unwrap_or(flows::CODING).to_owned(),
                Err(_) => flows::default_flow(),
            };
            match self.call(&flows::method(&flow, chatflow_verbs::APPROVE), json!({"session_id": session_id, "seq": seq, "approve": true})).await {
                Ok(_) => tracing::info!(session_id, seq, "approved from the notification"),
                Err(error) => tracing::warn!(%error, session_id, seq, "the notification's Approve did not reach the chat flow"),
            }
        }
        self.publish(topics::ACTIVATE, to_value(&activate)?);
        if self.live_lease().is_none() {
            // Nobody shows the target now: bring the app up and hand the click to it once it is back.
            *lock(&self.pending_activation) = Some((activate, Instant::now()));
            let notifier = self.clone();
            tokio::spawn(async move {
                if let Err(error) = notifier.call(frame_methods::UI_OPEN, json!({})).await {
                    tracing::info!(%error, "the UI could not be opened for a clicked notification");
                }
            });
        }
        Ok(ActivatedResult { routed: true, target: n.target.clone() })
    }

    /// Clicks the notification app spooled while no plugin was listening (it was launched by a
    /// click): published to [`ACTIVATIONS_QUEUE`] in the background, so activation does not wait
    /// for the memory plugin.
    pub async fn drain_spool(self: &Arc<Self>) {
        let notifier = self.clone();
        tokio::spawn(async move { notifier.publish_spool().await });
    }

    /// Move the spool into the queue. The spool is first renamed to a part of its own (the app
    /// starts a new file for later clicks), and the parts are removed only once the queue took
    /// them: when the memory plugin does not serve, they stay and are tried again (and at the next
    /// start). Returns whether every part went.
    pub async fn publish_spool(self: &Arc<Self>) -> bool {
        let _turn = self.spooling.lock().await;
        let spool = self.spool_path();
        if spool.exists() {
            let part = self.data_dir.join(format!("{SPOOL}.{}{TAKEN}", now_ms()));
            if let Err(error) = std::fs::rename(&spool, &part) {
                tracing::warn!(%error, "the spooled clicks could not be taken; they stay for the next start");
                return false;
            }
        }
        let mut parts: Vec<PathBuf> = std::fs::read_dir(&self.data_dir)
            .map(|entries| entries.flatten().map(|e| e.path()).filter(|p| p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with(SPOOL) && n.ends_with(TAKEN))).collect())
            .unwrap_or_default();
        if parts.is_empty() {
            return true;
        }
        parts.sort();
        let mut messages = Vec::new();
        for part in &parts {
            for line in std::fs::read_to_string(part).unwrap_or_default().lines().filter(|l| !l.trim().is_empty()) {
                match serde_json::from_str::<SpooledActivation>(line) {
                    Ok(click) if !click.id.is_empty() => messages.push(json!(click)),
                    _ => tracing::info!(line, "an unreadable spooled click is dropped"),
                }
            }
        }
        if !messages.is_empty() {
            loop {
                match self.graph.ask::<Value>(queue_methods::QUEUE_PUBLISH, json!({"queue": ACTIVATIONS_QUEUE, "messages": messages})).await {
                    Ok(_) => break,
                    Err(error) if matches!(error.code.as_str(), codes::UNAVAILABLE | codes::CONNECTION_CLOSED | codes::DRAINING | codes::UNKNOWN_METHOD | codes::TIMEOUT) => {
                        tracing::info!(error = %error.message, clicks = messages.len(), "the memory plugin does not take the spooled clicks yet; they stay spooled");
                        tokio::time::sleep(SPOOL_RETRY).await;
                    }
                    Err(error) => {
                        tracing::warn!(error = %error.message, "the memory queue refused the spooled clicks; they stay for the next start");
                        return false;
                    }
                }
            }
            tracing::info!(clicks = messages.len(), "spooled clicks moved to the queue");
        }
        for part in &parts {
            let _ = std::fs::remove_file(part);
        }
        drop(_turn);
        self.consume().await;
        true
    }

    /// Route the queued clicks, once the history is loaded: each message is acked after it was
    /// handled; one whose notification the history does not know is acked with a log line. A
    /// failed take or ack leaves the rest queued (an unacked message comes back after its lease).
    pub async fn consume(self: &Arc<Self>) {
        if !self.loaded.load(Ordering::SeqCst) {
            return;
        }
        let _turn = self.consuming.lock().await;
        loop {
            let taken: QueueTakeResult = match self.graph.ask(queue_methods::QUEUE_TAKE, json!({"queue": ACTIVATIONS_QUEUE, "consumer": CONSUMER, "max": TAKE_MAX})).await {
                Ok(taken) => taken,
                Err(error) => {
                    tracing::warn!(error = %error.message, "the queued clicks could not be read; they wait in the queue");
                    return;
                }
            };
            if taken.messages.is_empty() {
                return;
            }
            let mut ids = Vec::new();
            for message in taken.messages {
                match serde_json::from_value::<SpooledActivation>(message.value) {
                    Ok(click) => {
                        if let Err(error) = self.activated(ActivatedParams { id: click.id.clone(), action: click.action, via: Some("helper".into()) }).await {
                            tracing::info!(%error, id = %click.id, "a queued click names no notification in the history; it is dropped");
                        }
                    }
                    Err(error) => tracing::info!(%error, "an unreadable queued click is dropped"),
                }
                ids.push(message.id);
            }
            if let Err(error) = self.graph.ask::<Value>(queue_methods::QUEUE_ACK, json!({"queue": ACTIVATIONS_QUEUE, "consumer": CONSUMER, "ids": ids})).await {
                tracing::warn!(error = %error.message, "the handled clicks were not acked; they may be routed again");
                return;
            }
        }
    }

    pub fn spool_path(&self) -> PathBuf {
        self.data_dir.join(SPOOL)
    }

    // ----- preferences -----

    async fn load_prefs(&self, state: &mut PrefsState) {
        let outcome = self.call(memory_methods::SETTINGS_LIST, json!({"prefix": prefs::SECTION})).await;
        let settings = match outcome {
            Ok(result) => Ok(result.get("settings").cloned().unwrap_or(Value::Null)),
            // No `notify` section yet: nothing set, defaults apply from the settings database's view.
            Err(error) if error.code == codes::NOT_FOUND => Ok(Value::Null),
            Err(error) => Err(error),
        };
        match settings {
            Ok(settings) => {
                let (prefs, problems) = Prefs::from_settings(&settings);
                if matches!(state.source, Source::Defaults(_)) {
                    tracing::info!("notification preferences read from the settings database");
                }
                *state = PrefsState { prefs, source: Source::Settings, problems, loaded: Some(Instant::now()) };
            }
            Err(error) => {
                let reason = format!("memory.settings unavailable: {}", error.message);
                if !matches!(&state.source, Source::Defaults(old) if *old == reason) {
                    tracing::warn!(%reason, "notification preferences fall back to the built-in defaults");
                }
                *state = PrefsState { prefs: Prefs::default(), source: Source::Defaults(reason), problems: Vec::new(), loaded: Some(Instant::now()) };
            }
        }
    }

    async fn prefs_now(&self) -> (Prefs, Option<String>) {
        let mut state = self.prefs.lock().await;
        let stale = match (state.loaded, &state.source) {
            (None, _) => true,
            (Some(at), Source::Settings) => at.elapsed() > PREFS_FRESH,
            (Some(at), Source::Defaults(_)) => at.elapsed() > PREFS_RETRY,
        };
        if stale {
            self.load_prefs(&mut state).await;
        }
        (state.prefs.clone(), state.note())
    }

    async fn prefs_result(&self, force: bool) -> PrefsResult {
        let mut state = self.prefs.lock().await;
        if force || state.loaded.is_none() || !matches!(state.source, Source::Settings) {
            self.load_prefs(&mut state).await;
        }
        let (source, detail) = state.source_name();
        state.prefs.to_result(source, &detail)
    }

    async fn prefs_set(&self, params: PrefsSetParams) -> Result<PrefsResult, RpcError> {
        let mut writes: Vec<(String, Value, String)> = Vec::new();
        if let Some(kind) = &params.kind {
            let info = kind_info(kind).ok_or_else(|| RpcError::invalid_params(format!("`{kind}` is not a notification kind with a switch")))?;
            let enabled = params.enabled.ok_or_else(|| RpcError::invalid_params("`enabled` is required with `kind`"))?;
            writes.push((prefs::path(&format!("kinds.{kind}")), json!(enabled), format!("Notify: {}", info.description)));
        }
        if let Some(flag) = params.only_when_away {
            writes.push((prefs::path("only_when_away"), json!(flag), "Notify only when the user is not looking at the session".into()));
        }
        if let Some(ms) = params.long_job_ms {
            writes.push((prefs::path("long_job_ms"), json!(ms), "A background job must run this long (ms) to notify when it ends".into()));
        }
        if let Some(ms) = params.dedup_window_ms {
            writes.push((prefs::path("dedup_window_ms"), json!(ms), "Repeats within this window (ms) update one notification".into()));
        }
        if writes.is_empty() {
            return Err(RpcError::invalid_params("nothing to set"));
        }
        for (path, value, description) in writes {
            self.call(memory_methods::SETTINGS_SET, json!({"path": path, "value": value, "by": "notify", "description": description}))
                .await
                .map_err(|error| {
                    RpcError::new(
                        &error.code,
                        format!("notification preferences live in the settings database (memory.settings), which did not take the change: {}", error.message),
                    )
                })?;
        }
        Ok(self.prefs_result(true).await)
    }

    // ----- status -----

    pub async fn status(&self) -> StatusResult {
        let native = self.native.status().await;
        let deliverer = self.live_lease().map(|lease| DelivererInfo {
            deliverer_id: lease.id,
            kind: lease.kind,
            age_ms: lease.since.elapsed().as_millis() as u64,
            ttl_ms: lease.ttl_ms,
        });
        let (focus, focus_age_ms) = match lock(&self.focus).clone() {
            Some((focus, at)) => (Some(focus), Some(at.elapsed().as_millis() as u64)),
            None => (None, None),
        };
        let (prefs_source, prefs_detail) = {
            let mut state = self.prefs.lock().await;
            if state.loaded.is_none() {
                self.load_prefs(&mut state).await;
            }
            let (source, detail) = state.source_name();
            (source.to_owned(), detail)
        };
        StatusResult { native, deliverer, focus, focus_age_ms, prefs_source, prefs_detail, history: lock(&self.history).len() }
    }

    // ----- events -----

    pub async fn event(self: &Arc<Self>, topic: &str, data: Value) {
        let trigger = if topic.starts_with("journal.") {
            derive::from_journal_patch(&data)
        } else {
            match topic {
                agents::topics::AGENT_CHANGED => derive::from_agents_changed(&data),
                "plugin.swapped" => derive::from_swapped(&data),
                "plugin.state" => derive::from_plugin_state(&data),
                tools::topics::JOB_EVENT => derive::from_job(&data),
                "schedule.ran" => derive::from_routine_ran(&data),
                topics::FOCUS => {
                    self.set_focus(data);
                    None
                }
                "memory.settings.changed" => {
                    let path = data.get("path").and_then(Value::as_str).unwrap_or("");
                    if path == prefs::SECTION || path.starts_with(&format!("{}.", prefs::SECTION)) {
                        let mut state = self.prefs.lock().await;
                        self.load_prefs(&mut state).await;
                    }
                    None
                }
                _ => None,
            }
        };
        if let Some(trigger) = trigger {
            self.ingest(trigger).await;
        }
    }

    /// Derive and post the notification for one trigger (delivery continues in the background).
    pub async fn ingest(self: &Arc<Self>, trigger: Trigger) {
        let context = self.resolve(&trigger).await;
        let (prefs, _) = self.prefs_now().await;
        if let Some(notification) = derive::notification(&trigger, &context, &prefs) {
            self.post(notification, &trigger.source(), false).await;
        }
    }

    /// Look up the names a trigger's text needs. A failed lookup leaves the name out.
    async fn resolve(&self, trigger: &Trigger) -> Context {
        let mut ctx = Context { now_ms: now_ms(), ..Context::default() };
        let mut task_id = trigger.task_id().map(str::to_owned);
        if let Some(session_id) = trigger.session_id() {
            match self.call(frame_methods::SESSION_GET, json!({"id": session_id})).await.and_then(parse_params::<Session>) {
                Ok(session) => {
                    ctx.project_id = Some(session.project_id.clone());
                    ctx.session_title = Some(session.title.clone()).filter(|t| !t.trim().is_empty());
                    if session.kind == SessionKind::Agent {
                        ctx.agent = session.agent.clone();
                    }
                    if task_id.is_none() {
                        task_id = session.task_id.clone();
                    }
                }
                Err(error) => tracing::debug!(%error, session_id, "session lookup failed"),
            }
        }
        match trigger {
            Trigger::Status { session_id, seq, state, run_id, .. } if state == "done" || state == "failed" => {
                let entries = self.journal_before(session_id, *seq).await;
                ctx.focus_seq = focus_of_run(&entries, state, run_id.as_deref());
            }
            Trigger::AgentBlocked { session_id: Some(session_id), .. } => {
                let entries = self.journal_before(session_id, u64::MAX).await;
                ctx.focus_seq = blocking_entry(&entries);
            }
            _ => {}
        }
        match trigger {
            Trigger::AgentBlocked { project_id, agent, .. } => {
                ctx.project_id = Some(project_id.clone());
                if task_id.is_none()
                    && let Ok(list) = self.call(agents::methods::LIST, json!({"project_id": project_id})).await.and_then(parse_params::<agents::ListResult>)
                {
                    task_id = list.agents.into_iter().find(|record| record.name == *agent).and_then(|record| record.task_id);
                }
            }
            Trigger::Job { job_id, .. } => {
                if let Ok(status) = self.call(tools::methods::JOB_STATUS, json!({"job_id": job_id})).await.and_then(parse_params::<tools::JobStatus>) {
                    ctx.job_started_ms = Some(status.started_ms);
                    ctx.job_output_tail = Some(status.output_tail);
                }
            }
            _ => {}
        }
        if let Some(project_id) = ctx.project_id.clone() {
            match self.call(frame_methods::PROJECT_GET, json!({"id": project_id})).await.and_then(parse_params::<Project>) {
                Ok(project) => ctx.project_name = Some(project.name),
                Err(error) => tracing::debug!(%error, "project lookup failed"),
            }
        }
        if let Some(task_id) = task_id {
            match self.call(memory_methods::TASK_GET, json!({"id": task_id})).await {
                Ok(task) => ctx.task_title = task.get("title").and_then(Value::as_str).filter(|t| !t.trim().is_empty()).map(str::to_owned),
                Err(error) => tracing::debug!(%error, "task lookup failed"),
            }
        }
        ctx
    }

    /// The session's entries before `seq` (at most [`FOCUS_LOOKBACK`] of them); empty when the
    /// journal cannot be read.
    async fn journal_before(&self, session_id: &str, seq: u64) -> Vec<JournalEntry> {
        let from = if seq == u64::MAX { 1 } else { seq.saturating_sub(FOCUS_LOOKBACK).max(1) };
        match self.call(frame_methods::JOURNAL_READ, json!({"session_id": session_id, "from": from})).await.and_then(parse_params::<JournalReadResult>) {
            Ok(read) => {
                let mut entries: Vec<JournalEntry> = read.entries.into_iter().filter(|entry| entry.seq < seq).collect();
                if seq == u64::MAX && entries.len() as u64 > FOCUS_LOOKBACK {
                    entries.drain(..entries.len() - FOCUS_LOOKBACK as usize);
                }
                entries
            }
            Err(error) => {
                tracing::debug!(%error, session_id, "journal lookup failed");
                Vec::new()
            }
        }
    }

    // ----- lifecycle -----

    /// The conformance self-test: derives notifications from synthetic events, exercises dedup and
    /// the gate on scratch state, and checks a native path without showing anything.
    pub async fn probe(&self) -> ProbeReport {
        let mut checks = Vec::new();
        let started = Instant::now();
        let failed = json!({"op": "create", "entry": {"seq": 7, "ts_ms": 1, "session_id": "probe-session", "kind": "status",
                             "data": {"state": "failed", "detail": "probe reason", "run_id": "probe-run"}}});
        let streaming = json!({"op": "append", "session_id": "probe-session", "seq": 8, "field": "text", "text": "x"});
        let context = Context { project_name: Some("probe-project".into()), task_title: Some("probe task".into()), now_ms: now_ms(), ..Context::default() };
        let derived = derive::from_journal_patch(&failed).and_then(|trigger| derive::notification(&trigger, &context, &Prefs::default()));
        let derive_ok = derived.as_ref().is_some_and(|n| {
            n.kind == kinds::FAILED
                && n.title.contains("probe task")
                && n.body == "probe reason"
                && n.subtitle.as_deref() == Some("probe-project")
                && n.target["session_id"] == "probe-session"
                && n.key.as_deref() == Some("run:probe-session:probe-run")
        }) && derive::from_journal_patch(&streaming).is_none();
        checks.push(ProbeCheck {
            name: "derive".into(),
            ok: derive_ok,
            detail: derived.map(|n| format!("{}: {}", n.kind, n.title)).unwrap_or_else(|| "no notification derived".into()),
            ms: started.elapsed().as_millis() as u64,
        });

        let started = Instant::now();
        let mut scratch = History::default();
        let mut sample = Notification {
            kind: kinds::DONE.into(),
            title: "probe".into(),
            body: "one".into(),
            key: Some("probe-key".into()),
            project_id: None,
            session_id: Some("probe-session".into()),
            target: Value::Null,
            actions: vec![],
            subtitle: None,
        };
        let first = scratch.record(sample.clone(), "probe", 1_000, 60_000);
        sample.body = "two".into();
        let second = scratch.record(sample.clone(), "probe", 2_000, 60_000);
        let dedup_ok = matches!(first, Recorded::New(_)) && matches!(&second, Recorded::Updated { posted, .. } if posted.updates == 1) && scratch.len() == 1;
        let mut off = Prefs::default();
        off.kinds.insert(kinds::DONE.into(), false);
        let gate_ok = self.suppression(&sample, &off).is_some() && self.suppression(&sample, &Prefs::default()).is_some() == self.looking_at("probe-session");
        checks.push(ProbeCheck {
            name: "dedup_and_gate".into(),
            ok: dedup_ok && gate_ok,
            detail: format!("dedup {dedup_ok}, preference gate {gate_ok}"),
            ms: started.elapsed().as_millis() as u64,
        });

        let started = Instant::now();
        let native = self.native.check().await;
        checks.push(ProbeCheck {
            name: "native_path".into(),
            ok: native.is_ok(),
            detail: match native {
                Ok(detail) | Err(detail) => detail,
            },
            ms: started.elapsed().as_millis() as u64,
        });

        let started = Instant::now();
        let (_, note) = self.prefs_now().await;
        checks.push(ProbeCheck {
            name: "preferences".into(),
            ok: true,
            detail: note.unwrap_or_else(|| "read from the settings database".into()),
            ms: started.elapsed().as_millis() as u64,
        });
        ProbeReport::from_checks(checks)
    }

    pub fn export_state(&self) -> Value {
        let history = lock(&self.history).clone();
        let (focus, focus_age_ms) = match lock(&self.focus).clone() {
            Some((focus, at)) => (Some(focus), at.elapsed().as_millis() as u64),
            None => (None, 0),
        };
        let lease = self.live_lease().map(|l| (l.id, l.kind, l.ttl_ms, l.renewed.elapsed().as_millis() as u64));
        serde_json::to_value(Exported { history, focus, focus_age_ms, lease }).unwrap_or(Value::Null)
    }

    pub fn import_state(&self, state: Value) {
        let Ok(exported) = serde_json::from_value::<Exported>(state) else { return };
        {
            let mut targets = lock(&self.targets);
            for item in exported.history.items() {
                targets.entry(item.id.clone()).or_insert_with(|| history::target_names(&item.notification.target));
            }
        }
        lock(&self.history).merge(exported.history);
        self.retire_evicted();
        if let Some(focus) = exported.focus {
            let at = Instant::now().checked_sub(Duration::from_millis(exported.focus_age_ms)).unwrap_or_else(Instant::now);
            *lock(&self.focus) = Some((focus, at));
        }
        if let Some((id, kind, ttl_ms, age_ms)) = exported.lease {
            let renewed = Instant::now().checked_sub(Duration::from_millis(age_ms)).unwrap_or_else(Instant::now);
            *lock(&self.lease) = Some(Lease { id, kind, ttl_ms, renewed, since: renewed });
        }
    }

    /// Wait until the writes asked for so far reached the graph, for at most [`FLUSH_WAIT`] (the
    /// memory plugin may be gone already).
    pub async fn flush(&self) {
        if self.bus.get().is_some() && tokio::time::timeout(FLUSH_WAIT, self.graph.flush()).await.is_err() {
            tracing::warn!("notifications not written to the graph before the stop");
        }
    }

    pub async fn shutdown(&self) {
        self.flush().await;
        self.native.stop().await;
    }
}
