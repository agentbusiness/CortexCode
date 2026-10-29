//! The frame's shared state and its connection handling: peers (plugin processes over the Unix
//! socket, UI clients over WebSocket), request routing to the active instance of a category,
//! event fan-out, and the locally served `frame.*` methods.

use crate::config::Config;
use crate::home::Home;
use crate::journal::{Durable, Journal};
use crate::power::KeepAwake;
use crate::shell::Shell;
use crate::state::StateStore;
use crate::supervisor::{self, Instance, Slot, Supervisor};
use crate::uiplugins::{UiChange, UiPlugins};
use crate::units::Units;
use cc_protocol::contracts::frame::{
    DeleteResult, InstallParams, JournalAppendParams, JournalAppendResult, JournalPatch, JournalPatchParams, JournalReadParams,
    JournalReadResult, KeepAwakeParams, KeepAwakeResult, Machine, PluginInfo, PluginListResult, PluginToggleParams, RouteClearParams, RouteSetParams,
    ProjectDeleted, ProjectGetParams, ProjectListResult, ProjectOpenParams, ProvidersParams, RollbackParams, SessionCreateParams, SessionDeleted,
    SessionGetParams,
    SessionListParams, SessionListResult, SessionUpdateParams, SubscribeParams, SubscribeResult, SwapParams, SwitchParams,
    UiOpenResult, UiPluginListResult, UiStatePutParams, UiStatePutResult, methods, topics,
};
use cc_protocol::contracts::lifecycle::{Hello, HelloAck};
use cc_protocol::contracts::{category_of, contract_of, now_ms};
use cc_protocol::{BoxFuture, CallStream, Connection, Handler, IncomingRequest, Responder, RpcError, codes, parse_params, to_value};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, RwLock};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

pub const FRAME_VERSION: &str = env!("CARGO_PKG_VERSION");

/// How long a cancelled forwarded call may take to deliver its final response.
const CANCEL_GRACE: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeerKind {
    /// A plugin process on the Unix socket.
    Plugin,
    /// A WebSocket (or HTTP API) client.
    Client,
}

pub struct Peer {
    pub id: u64,
    pub kind: PeerKind,
    pub conn: OnceLock<Connection>,
    pub hello: OnceLock<Hello>,
    pub instance: OnceLock<Arc<Instance>>,
    subs: Mutex<Vec<String>>,
    /// The category this client serves after its hello (takes the category over).
    serves: Mutex<Option<String>>,
    /// The category this client serves for the sessions routed to it (`frame.route.set`).
    serves_sessions: Mutex<Option<String>>,
    pub connected_ms: u64,
    pub command_scope: Option<Arc<crate::access::CommandScope>>,
}

impl Peer {
    pub fn conn(&self) -> Option<&Connection> {
        self.conn.get()
    }

    fn subscriptions(&self) -> Vec<String> {
        self.subs.lock().map(|subs| subs.clone()).unwrap_or_default()
    }

    pub fn add_subscriptions(&self, prefixes: &[String]) -> Vec<String> {
        let Ok(mut subs) = self.subs.lock() else {
            return Vec::new();
        };
        for prefix in prefixes {
            if !subs.contains(prefix) {
                subs.push(prefix.clone());
            }
        }
        subs.clone()
    }
}

/// The routing target of a category.
#[derive(Clone)]
pub struct Route {
    pub conn: Connection,
    /// Instance id (process plugins) or `ws-<peer>` (WebSocket-served categories).
    pub instance: String,
    pub peer: u64,
}

pub struct Frame {
    pub config: Config,
    pub home: Home,
    pub token: String,
    pub command_access: crate::access::CommandAccess,
    pub socket: PathBuf,
    pub port: AtomicU16,
    pub headless: bool,
    /// Calls on the projects, sessions and slot state that came before the records were loaded
    /// from the store, in arrival order; None once loaded ([`Frame::records_ready`]).
    records_waiting: Mutex<Option<Vec<(Arc<Peer>, IncomingRequest)>>>,
    /// The live folder (`[frame] live`, default `<home>/live`).
    pub live: PathBuf,
    /// Safe mode: plugins are not pointed at their live data.
    pub safe: bool,
    pub machine: Machine,
    pub journal: Journal,
    pub state: StateStore,
    /// The memory plugin that keeps the frame's records (see [`crate::store`]).
    pub memory: Arc<crate::store::MemoryLink>,
    pub ui_plugins: UiPlugins,
    pub units: Units,
    pub power: KeepAwake,
    pub sup: Supervisor,
    pub shell: Shell,
    pub shutting_down: AtomicBool,
    pub shutdown: CancellationToken,
    peers: RwLock<HashMap<u64, Arc<Peer>>>,
    routes: RwLock<HashMap<String, Route>>,
    /// Routes of one session's calls of a category, by `(category, key)`: a client serving the
    /// category with scope `sessions` (a joined computer's model lent to one agent).
    session_routes: RwLock<HashMap<(String, String), Route>>,
    /// WebSocket clients serving a category, oldest first; the newest one is routed.
    ws_servers: Mutex<HashMap<String, Vec<(u64, Hello)>>>,
    next_peer: AtomicU64,
    /// Serializes UI rescans, so `ui_plugin.changed` events go out in scan order.
    ui_scan: tokio::sync::Mutex<()>,
    /// File watches on the UI sources, by directory; a change triggers a rescan.
    /// The served window's bundle hash (empty while none is served); a change is announced as
    /// `ui.renderer.changed`.
    window_build: Mutex<String>,
}

pub struct FrameParts {
    pub config: Config,
    pub home: Home,
    pub token: String,
    pub socket: PathBuf,
    pub headless: bool,
    pub machine: Machine,
    pub journal: Journal,
    pub state: StateStore,
    pub memory: Arc<crate::store::MemoryLink>,
    pub units: Units,
    pub live: PathBuf,
    pub safe: bool,
}

impl Frame {
    pub fn new(parts: FrameParts) -> Arc<Frame> {
        let FrameParts { config, home, token, socket, headless, machine, journal, state, memory, units, live, safe } = parts;
        let disabled = units.disabled();
        let ui_plugins = UiPlugins::new(units.ui_sources(), disabled.clone());
        let window_build = ui_plugins.window().map(|window| window.plugin.hash).unwrap_or_default();
        let sup = Supervisor::new(&units.backends(), &disabled);
        let shell = Shell::new(config.ui_dir.clone());
        Arc::new(Frame {
            units,
            ui_scan: tokio::sync::Mutex::new(()),
            window_build: Mutex::new(window_build),
            power: KeepAwake::new(),
            port: AtomicU16::new(0),
            records_waiting: Mutex::new(Some(Vec::new())),
            shutting_down: AtomicBool::new(false),
            shutdown: CancellationToken::new(),
            peers: RwLock::new(HashMap::new()),
            routes: RwLock::new(HashMap::new()),
            session_routes: RwLock::new(HashMap::new()),
            ws_servers: Mutex::new(HashMap::new()),
            next_peer: AtomicU64::new(1),
            config,
            home,
            token,
            command_access: crate::access::CommandAccess::default(),
            socket,
            headless,
            live,
            safe,
            machine,
            journal,
            state,
            memory,
            ui_plugins,
            sup,
            shell,
        })
    }

    pub fn is_shutting_down(&self) -> bool {
        self.shutting_down.load(Ordering::SeqCst)
    }

    /// Register a new peer on `stream` and start serving it.
    pub fn attach<S>(self: &Arc<Self>, stream: S, kind: PeerKind) -> Arc<Peer>
    where
        S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Send + 'static,
    {
        self.attach_scoped(stream, kind, None)
    }

    pub fn attach_scoped<S>(
        self: &Arc<Self>,
        stream: S,
        kind: PeerKind,
        command_scope: Option<Arc<crate::access::CommandScope>>,
    ) -> Arc<Peer>
    where
        S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Send + 'static,
    {
        let peer = Arc::new(Peer {
            id: self.next_peer.fetch_add(1, Ordering::Relaxed),
            kind,
            conn: OnceLock::new(),
            hello: OnceLock::new(),
            instance: OnceLock::new(),
            subs: Mutex::new(Vec::new()),
            serves: Mutex::new(None),
            serves_sessions: Mutex::new(None),
            connected_ms: now_ms(),
            command_scope,
        });
        if let Ok(mut peers) = self.peers.write() {
            peers.insert(peer.id, peer.clone());
        }
        let handler = Arc::new(PeerHandler { frame: self.clone(), peer: peer.clone() });
        let conn = Connection::spawn(stream, handler);
        let _ = peer.conn.set(conn);
        peer
    }

    fn detach(self: &Arc<Self>, peer: &Arc<Peer>) {
        if let Ok(mut peers) = self.peers.write() {
            peers.remove(&peer.id);
        }
        self.power.release_peer(peer.id);
        let served = peer.serves.lock().ok().and_then(|mut serves| serves.take());
        if let Some(category) = served {
            self.ws_unserve(&category, peer.id);
        }
        if let Ok(mut routes) = self.session_routes.write() {
            routes.retain(|_, route| route.peer != peer.id);
        }
        if let Some(instance) = peer.instance.get()
            && !instance.is_retiring()
            && !self.is_shutting_down()
        {
            // A plugin that closed its connection is not serving anymore; the exit monitor turns
            // the process exit into crash handling.
            tracing::warn!(instance = %instance.id, "plugin connection closed unexpectedly; stopping the process");
            instance.kill();
        }
    }

    /// Disconnect every WebSocket / API client (frame shutdown).
    pub fn close_clients(&self) {
        let peers: Vec<Arc<Peer>> = self.peers.read().map(|peers| peers.values().cloned().collect()).unwrap_or_default();
        for peer in peers {
            if peer.kind == PeerKind::Client
                && let Some(conn) = peer.conn()
            {
                conn.close();
            }
        }
    }

    // ----- routing -----

    /// The flows installed here (plugins of the `chatflow` family, enabled), CortexCode's own
    /// first; none installed: the coding flow.
    pub fn installed_flows(&self) -> Vec<String> {
        use cc_protocol::contracts::frame::flows;
        let mut installed: Vec<String> = self.sup.slots().iter().filter(|slot| !slot.is_disabled()).filter_map(|slot| flows::flow_of(&slot.category).map(str::to_owned)).collect();
        installed.sort_by_key(|flow| (flow != flows::CODING, flow.clone()));
        installed.dedup();
        if installed.is_empty() { flows::default_flows() } else { installed }
    }

    pub fn route(&self, category: &str) -> Option<Route> {
        self.routes.read().ok().and_then(|routes| routes.get(category).cloned())
    }

    pub fn set_route(&self, category: &str, route: Route) {
        if category == crate::store::MEMORY {
            self.memory.set(route.conn.clone());
        }
        let instance = route.instance.clone();
        let conn = route.conn.clone();
        if let Ok(mut routes) = self.routes.write() {
            routes.insert(category.to_owned(), route);
        }
        // A plugin that starts now did not hear of the projects deleted before: told again (its
        // cleanup is the same each time, so one that heard already loses nothing).
        let subscribed = self.peers.read().ok().is_some_and(|peers| {
            peers.values().any(|peer| {
                peer.instance.get().is_some_and(|i| i.id == instance) && peer.subscriptions().iter().any(|prefix| topics::PROJECT_DELETED.starts_with(prefix.as_str()))
            })
        });
        if subscribed {
            for deleted in self.state.deleted_projects() {
                if let Ok(data) = to_value(&deleted) {
                    conn.publish(topics::PROJECT_DELETED, data);
                }
            }
        }
    }

    /// Stop routing `category` to the WebSocket client that serves it (a disabled plugin's shell
    /// module, until the shell unloads it). Returns whether a route was removed.
    pub fn unroute_ws(&self, category: &str) -> bool {
        let Ok(mut routes) = self.routes.write() else {
            return false;
        };
        if routes
            .get(category)
            .is_some_and(|route| route.instance.starts_with("ws-"))
        {
            routes.remove(category);
            return true;
        }
        false
    }

    /// Remove the route of `category` if it still points at `instance`.
    pub fn clear_route(&self, category: &str, instance: &str) -> bool {
        let Ok(mut routes) = self.routes.write() else {
            return false;
        };
        if routes
            .get(category)
            .is_some_and(|route| route.instance == instance)
        {
            routes.remove(category);
            if category == crate::store::MEMORY {
                self.memory.clear();
            }
            return true;
        }
        false
    }

    /// Open a forwarded call to the active instance of `category`. Done synchronously while the
    /// routing table is read-locked, so a request is enqueued on the old instance strictly before
    /// a swap's commit (and therefore before its `plugin.drain`), or on the new one after it.
    fn dispatch(&self, category: &str, method: &str, params: Value) -> Result<(CallStream, String), RpcError> {
        if let Some(key) = session_key(&params)
            && let Some(route) = self
                .session_routes
                .read()
                .ok()
                .and_then(|routes| routes.get(&(category.to_owned(), key)).cloned())
        {
            let stream = route.conn.call_stream(method, params)?;
            return Ok((stream, route.instance));
        }
        let routes = self.routes.read().map_err(|_| RpcError::internal("routing table poisoned"))?;
        match routes.get(category) {
            Some(route) => {
                let stream = route.conn.call_stream(method, params)?;
                Ok((stream, route.instance.clone()))
            }
            None => Err(self.unavailable(category)),
        }
    }

    /// The error calls to a disabled plugin's category get: retryable, so a caller that waits for
    /// the plugin to be enabled again can keep trying.
    pub fn disabled_error(&self, slot: &Slot) -> RpcError {
        RpcError::new(
            codes::UNAVAILABLE,
            format!("`{}` is unavailable: the plugin `{}` is disabled (enable it in Settings > Plugins or with frame.plugins.enable)", slot.category, slot.plugin()),
        )
        .retryable()
        .with_data(json!({"category": slot.category, "state": "disabled", "plugin": slot.plugin()}))
    }

    /// Publish `plugin.state` for a slot as `frame.plugins.list` shows it.
    pub fn publish_slot_state(&self, slot: &Slot) {
        for info in self.sup.list(self).into_iter().filter(|info| info.category == slot.category) {
            if let Ok(value) = to_value(&info) {
                self.publish(topics::PLUGIN_STATE, &value);
            }
        }
    }

    /// Re-read what UI packages are served (the units' running versions, enabled or not) and
    /// announce every change as `ui_plugin.changed`. Returns the changes.
    pub async fn refresh_ui(self: &Arc<Self>) -> Vec<UiChange> {
        let _scan = self.ui_scan.lock().await;
        self.ui_plugins.set_packages(self.units.ui_sources());
        let frame = self.clone();
        let changes = tokio::task::spawn_blocking(move || frame.ui_plugins.rescan()).await.unwrap_or_default();
        for change in &changes {
            let data = match change {
                UiChange::Changed(plugin) => {
                    tracing::info!(id = %plugin.id, hash = %plugin.hash, "UI plugin changed");
                    serde_json::to_value(plugin).unwrap_or_default()
                }
                UiChange::Removed(id) => {
                    tracing::info!(%id, "UI plugin removed");
                    json!({"id": id, "removed": true})
                }
            };
            self.publish(topics::UI_PLUGIN_CHANGED, &data);
        }
        // A new window: an open workbench reloads, so it never runs the old layout with the new
        // plugin packages.
        let build = self.ui_plugins.window().map(|window| window.plugin.hash).unwrap_or_default();
        let window_changed = self.window_build.lock().ok().is_some_and(|mut last| {
            let changed = !build.is_empty() && !last.is_empty() && *last != build;
            *last = build.clone();
            changed
        });
        if window_changed {
            tracing::info!(%build, "the window changed; open workbenches reload");
            self.publish(topics::UI_RENDERER_CHANGED, &json!({"build": build}));
        }
        changes
    }

    /// Whether a command in the model's shells may call `method`: the frame's own read methods,
    /// and the methods the plugin serving the category declared in its hello (`commands`).
    fn command_permitted(&self, method: &str, params: &Value) -> Result<(), RpcError> {
        let category = category_of(method);
        let declared = if category == "frame" {
            crate::access::FRAME_COMMANDS.contains(&method)
        } else {
            let session_route = session_key(params).and_then(|key| {
                self.session_routes.read().ok().and_then(|routes| routes.get(&(category.to_owned(), key)).cloned())
            });
            let Some(route) = session_route.or_else(|| self.routes.read().ok().and_then(|routes| routes.get(category).cloned())) else {
                return Err(self.unavailable(category));
            };
            let peer = self.peers.read().ok().and_then(|peers| peers.get(&route.peer).cloned());
            peer.and_then(|peer| peer.hello.get().map(|hello| hello.commands.iter().any(|known| known == method))).unwrap_or(false)
        };
        if declared {
            Ok(())
        } else {
            Err(RpcError::new(
                codes::CONTRACT_VIOLATION,
                format!("`{method}` requires the user's app connection; use the CortexCode window to approve or perform it"),
            ))
        }
    }

    fn unavailable(&self, category: &str) -> RpcError {
        if let Some(slot) = self.sup.slot(category)
            && slot.is_disabled()
        {
            return self.disabled_error(&slot);
        }
        if let Some(slot) = self.sup.slot(category) {
            let state = slot.describe_state();
            RpcError::new(codes::UNAVAILABLE, format!("`{category}` is not active yet ({state})"))
                .retryable()
                .with_data(json!({"category": category, "state": state}))
        } else if let Some(plugin) = self.units.disabled_server_of(category) {
            RpcError::new(
                codes::UNAVAILABLE,
                format!("`{category}` is unavailable: the plugin `{plugin}` is disabled (enable it in Settings > Plugins or with frame.plugins.enable)"),
            )
            .retryable()
            .with_data(json!({"category": category, "state": "disabled", "plugin": plugin}))
        } else if self.ws_servers.lock().is_ok_and(|servers| servers.contains_key(category)) {
            // Served by a client earlier in this run (e.g. the UI shell while it restarts).
            RpcError::new(codes::UNAVAILABLE, format!("the client serving `{category}` disconnected"))
                .retryable()
                .with_data(json!({"category": category, "state": "disconnected"}))
        } else {
            RpcError::new(codes::UNAVAILABLE, format!("no plugin serves `{category}`"))
                .with_data(json!({"category": category, "state": "absent"}))
        }
    }

    // ----- events -----

    /// Deliver an event to every active plugin whose hello subscribes to a matching prefix and to
    /// every client subscribed to one. Sends are enqueued synchronously, so callers that publish
    /// under a lock (the journal) get per-connection ordering.
    pub fn publish(&self, topic: &str, data: &Value) {
        let active: Vec<String> =
            self.routes.read().map(|routes| routes.values().map(|route| route.instance.clone()).collect()).unwrap_or_default();
        let Ok(peers) = self.peers.read() else { return };
        for peer in peers.values() {
            if peer.kind == PeerKind::Plugin {
                let Some(instance) = peer.instance.get() else {
                    continue;
                };
                if !active.contains(&instance.id) {
                    continue;
                }
            }
            let matches = peer.subscriptions().iter().any(|prefix| topic.starts_with(prefix.as_str()));
            if matches && let Some(conn) = peer.conn() {
                conn.publish(topic, data.clone());
            }
        }
    }

    // ----- WebSocket-served categories -----

    fn ws_hello(self: &Arc<Self>, peer: &Arc<Peer>, hello: Hello) -> HelloAck {
        let reject = |reason: String| HelloAck {
            accepted: false,
            reason,
            instance: String::new(),
            frame_version: FRAME_VERSION.into(),
            config: Value::Null,
        };
        if let Err(reason) = supervisor::check_contract(&hello.category, &hello.contract) {
            return reject(reason);
        }
        if hello.scope == "sessions" {
            // Serves the category only for the sessions `frame.route.set` binds to it; the
            // category's own plugin keeps serving everything else.
            if peer.hello.get().is_some() {
                return reject("this connection already said hello".into());
            }
            let instance = format!("ws-{}", peer.id);
            let _ = peer.hello.set(hello.clone());
            peer.add_subscriptions(&hello.subscribes);
            if let Ok(mut serves) = peer.serves_sessions.lock() {
                *serves = Some(hello.category.clone());
            }
            tracing::info!(category = %hello.category, peer = peer.id, "client serves a category for routed sessions");
            return HelloAck { accepted: true, reason: String::new(), instance, frame_version: FRAME_VERSION.into(), config: Value::Null };
        }
        if !hello.scope.is_empty() {
            return reject(format!("unknown hello scope `{}` (empty or `sessions`)", hello.scope));
        }
        // A category whose plugin runs here is the plugin's; a client may serve it while that
        // plugin is disabled (a member of the mesh lending its model: the local one is unloaded
        // first) or when no plugin of the category is installed.
        if let Some(slot) = self.sup.slot(&hello.category)
            && !slot.is_disabled()
        {
            return reject(format!("`{}` is served by a supervised plugin process", hello.category));
        }
        if let Some(plugin) = self.units.disabled_server_of(&hello.category) {
            return reject(format!("`{}` belongs to the plugin `{plugin}`, which is disabled", hello.category));
        }
        if peer.hello.get().is_some() {
            return reject("this connection already said hello".into());
        }
        let instance = format!("ws-{}", peer.id);
        let _ = peer.hello.set(hello.clone());
        peer.add_subscriptions(&hello.subscribes);
        if let Ok(mut serves) = peer.serves.lock() {
            *serves = Some(hello.category.clone());
        }
        if let Ok(mut servers) = self.ws_servers.lock() {
            servers.entry(hello.category.clone()).or_default().push((peer.id, hello.clone()));
        }
        if let Some(conn) = peer.conn() {
            self.set_route(&hello.category, Route { conn: conn.clone(), instance: instance.clone(), peer: peer.id });
        }
        tracing::info!(category = %hello.category, plugin = %hello.plugin_id, version = %hello.version, peer = peer.id, "client serves a category");
        self.publish(topics::PLUGIN_STATE, &json!(self.ws_info(peer.id, &hello, "active")));
        HelloAck { accepted: true, reason: String::new(), instance, frame_version: FRAME_VERSION.into(), config: Value::Null }
    }

    fn ws_unserve(self: &Arc<Self>, category: &str, peer_id: u64) {
        let (removed, next) = {
            let Ok(mut servers) = self.ws_servers.lock() else {
                return;
            };
            let list = servers.entry(category.to_owned()).or_default();
            let removed = list.iter().position(|(id, _)| *id == peer_id).map(|index| list.remove(index));
            (removed, list.last().cloned())
        };
        let Some((_, hello)) = removed else { return };
        let instance = format!("ws-{peer_id}");
        if self.clear_route(category, &instance) {
            match &next {
                Some((next_peer, _)) => {
                    let conn = self.peers.read().ok().and_then(|peers| peers.get(next_peer).and_then(|p| p.conn().cloned()));
                    if let Some(conn) = conn {
                        self.set_route(category, Route { conn, instance: format!("ws-{next_peer}"), peer: *next_peer });
                    }
                }
                // No client serves it any more: the category's own plugin, which the client had
                // taken over (a borrowed model, a borrowed tool), serves it again.
                None => {
                    if let Some(route) = self.sup.slot(category).and_then(|slot| slot.live_current()).and_then(|instance| instance.route()) {
                        self.set_route(category, route);
                        tracing::info!(category, "the category's own plugin serves it again");
                    }
                }
            }
        }
        tracing::info!(category, peer = peer_id, "client stopped serving a category");
        self.publish(topics::PLUGIN_STATE, &json!(self.ws_info(peer_id, &hello, "stopped")));
    }

    fn ws_info(&self, peer_id: u64, hello: &Hello, state: &str) -> PluginInfo {
        let started_ms = self.peers.read().ok().and_then(|peers| peers.get(&peer_id).map(|p| p.connected_ms)).unwrap_or(0);
        PluginInfo {
            plugin_id: hello.plugin_id.clone(),
            version: hello.version.clone(),
            category: hello.category.clone(),
            contract: hello.contract.clone(),
            instance: format!("ws-{peer_id}"),
            state: state.into(),
            pid: hello.pid,
            exec: "websocket".into(),
            started_ms,
            restarts: 0,
            build: String::new(),
            commit: None,
            known_good: false,
            transport: "websocket".into(),
            plugin: String::new(),
        }
    }

    /// Every plugin as a unit (configured, installed and `[[ui_plugin_dir]]` owners).
    pub fn unit_list(&self) -> Vec<cc_protocol::contracts::frame::PluginUnit> {
        self.units.all().iter().map(|unit| self.units.info_of(unit)).collect()
    }

    pub fn plugin_list(&self) -> Vec<PluginInfo> {
        let mut list = self.sup.list(self);
        let servers: Vec<(u64, Hello)> =
            self.ws_servers.lock().map(|servers| servers.values().flatten().cloned().collect()).unwrap_or_default();
        for (peer_id, hello) in servers {
            let routed = self.route(&hello.category).is_some_and(|route| route.peer == peer_id);
            list.push(self.ws_info(peer_id, &hello, if routed { "active" } else { "standby" }));
        }
        list
    }

    // ----- local methods -----

    /// Frame methods that finish without waiting. They run inline in the connection's read loop,
    /// so a client's state changes apply in the order it sent them.
    fn serve_now(self: &Arc<Self>, peer: &Arc<Peer>, method: &str, params: Value) -> Result<Value, RpcError> {
        match method {
            methods::HELLO => {
                let hello: Hello = parse_params(params)?;
                let ack = match peer.kind {
                    PeerKind::Plugin => self.sup.accept_hello(self, peer, hello),
                    PeerKind::Client => self.ws_hello(peer, hello),
                };
                to_value(&ack)
            }
            methods::COMMAND_ACCESS => {
                let tools_backend = peer.kind == PeerKind::Plugin && peer.hello.get().is_some_and(|hello| hello.category == "tools");
                if !tools_backend {
                    return Err(RpcError::new(
                        codes::CONTRACT_VIOLATION,
                        "command credentials are issued only to the active tools backend",
                    ));
                }
                let params = parse_params(params)?;
                to_value(&self.command_access.issue(
                    params,
                    self.port.load(Ordering::SeqCst),
                    &self.socket,
                )?)
            }
            methods::PROJECT_OPEN => {
                let params: ProjectOpenParams = parse_params(params)?;
                if !std::path::Path::new(&params.root).is_absolute() {
                    return Err(RpcError::invalid_params(format!("project root `{}` must be an absolute path", params.root)));
                }
                let (project, created) = self.state.open_project(&params.root, self.installed_flows())?;
                if created {
                    self.publish(topics::PROJECT_CHANGED, &to_value(&project)?);
                }
                to_value(&project)
            }
            methods::PROJECT_LIST => to_value(&ProjectListResult { projects: self.state.projects() }),
            methods::PROJECT_GET => {
                let params: ProjectGetParams = parse_params(params)?;
                to_value(&self.state.project(&params.id)?)
            }
            methods::SESSION_CREATE => {
                let params: SessionCreateParams = parse_params(params)?;
                let session = self.state.create_session(params)?;
                self.publish(topics::SESSION_CHANGED, &to_value(&session)?);
                to_value(&session)
            }
            methods::SESSION_LIST => {
                let params: SessionListParams = if params.is_null() { SessionListParams::default() } else { parse_params(params)? };
                to_value(&SessionListResult { sessions: self.state.sessions(params.project_id.as_deref()) })
            }
            methods::SESSION_GET => {
                let params: SessionGetParams = parse_params(params)?;
                to_value(&self.state.session(&params.id)?)
            }
            methods::SESSION_UPDATE => {
                let params: SessionUpdateParams = parse_params(params)?;
                let session = self.state.update_session(params)?;
                self.publish(topics::SESSION_CHANGED, &to_value(&session)?);
                to_value(&session)
            }
            methods::SESSION_DELETE => {
                let params: SessionGetParams = parse_params(params)?;
                let removed = self.state.delete_session(&params.id)?;
                to_value(&DeleteResult { sessions: self.forget_sessions(removed) })
            }
            methods::PROJECT_DELETE => {
                let params: ProjectGetParams = parse_params(params)?;
                let (project, removed) = self.state.delete_project(&params.id)?;
                let sessions = self.forget_sessions(removed);
                self.publish(topics::PROJECT_DELETED, &to_value(&ProjectDeleted { project_id: project.id, root: project.root })?);
                to_value(&DeleteResult { sessions })
            }
            methods::JOURNAL_READ => {
                let params: JournalReadParams = parse_params(params)?;
                self.state.session(&params.session_id)?;
                let (entries, rev) = self.journal.read(&params.session_id, params.from.unwrap_or(1))?;
                to_value(&JournalReadResult { entries, rev })
            }
            methods::PLUGINS_LIST => to_value(&PluginListResult { plugins: self.plugin_list(), units: self.unit_list() }),
            methods::UI_PLUGINS_LIST => to_value(&UiPluginListResult { plugins: self.ui_plugins.list() }),
            methods::PLUGINS_PROVIDERS => {
                let params: ProvidersParams = parse_params(params)?;
                to_value(&self.units.providers(&params.category))
            }
            methods::PLUGINS_BUNDLED => to_value(&crate::units::bundled(self.config.packages.as_deref())),
            methods::UI_STATE_GET => to_value(&self.state.ui_state()),
            methods::UI_STATE_PUT => {
                let params: UiStatePutParams = parse_params(params)?;
                to_value(&UiStatePutResult { rev: self.state.put_ui_state(params.document)? })
            }
            methods::SUBSCRIBE => {
                let params: SubscribeParams = parse_params(params)?;
                to_value(&SubscribeResult { topics: peer.add_subscriptions(&params.topics) })
            }
            methods::MACHINE => to_value(&self.machine),
            methods::UPDATE_STATUS => to_value(&crate::update::status(&self.home.root)),
            methods::UPDATE_RESTART => {
                let status = crate::update::status(&self.home.root);
                if status.staged.is_none() {
                    return Err(RpcError::invalid_params("no newer frame is staged; nothing to restart into"));
                }
                tracing::info!(staged = ?status.staged, "restarting onto the staged frame");
                crate::update::request_restart();
                self.shell.quit.notify_one();
                to_value(&status)
            }
            methods::ROUTE_SET => {
                let params: RouteSetParams = parse_params(params)?;
                if params.key.is_empty() {
                    return Err(RpcError::invalid_params("key must not be empty"));
                }
                let peer_id = params
                    .instance
                    .strip_prefix("ws-")
                    .and_then(|id| id.parse::<u64>().ok())
                    .ok_or_else(|| RpcError::invalid_params(format!("`{}` is not a client instance (`ws-<peer>`)", params.instance)))?;
                let target = self.peers.read().ok().and_then(|peers| peers.get(&peer_id).cloned());
                let Some(target) = target else {
                    return Err(RpcError::invalid_params(format!("`{}` is not connected", params.instance)));
                };
                let serves = target.serves_sessions.lock().ok().and_then(|serves| serves.clone());
                if serves.as_deref() != Some(params.category.as_str()) {
                    return Err(RpcError::invalid_params(format!(
                        "`{}` does not serve `{}` for sessions (hello with scope `sessions`)",
                        params.instance, params.category
                    )));
                }
                let Some(conn) = target.conn().cloned() else {
                    return Err(RpcError::invalid_params(format!("`{}` is not connected", params.instance)));
                };
                if let Ok(mut routes) = self.session_routes.write() {
                    routes.insert((params.category.clone(), params.key.clone()), Route { conn, instance: params.instance.clone(), peer: peer_id });
                }
                tracing::info!(category = %params.category, key = %params.key, instance = %params.instance, "session routed");
                Ok(json!({}))
            }
            methods::ROUTE_CLEAR => {
                let params: RouteClearParams = parse_params(params)?;
                if let Ok(mut routes) = self.session_routes.write() {
                    routes.remove(&(params.category, params.key));
                }
                Ok(json!({}))
            }
            methods::KEEP_AWAKE => {
                let params: KeepAwakeParams = parse_params(params)?;
                if params.holder.is_empty() {
                    return Err(RpcError::invalid_params("holder must not be empty"));
                }
                if params.holder == crate::power::FRAME_HOLDER {
                    return Err(RpcError::invalid_params("the frame's own keep-awake holder is not the caller's"));
                }
                let state = if params.hold {
                    self.power.hold(&params.holder, &params.reason, peer.id)
                } else {
                    self.power.release(&params.holder)
                }
                .map_err(RpcError::internal)?;
                to_value(&KeepAwakeResult { asserted: state.asserted, holders: state.holders, assertion_id: state.assertion_id })
            }
            other => Err(RpcError::unknown_method(other)),
        }
    }

    /// The methods that read or change the projects, sessions, journals and slot state.
    fn is_record_method(method: &str) -> bool {
        matches!(
            method,
            methods::PROJECT_OPEN
                | methods::PROJECT_LIST
                | methods::PROJECT_GET
                | methods::PROJECT_DELETE
                | methods::SESSION_CREATE
                | methods::SESSION_LIST
                | methods::SESSION_GET
                | methods::SESSION_UPDATE
                | methods::SESSION_DELETE
                | methods::JOURNAL_READ
                | methods::JOURNAL_APPEND
                | methods::JOURNAL_PATCH
                | methods::UI_STATE_GET
                | methods::UI_STATE_PUT
        )
    }

    /// The records are loaded from the store (or will not be): the calls that waited for them are
    /// served in the order they came, under the same lock, so no later call overtakes them.
    pub fn records_ready(self: &Arc<Self>) {
        let mut waiting = self.records_waiting.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        for (peer, request) in waiting.take().unwrap_or_default() {
            tokio::spawn(serve(self.clone(), peer, request));
        }
    }

    fn is_long_running(method: &str) -> bool {
        matches!(
            method,
            methods::PLUGINS_SWAP
                | methods::PLUGINS_ROLLBACK
                | methods::UI_OPEN
                | methods::PLUGINS_DISABLE
                | methods::PLUGINS_ENABLE
                | methods::PLUGINS_INSTALL
                | methods::PLUGINS_INSTALL_MANY
                | methods::PLUGINS_INSTALL_WAIT
                | methods::PLUGINS_SWITCH
                | methods::UPDATE_STAGE
        )
    }

    /// Frame methods that take time (swaps, starting the UI). They run as detached tasks.
    async fn serve_long(self: Arc<Self>, method: String, params: Value) -> Result<Value, RpcError> {
        match method.as_str() {
            methods::PLUGINS_SWAP => {
                let params: SwapParams = parse_params(params)?;
                to_value(&supervisor::swap_request(&self, params).await?)
            }
            methods::PLUGINS_ROLLBACK => {
                let params: RollbackParams = parse_params(params)?;
                let plugin = match params.plugin.filter(|p| !p.is_empty()) {
                    Some(plugin) => plugin,
                    None if params.category.is_empty() => {
                        return Err(RpcError::invalid_params(
                            "give `plugin` (a plugin id) or `category` (its backend's category)",
                        ));
                    }
                    None => self
                        .units
                        .of_category(&params.category)
                        .map(|unit| unit.id.clone())
                        .ok_or_else(|| RpcError::not_found(format!("no installed plugin serves `{}`", params.category)))?,
                };
                to_value(&crate::units::rollback(&self, &plugin).await?)
            }
            methods::PLUGINS_DISABLE | methods::PLUGINS_ENABLE => {
                let params: PluginToggleParams = parse_params(params)?;
                if self.is_shutting_down() {
                    return Err(RpcError::new(codes::UNAVAILABLE, "the frame is shutting down"));
                }
                to_value(
                    &crate::units::set_enabled(
                        &self,
                        &params.plugin,
                        method == methods::PLUGINS_ENABLE,
                    )
                    .await?,
                )
            }
            methods::PLUGINS_INSTALL_MANY => {
                to_value(&crate::units::install_many(&self, parse_params(params)?).await?)
            }
            methods::PLUGINS_INSTALL_WAIT => {
                let params: cc_protocol::contracts::frame::InstallWaitParams =
                    parse_params(params)?;
                to_value(&crate::units::install_wait(&self, &params.id).await?)
            }
            methods::PLUGINS_INSTALL => {
                let params: InstallParams = parse_params(params)?;
                if self.is_shutting_down() {
                    return Err(RpcError::new(codes::UNAVAILABLE, "the frame is shutting down"));
                }
                to_value(&crate::units::install(&self, &params.path).await?)
            }
            methods::PLUGINS_SWITCH => {
                let params: SwitchParams = parse_params(params)?;
                if self.is_shutting_down() {
                    return Err(RpcError::new(codes::UNAVAILABLE, "the frame is shutting down"));
                }
                to_value(&crate::units::switch_provider(&self, &params.category, &params.plugin).await?)
            }
            methods::UI_OPEN => {
                let (running, pid) = self.shell.open(&self).await?;
                to_value(&UiOpenResult { running, pid })
            }
            methods::UPDATE_STAGE => {
                let params: cc_protocol::contracts::frame::UpdateStageParams = parse_params(params)?;
                for path in [&params.frame_archive, &params.electron_archive] {
                    if !path.is_empty() && !std::path::Path::new(path).is_absolute() {
                        return Err(RpcError::invalid_params(format!("`{path}` must be an absolute path")));
                    }
                }
                let root = self.home.root.clone();
                let staged = tokio::task::spawn_blocking(move || crate::update::stage(&root, &params))
                    .await
                    .map_err(|e| RpcError::internal(e.to_string()))?
                    .map_err(RpcError::invalid_params)?;
                to_value(&staged)
            }
            other => Err(RpcError::unknown_method(other)),
        }
    }

    /// Apply a journal append or patch. Runs synchronously (callers on a connection invoke it in
    /// wire order), so patches a client sends back to back are applied in the order it sent them
    /// even when it does not wait for each answer; only the fsync wait is asynchronous.
    fn journal_mutation(&self, method: &str, params: Value) -> Result<(Value, Durable), RpcError> {
        if method == methods::JOURNAL_APPEND {
            let params: JournalAppendParams = parse_params(params)?;
            self.state.session(&params.session_id)?;
            let topic = format!("{}{}", topics::JOURNAL_PREFIX, params.session_id);
            let (seq, durable) =
                self.journal.append(&params.session_id, &params.kind, params.data, |patch| self.publish_patch(&topic, patch))?;
            Ok((to_value(&JournalAppendResult { seq })?, durable))
        } else {
            let params: JournalPatchParams = parse_params(params)?;
            self.state.session(&params.session_id)?;
            let topic = format!("{}{}", topics::JOURNAL_PREFIX, params.session_id);
            let durable = self.journal.patch(&params.session_id, params.seq, &params.field, params.text, params.value, |patch| {
                self.publish_patch(&topic, patch)
            })?;
            Ok((to_value(&JournalAppendResult { seq: params.seq })?, durable))
        }
    }

    /// Sessions removed from the state: their journals go, and every plugin is told (each drops
    /// its own data about them). Returns their ids.
    fn forget_sessions(&self, removed: Vec<cc_protocol::contracts::frame::Session>) -> Vec<String> {
        let mut ids = Vec::with_capacity(removed.len());
        for session in removed {
            self.command_access.forget_session(&session.id);
            if let Err(error) = self.journal.remove(&session.id) {
                tracing::warn!(session = %session.id, %error, "deleting a session's journal failed");
            }
            let deleted = SessionDeleted { session_id: session.id.clone(), project_id: session.project_id, workspace: session.workspace };
            if let Ok(data) = to_value(&deleted) {
                self.publish(topics::SESSION_DELETED, &data);
            }
            ids.push(session.id);
        }
        ids
    }

    fn publish_patch(&self, topic: &str, patch: &JournalPatch) {
        match serde_json::to_value(patch) {
            Ok(data) => self.publish(topic, &data),
            Err(error) => tracing::error!(%error, "cannot serialize a journal patch"),
        }
    }
}

struct PeerHandler {
    frame: Arc<Frame>,
    peer: Arc<Peer>,
}

impl Handler for PeerHandler {
    fn handle(&self, mut request: IncomingRequest) -> BoxFuture<'static, ()> {
        let frame = self.frame.clone();
        let peer = self.peer.clone();
        let method = request.method.clone();
        if let Some(scope) = &peer.command_scope
            && let Err(error) = frame.command_permitted(&method, &request.params).and_then(|()| scope.authorize(&method, &mut request.params))
        {
            return Box::pin(async move {
                request.responder.err(error);
            });
        }
        if method != methods::HELLO && peer.kind == PeerKind::Plugin && peer.hello.get().is_none() {
            return Box::pin(async move {
                request.responder.err(RpcError::new(codes::CONTRACT_VIOLATION, "send frame.hello first"));
            });
        }
        // Until the records are loaded, calls that read or change them wait in arrival order.
        if Frame::is_record_method(&method) {
            let mut waiting = frame.records_waiting.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            if let Some(queue) = waiting.as_mut() {
                queue.push((peer, request));
                return Box::pin(async {});
            }
        }
        serve(frame, peer, request)
    }

    fn event(&self, topic: String, data: Value) -> BoxFuture<'static, ()> {
        if self.peer.command_scope.is_some() {
            return Box::pin(async {});
        }
        if self.peer.kind == PeerKind::Plugin && self.peer.hello.get().is_none() {
            tracing::warn!(peer = self.peer.id, %topic, "event before hello dropped");
        } else {
            // Fan out synchronously in arrival order.
            self.frame.publish(&topic, &data);
        }
        Box::pin(async {})
    }

    fn closed(&self) {
        self.frame.detach(&self.peer);
    }
}

/// Serve one admitted call. The state changes of frame methods are applied before this returns,
/// so calls served one after another apply in that order; the returned future sends the answer.
fn serve(frame: Arc<Frame>, peer: Arc<Peer>, request: IncomingRequest) -> BoxFuture<'static, ()> {
    let method = request.method.clone();
    if method == methods::JOURNAL_APPEND || method == methods::JOURNAL_PATCH {
        let IncomingRequest { method, params, responder, .. } = request;
        let applied = frame.journal_mutation(&method, params);
        return Box::pin(async move {
            match applied {
                Ok((result, durable)) => responder.respond(durable.wait().await.map(|()| result)),
                Err(error) => responder.err(error),
            }
        });
    }
    if method.starts_with("frame.") && Frame::is_long_running(&method) {
        return Box::pin(async move {
            let IncomingRequest { method, params, responder, cancel, .. } = request;
            // Detached: a caller that cancels or disconnects gets `cancelled`, but a swap always
            // runs to completion (dropping it half-way could orphan a staged instance).
            let work = tokio::spawn(frame.serve_long(method, params));
            tokio::select! {
                joined = work => {
                    responder.respond(joined.unwrap_or_else(|error| Err(RpcError::internal(format!("frame task failed: {error}")))));
                }
                _ = cancel.cancelled() => responder.err(RpcError::cancelled()),
            }
        });
    }
    if method.starts_with("frame.") {
        let IncomingRequest { method, params, responder, .. } = request;
        let outcome = frame.serve_now(&peer, &method, params);
        let rejected = method == methods::HELLO
            && outcome
                .as_ref()
                .is_ok_and(|ack| ack.get("accepted") == Some(&Value::Bool(false)));
        let persist = matches!(
            method.as_str(),
            methods::PROJECT_OPEN
                | methods::PROJECT_DELETE
                | methods::SESSION_CREATE
                | methods::SESSION_UPDATE
                | methods::SESSION_DELETE
                | methods::UI_STATE_PUT
        );
        if persist && outcome.is_ok() {
            return Box::pin(async move {
                match frame.state.flush().await {
                    Ok(()) => responder.respond(outcome),
                    Err(error) => responder.err(error),
                }
            });
        }
        responder.respond(outcome);
        if rejected
            && peer.kind == PeerKind::Plugin
            && let Some(conn) = peer.conn()
        {
            // A process the frame cannot admit is disconnected (the reply is flushed first).
            conn.close();
        }
        return Box::pin(async {});
    }
    let category = category_of(&method).to_owned();
    if contract_of(&category).is_none() {
        return Box::pin(async move {
            request.responder.err(RpcError::unknown_method(&method));
        });
    }
    let IncomingRequest { method, params, responder, cancel, .. } = request;
    let first = frame.dispatch(&category, &method, params.clone());
    Box::pin(forward(frame, category, method, params, first, responder, cancel))
}

/// Relay one forwarded call. A `draining` answer (an old instance that already began draining when
/// the request reached it) is retried on the category's new active instance.
async fn forward(
    frame: Arc<Frame>,
    category: String,
    method: String,
    params: Value,
    first: Result<(CallStream, String), RpcError>,
    responder: Responder,
    cancel: CancellationToken,
) {
    let mut attempt = first;
    let mut retries = 0;
    loop {
        let (stream, used) = match attempt {
            Ok(opened) => opened,
            Err(error) => {
                responder.err(error);
                return;
            }
        };
        let (outcome, items) = relay(stream, &responder, &cancel).await;
        let retry = matches!(&outcome, Err(error) if error.code == codes::DRAINING) && items == 0 && retries < 50;
        if !retry || cancel.is_cancelled() {
            responder.respond(outcome);
            return;
        }
        retries += 1;
        // Wait (briefly) for the routing table to move off the draining instance.
        let mut waited = 0;
        while frame.route(&category).is_some_and(|route| route.instance == used) && waited < 100 {
            tokio::time::sleep(Duration::from_millis(20)).await;
            waited += 1;
        }
        attempt = frame.dispatch(&category, &method, params.clone());
    }
}

async fn relay(mut stream: CallStream, responder: &Responder, cancel: &CancellationToken) -> (Result<Value, RpcError>, usize) {
    let mut items = 0;
    let mut deadline: Option<std::pin::Pin<Box<tokio::time::Sleep>>> = None;
    loop {
        tokio::select! {
            item = stream.next() => match item {
                Some(data) => {
                    responder.item(data);
                    items += 1;
                }
                None => break,
            },
            _ = cancel.cancelled(), if deadline.is_none() => {
                stream.cancel();
                deadline = Some(Box::pin(tokio::time::sleep(CANCEL_GRACE)));
            }
            _ = async { if let Some(sleep) = deadline.as_mut() { sleep.as_mut().await } }, if deadline.is_some() => {
                return (Err(RpcError::cancelled()), items);
            }
        }
    }
    (stream.finish().await, items)
}

/// The session a call belongs to, as its parameters name it: `cache_key` (model calls: the
/// session or agent id), `session_id`, or `context.session_id`.
fn session_key(params: &Value) -> Option<String> {
    let direct = params.get("cache_key").or_else(|| params.get("session_id"));
    let key = direct.or_else(|| params.get("context").and_then(|context| context.get("session_id")))?;
    let key = key.as_str()?;
    (!key.is_empty()).then(|| key.to_owned())
}
