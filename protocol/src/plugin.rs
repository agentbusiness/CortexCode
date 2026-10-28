//! Plugin-side runtime: connect to the frame, introduce yourself, serve requests with the standard
//! lifecycle (health, probe, drain, shutdown, state handoff) and call other categories through the frame.
//!
//! A plugin process is started by the frame with these environment variables:
//! - `CORTEX_FRAME_SOCKET`: Unix socket of the frame
//! - `CORTEX_PLUGIN_INSTANCE`: the instance id the frame assigned to this process
//! - `CORTEX_HOME`: the CortexCode home directory (default `~/.cortexcode`)
//! - `CORTEX_PLUGIN_DATA`: this plugin's private data directory (stable across versions)
//! - `CORTEX_LIVE`: the live folder, every plugin's data (prompts, texts, skills) as one git
//!   repository, edited in place
//! - `CORTEX_PLUGIN_LIVE`: this plugin's folder in the live folder (`<live>/plugins/<id>`), where
//!   its data is read from when present; unset in safe mode (packaged data only)

use crate::conn::{BoxFuture, Connection, Handler, IncomingRequest, ItemSink};
use crate::contracts::lifecycle::{
    DrainParams, DrainResult, Health, Hello, HelloAck, ProbeReport, methods as lifecycle_methods,
};
use crate::envelope::{RpcError, codes, parse_params, to_value};
use serde_json::Value;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

/// Environment the frame gives every plugin process.
#[derive(Debug, Clone)]
pub struct PluginEnv {
    pub socket: PathBuf,
    pub instance: String,
    pub home: PathBuf,
    pub data_dir: PathBuf,
    /// The live folder (see the module doc).
    pub live_root: Option<PathBuf>,
    /// The plugin's folder in the live folder (see the module doc). It may not exist yet: the live
    /// folder is created after the first start, so readers check it on every read.
    pub live_dir: Option<PathBuf>,
}

impl PluginEnv {
    /// `<live_dir>/<relative>` when that exists, else `packaged`: live data first, the
    /// package's copy otherwise (and always in safe mode).
    pub fn live_or(&self, relative: &str, packaged: PathBuf) -> PathBuf {
        match &self.live_dir {
            Some(dir) if dir.join(relative).is_dir() => dir.join(relative),
            _ => packaged,
        }
    }
}

impl PluginEnv {
    pub fn from_env() -> Result<Self, RpcError> {
        let var = |name: &str| {
            std::env::var(name).map_err(|_| RpcError::internal(format!("{name} is not set; plugins are started by the frame")))
        };
        Ok(Self {
            socket: PathBuf::from(var("CORTEX_FRAME_SOCKET")?),
            instance: var("CORTEX_PLUGIN_INSTANCE")?,
            home: PathBuf::from(var("CORTEX_HOME")?),
            data_dir: PathBuf::from(var("CORTEX_PLUGIN_DATA")?),
            live_root: std::env::var_os("CORTEX_LIVE").filter(|value| !value.is_empty()).map(PathBuf::from),
            live_dir: std::env::var_os("CORTEX_PLUGIN_LIVE").filter(|value| !value.is_empty()).map(PathBuf::from),
        })
    }
}

/// What a plugin gets to talk to the rest of the system.
#[derive(Clone)]
pub struct Ctx {
    conn: Arc<OnceLock<Connection>>,
    pub env: PluginEnv,
    ack: Arc<OnceLock<HelloAck>>,
    ack_ready: Arc<Notify>,
}

impl Ctx {
    /// Resolve once the frame's hello acknowledgement (and so [`Ctx::config`]) is available.
    /// The frame may send `plugin.probe` the instant it has answered hello, before `run_plugin`
    /// has stored the answer, so requests wait here first (bounded; the frame answers hello once).
    async fn wait_ack(&self) {
        let deadline = tokio::time::sleep(Duration::from_secs(10));
        tokio::pin!(deadline);
        loop {
            let notified = self.ack_ready.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.ack.get().is_some() {
                return;
            }
            tokio::select! {
                _ = &mut notified => {}
                _ = &mut deadline => return,
            }
        }
    }

    /// The connection to the frame. Available once the plugin has said hello.
    pub fn frame(&self) -> Result<&Connection, RpcError> {
        self.conn.get().ok_or_else(|| RpcError::new(codes::UNAVAILABLE, "not connected to the frame yet"))
    }

    /// Version of the frame this plugin is connected to (from its hello acknowledgement).
    pub fn frame_version(&self) -> Option<String> {
        self.ack.get().map(|ack| ack.frame_version.clone())
    }

    /// Configuration the frame sent in its hello acknowledgement.
    pub fn config(&self) -> Value {
        self.ack.get().map(|ack| ack.config.clone()).unwrap_or(Value::Null)
    }

    /// Call any method on any category; the frame routes it to the active plugin.
    pub async fn call(&self, method: &str, params: Value) -> Result<Value, RpcError> {
        self.frame()?.call(method, params).await
    }

    /// Call with stream items delivered.
    pub fn call_stream(&self, method: &str, params: Value) -> Result<crate::conn::CallStream, RpcError> {
        self.frame()?.call_stream(method, params)
    }

    /// Publish an event; the frame fans it out to subscribers (plugins and UI clients).
    pub fn publish(&self, topic: &str, data: Value) {
        if let Some(conn) = self.conn.get() {
            conn.publish(topic, data);
        }
    }
}

/// Context of one incoming request.
pub struct Call {
    pub method: String,
    pub params: Value,
    /// Stream items to the caller.
    pub items: ItemSink,
    /// Cancelled when the caller cancels or disconnects.
    pub cancel: CancellationToken,
}

/// The behaviour of a plugin. Only [`Plugin::handle`] and [`Plugin::probe`] are required.
pub trait Plugin: Send + Sync + 'static {
    /// Serve one category method.
    fn handle(&self, ctx: Ctx, call: Call) -> BoxFuture<'static, Result<Value, RpcError>>;

    /// Contract conformance self-test run by the frame before this instance receives traffic.
    /// It must exercise the plugin for real (not report static success).
    fn probe(&self, ctx: Ctx) -> BoxFuture<'static, Result<ProbeReport, RpcError>>;

    /// Called after a passed probe (and state handoff), right before the frame routes traffic to
    /// this instance. An error keeps the instance out of service.
    fn activate(&self, _ctx: Ctx) -> BoxFuture<'static, Result<(), RpcError>> {
        Box::pin(async { Ok(()) })
    }

    /// Hand state to a successor version. Most plugins keep no private state and return `null`.
    fn export_state(&self, _ctx: Ctx) -> BoxFuture<'static, Result<Value, RpcError>> {
        Box::pin(async { Ok(Value::Null) })
    }

    /// Receive the predecessor's exported state.
    fn import_state(&self, _ctx: Ctx, _state: Value) -> BoxFuture<'static, Result<(), RpcError>> {
        Box::pin(async { Ok(()) })
    }

    /// An event this plugin subscribed to.
    fn event(&self, _ctx: Ctx, _topic: String, _data: Value) -> BoxFuture<'static, ()> {
        Box::pin(async {})
    }

    /// Called before exit on `plugin.shutdown`.
    fn shutdown(&self, _ctx: Ctx) -> BoxFuture<'static, ()> {
        Box::pin(async {})
    }
}

struct Host<P: Plugin> {
    plugin: Arc<P>,
    ctx: Ctx,
    in_flight: Arc<AtomicUsize>,
    idle: Arc<Notify>,
    draining: Arc<AtomicBool>,
    exit: CancellationToken,
    started: Instant,
}

fn is_lifecycle(method: &str) -> bool {
    matches!(
        method,
        lifecycle_methods::HEALTH
            | lifecycle_methods::PROBE
            | lifecycle_methods::ACTIVATE
            | lifecycle_methods::EXPORT_STATE
            | lifecycle_methods::IMPORT_STATE
            | lifecycle_methods::DRAIN
            | lifecycle_methods::SHUTDOWN
    )
}

struct FlightGuard {
    in_flight: Arc<AtomicUsize>,
    idle: Arc<Notify>,
}

impl Drop for FlightGuard {
    fn drop(&mut self) {
        if self.in_flight.fetch_sub(1, Ordering::SeqCst) == 1 {
            self.idle.notify_waiters();
        }
    }
}

impl<P: Plugin> Handler for Host<P> {
    fn handle(&self, request: IncomingRequest) -> BoxFuture<'static, ()> {
        let plugin = self.plugin.clone();
        let ctx = self.ctx.clone();
        let in_flight = self.in_flight.clone();
        let idle = self.idle.clone();
        let draining = self.draining.clone();
        let exit = self.exit.clone();
        let uptime_ms = self.started.elapsed().as_millis() as u64;
        // `handle` runs synchronously in the connection's read loop, in wire order. Admitting a
        // category request here (not inside the spawned task) means a request that arrived before
        // `plugin.drain` is always counted as in flight and finished before the drain completes.
        let admitted = if is_lifecycle(&request.method) || draining.load(Ordering::SeqCst) {
            None
        } else {
            in_flight.fetch_add(1, Ordering::SeqCst);
            Some(FlightGuard { in_flight: in_flight.clone(), idle: idle.clone() })
        };
        Box::pin(async move {
            ctx.wait_ack().await;
            let IncomingRequest { method, params, responder, cancel, .. } = request;
            let outcome = match method.as_str() {
                lifecycle_methods::HEALTH => to_value(&Health {
                    status: if draining.load(Ordering::SeqCst) { "draining".into() } else { "ok".into() },
                    in_flight: in_flight.load(Ordering::SeqCst),
                    uptime_ms,
                    pid: std::process::id(),
                }),
                lifecycle_methods::PROBE => match plugin.probe(ctx).await {
                    Ok(report) => to_value(&report),
                    Err(error) => Err(error),
                },
                lifecycle_methods::ACTIVATE => plugin.activate(ctx).await.map(|_| Value::Null),
                lifecycle_methods::EXPORT_STATE => plugin.export_state(ctx).await,
                lifecycle_methods::IMPORT_STATE => plugin.import_state(ctx, params).await.map(|_| Value::Null),
                lifecycle_methods::DRAIN => {
                    let limit = parse_params::<DrainParams>(params).map(|p| p.timeout_ms).unwrap_or(30_000);
                    draining.store(true, Ordering::SeqCst);
                    let deadline = tokio::time::sleep(Duration::from_millis(limit));
                    tokio::pin!(deadline);
                    loop {
                        // Register interest before checking, so a completion in between is not missed.
                        let notified = idle.notified();
                        tokio::pin!(notified);
                        notified.as_mut().enable();
                        if in_flight.load(Ordering::SeqCst) == 0 {
                            break;
                        }
                        tokio::select! {
                            _ = &mut notified => {}
                            _ = &mut deadline => break,
                        }
                    }
                    to_value(&DrainResult { remaining: in_flight.load(Ordering::SeqCst) })
                }
                lifecycle_methods::SHUTDOWN => {
                    plugin.shutdown(ctx).await;
                    responder.ok(Value::Null);
                    // Give the writer a moment to flush the response before the process exits.
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    exit.cancel();
                    return;
                }
                _ => match admitted {
                    None => Err(RpcError::new(codes::DRAINING, "this plugin instance is draining").retryable()),
                    Some(_guard) => {
                        let call = Call { method, params, items: responder.sink(), cancel: cancel.clone() };
                        tokio::select! {
                            outcome = plugin.handle(ctx, call) => outcome,
                            _ = cancel.cancelled() => Err(RpcError::cancelled()),
                        }
                    }
                },
            };
            responder.respond(outcome);
        })
    }

    fn event(&self, topic: String, data: Value) -> BoxFuture<'static, ()> {
        self.plugin.event(self.ctx.clone(), topic, data)
    }

    fn closed(&self) {
        // The frame is gone: a plugin never outlives its frame.
        self.exit.cancel();
    }
}

/// Connect to the frame, say hello and serve until shutdown or until the frame disconnects.
/// Returns after a clean shutdown; the caller should then exit the process.
pub async fn run_plugin<P: Plugin>(mut hello: Hello, plugin: Arc<P>) -> Result<(), RpcError> {
    let env = PluginEnv::from_env()?;
    hello.pid = std::process::id();
    hello.instance = env.instance.clone();
    let stream = tokio::net::UnixStream::connect(&env.socket)
        .await
        .map_err(|error| RpcError::internal(format!("cannot connect to frame at {}: {error}", env.socket.display())))?;
    let ctx = Ctx { conn: Arc::new(OnceLock::new()), env, ack: Arc::new(OnceLock::new()), ack_ready: Arc::new(Notify::new()) };
    let exit = CancellationToken::new();
    let host = Arc::new(Host {
        plugin,
        ctx: ctx.clone(),
        in_flight: Arc::new(AtomicUsize::new(0)),
        idle: Arc::new(Notify::new()),
        draining: Arc::new(AtomicBool::new(false)),
        exit: exit.clone(),
        started: Instant::now(),
    });
    let conn = Connection::spawn(stream, host);
    let _ = ctx.conn.set(conn.clone());
    let ack: HelloAck = parse_params(conn.call(crate::contracts::frame::methods::HELLO, to_value(&hello)?).await?)?;
    if !ack.accepted {
        return Err(RpcError::new(codes::CONTRACT_VIOLATION, format!("frame rejected hello: {}", ack.reason)));
    }
    let _ = ctx.ack.set(ack);
    ctx.ack_ready.notify_waiters();
    tokio::select! {
        _ = exit.cancelled() => {}
        _ = conn.wait_closed() => {}
    }
    Ok(())
}
