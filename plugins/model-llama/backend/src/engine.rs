//! Where each `model.*` call goes.
//!
//! - A GPU with at least 16 GB (`gpu`, `catalog::MIN_GPU_BYTES`): Qwen is loaded on the first call
//!   that needs it (installing it first when needed), unloaded after `idle_unload_s` without a
//!   request and at critical memory pressure. The other apps' models on this machine stay loaded:
//!   each app lends its own (the cortex-network).
//! - Another app on this machine already holds a Qwen (a local member of the mesh): every call that
//!   reaches this plugin is forwarded to it through the mesh (borrowed), and none loads here.
//! - No such GPU: Qwen is never installed or loaded and nothing is lent; every call goes to a
//!   reachable member's Qwen, on this machine or another one.
//!
//! A watcher looks every 5 s.

use crate::borrow::{self, Lender};
use crate::catalog::{self, InstallProgress, Spec};
use crate::config::Config;
use crate::gpu::{self, Gpu};
use crate::local;
use crate::progress::{Publisher, Tracker};
use crate::server::{Launch, Server};
use crate::store;
use cc_protocol::contracts::model::{
    ChatParams, CompleteParams, HoldParams, ModelInfo, PrewarmParams, SelectParams, SelectResult, SlotState, SlotsResult, TokenizeParams, Variant, VariantsResult, VisionInfo,
    methods, tags,
};
use cc_protocol::{Ctx, ItemSink, RpcError, codes, parse_params, to_value};
use serde_json::{Value, json};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;

pub const ENGINE_REVISION: &str = "llama.cpp 468ad58 (unslothai b11876)";
const WATCH_EVERY: Duration = Duration::from_secs(5);

/// Where stream items go: the caller over CPP/1, or stdout (the `call` command).
pub enum Sink {
    Frame(ItemSink),
    Print,
}

impl Sink {
    pub fn send(&self, item: Value) -> bool {
        match self {
            Sink::Frame(sink) => sink.send(item),
            Sink::Print => {
                println!("{item}");
                true
            }
        }
    }
}

enum Target {
    Borrowed(Lender),
    Local(Arc<Server>),
}

pub struct Engine {
    config: Config,
    data_dir: PathBuf,
    models: PathBuf,
    publish: Publisher,
    memory: u64,
    /// The GPU Qwen runs on; None: this machine runs no model of its own.
    gpu: Option<Gpu>,
    variant: Mutex<&'static Spec>,
    /// The loaded llama-server. The lock is held while loading, so one load runs at a time.
    server: tokio::sync::Mutex<Option<Arc<Server>>>,
    /// The frame (the mesh is reached through it); None for the `call` command.
    ctx: Option<Ctx>,
    /// The member whose Qwen serves here now.
    borrowed: Mutex<Option<Lender>>,
    installing: tokio::sync::Mutex<()>,
    install: Arc<Mutex<Option<InstallProgress>>>,
    state: Mutex<&'static str>,
    last_request: Mutex<Instant>,
    /// What this machine learned about the quantized KV cache for the chosen model (`store::kv_check`),
    /// read once: `(model id, kv_type, ok)`.
    kv: Mutex<Option<(String, Option<(String, bool)>)>>,
}

fn physical_memory() -> u64 {
    meminfo_kb("MemTotal:").unwrap_or(0) * 1024
}

/// A `/proc/meminfo` line's value in kB.
fn meminfo_kb(key: &str) -> Option<u64> {
    let text = std::fs::read_to_string("/proc/meminfo").ok()?;
    text.lines().find_map(|line| line.strip_prefix(key)?.trim().strip_suffix("kB")?.trim().parse().ok())
}

/// Critical when under 3% of memory is still available.
fn pressure_critical() -> bool {
    match (meminfo_kb("MemAvailable:"), meminfo_kb("MemTotal:")) {
        (Some(available), Some(total)) => available * 100 < total * 3,
        _ => false,
    }
}

impl Engine {
    pub async fn new(config: Config, home: PathBuf, data_dir: PathBuf, publish: Publisher, ctx: Option<Ctx>) -> Arc<Engine> {
        let _ = std::fs::create_dir_all(&data_dir);
        crate::server::reclaim_stale(&data_dir);
        // The shared models folder; this app's old one (`<CORTEX_HOME>/models`) is moved in first.
        let models = match &config.models_dir {
            Some(dir) => PathBuf::from(dir),
            None => {
                let root = cc_shared_models::root();
                cc_shared_models::move_in(&home.join("models"), &root);
                root
            }
        };
        let gpu = crate::server::binary(&config).ok().and_then(|bin| gpu::find(&bin));
        // The GPU's own memory decides the variant.
        let memory = gpu.as_ref().map_or_else(physical_memory, |gpu| gpu.memory_bytes);
        // The chosen model (setting `model.variant`); the memory plugin is waited for a few seconds
        // at most, and when it answers later the choice applies if no model is loaded yet.
        let (selected, late) = match &ctx {
            Some(ctx) => match store::variant(ctx, Duration::from_secs(5)).await {
                Ok(id) => (id, false),
                Err(()) => (None, true),
            },
            None => (None, false),
        };
        let selected = selected.and_then(|id| catalog::find(&id));
        let variant = selected.or_else(|| catalog::find(&config.variant)).unwrap_or_else(|| catalog::for_memory(memory));
        tracing::info!(variant = variant.id, memory, "model-llama starting");
        let engine = Arc::new(Engine {
            config,
            data_dir,
            models,
            publish,
            memory,
            gpu,
            variant: Mutex::new(variant),
            server: tokio::sync::Mutex::new(None),
            ctx,
            borrowed: Mutex::new(None),
            installing: tokio::sync::Mutex::new(()),
            install: Arc::new(Mutex::new(None)),
            state: Mutex::new("unloaded"),
            last_request: Mutex::new(Instant::now()),
            kv: Mutex::new(None),
        });
        if late && let Some(ctx) = engine.ctx.clone() {
            let weak = Arc::downgrade(&engine);
            tokio::spawn(async move {
                let Ok(Some(id)) = store::variant(&ctx, Duration::from_secs(24 * 3600)).await else { return };
                let (Some(engine), Some(spec)) = (weak.upgrade(), catalog::find(&id)) else { return };
                if engine.spec().id != spec.id && engine.server.lock().await.is_none() {
                    tracing::info!(variant = spec.id, "the chosen model was read from the settings");
                    *engine.variant.lock().unwrap_or_else(|p| p.into_inner()) = spec;
                }
            });
        }
        // Look once before serving, so a first call never loads Qwen next to another app's Qwen.
        engine.watch_once().await;
        let watcher = Arc::downgrade(&engine);
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(WATCH_EVERY).await;
                let Some(engine) = watcher.upgrade() else { return };
                engine.watch_once().await;
            }
        });
        engine
    }

    /// The KV cache check for `spec` on this machine, read from the graph once (3 s at most).
    async fn kv_known(&self, spec: &'static Spec) -> Option<(String, bool)> {
        let model_id = spec.model_id();
        if let Some((id, known)) = self.kv.lock().unwrap_or_else(|p| p.into_inner()).clone()
            && id == model_id
        {
            return known;
        }
        let known = match &self.ctx {
            Some(ctx) => store::kv_check(ctx, &model_id, Duration::from_secs(3)).await,
            None => None,
        };
        *self.kv.lock().unwrap_or_else(|p| p.into_inner()) = Some((model_id, known.clone()));
        known
    }

    /// A KV cache check's outcome, kept in the graph.
    fn kv_learned(&self, spec: &'static Spec, kv: &str, ok: bool) {
        let model_id = spec.model_id();
        *self.kv.lock().unwrap_or_else(|p| p.into_inner()) = Some((model_id.clone(), Some((kv.to_owned(), ok))));
        if let Some(ctx) = &self.ctx {
            store::save_kv(ctx, &model_id, spec.id, kv, ok);
        }
    }

    fn spec(&self) -> &'static Spec {
        *self.variant.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn set_state(&self, state: &'static str) {
        *self.state.lock().unwrap_or_else(|p| p.into_inner()) = state;
    }

    fn state(&self) -> &'static str {
        *self.state.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn borrowed(&self) -> Option<Lender> {
        self.borrowed.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }

    async fn watch_once(self: &Arc<Self>) {
        let loaded = self.server.try_lock().ok().and_then(|slot| slot.clone());
        let lender = match (&self.ctx, self.config.borrow) {
            (Some(ctx), borrow) if borrow || self.gpu.is_none() => borrow::look(ctx, loaded.is_some(), self.gpu.is_none()).await,
            _ => None,
        };
        let was = self.borrowed();
        if was != lender {
            match &lender {
                Some(lender) => tracing::info!(app = %lender.name, model = %lender.model_id, "borrowing a member's model"),
                None if self.gpu.is_none() => tracing::info!("no member lends a model now; this machine has no GPU to run one"),
                None => tracing::info!("the other app's Qwen is gone; Qwen loads here on the next call that needs it"),
            }
            *self.borrowed.lock().unwrap_or_else(|p| p.into_inner()) = lender.clone();
            // The mesh's card follows (it shows no model while this one is borrowed).
            (self.publish)("model.state", json!({"state": if lender.is_some() { "borrowed" } else { self.state() }}));
        }
        let Some(server) = loaded else { return };
        if lender.is_some() {
            // Another app's Qwen serves here: in-flight local calls finish, then this one is stopped.
            let engine = self.clone();
            drop(server);
            tokio::spawn(async move { engine.unload("another app runs Qwen on this machine", Duration::from_secs(600)).await });
        } else if !server.alive() {
            tracing::warn!("llama-server exited; it loads again on the next call");
            drop(server);
            *self.server.lock().await = None;
            self.set_state("unloaded");
        } else if pressure_critical() {
            drop(server);
            let engine = self.clone();
            tokio::spawn(async move { engine.unload("memory pressure is critical", Duration::from_secs(10)).await });
        } else {
            let idle = self.last_request.lock().unwrap_or_else(|p| p.into_inner()).elapsed();
            // Only this watcher's handle and the slot: nothing in flight.
            if idle >= Duration::from_secs(self.config.idle_unload_s) && Arc::strong_count(&server) <= 2 {
                drop(server);
                self.unload("idle", Duration::from_secs(1)).await;
            }
        }
    }

    /// Take the server out of service, let in-flight calls finish (up to `wait`), then stop it.
    async fn unload(&self, reason: &str, wait: Duration) -> f64 {
        let started = Instant::now();
        let Some(server) = self.server.lock().await.take() else { return 0.0 };
        self.set_state("yielding");
        tracing::info!(reason, "unloading Qwen");
        while Arc::strong_count(&server) > 1 && started.elapsed() < wait {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        server.stop();
        drop(server);
        self.set_state("unloaded");
        started.elapsed().as_secs_f64() * 1000.0
    }

    /// Where a call goes now: a member's Qwen when one serves here; otherwise the local Qwen
    /// (loaded now when needed).
    async fn target(&self) -> Result<Target, RpcError> {
        *self.last_request.lock().unwrap_or_else(|p| p.into_inner()) = Instant::now();
        if let Some(lender) = self.borrowed() {
            return Ok(Target::Borrowed(lender));
        }
        if self.gpu.is_none() {
            return Err(no_gpu());
        }
        self.local().await.map(Target::Local)
    }

    /// The loaded server, loading it (and installing the model) first when needed.
    async fn local(&self) -> Result<Arc<Server>, RpcError> {
        let mut slot = self.server.lock().await;
        if let Some(server) = slot.as_ref()
            && server.alive()
        {
            return Ok(server.clone());
        }
        if pressure_critical() {
            return Err(RpcError::new(codes::BUSY, "memory pressure is critical; Qwen is not loaded now").retryable());
        }
        let spec = self.spec();
        self.ensure_installed(spec).await?;
        self.set_state("loading");
        let server = match self.start(spec).await {
            Ok(server) => Arc::new(server),
            Err(error) => {
                self.set_state("unloaded");
                return Err(RpcError::new(codes::UNAVAILABLE, error).retryable());
            }
        };
        self.set_state("ready");
        *slot = Some(server.clone());
        Ok(server)
    }

    /// Start llama-server; a quantized KV cache is tried once and recorded (`kv_check`), f16
    /// otherwise.
    async fn start(&self, spec: &'static Spec) -> Result<Server, String> {
        let wanted = self.config.kv_type.clone();
        let known_bad = wanted != "f16" && self.kv_known(spec).await == Some((wanted.clone(), false));
        let kv = if wanted == "f16" || known_bad { "f16".to_owned() } else { wanted.clone() };
        let device = self.gpu.as_ref().map(|gpu| gpu.device.clone()).ok_or_else(|| no_gpu().message)?;
        let launch = |kv_type: String| Launch { spec, models: self.models.clone(), config: &self.config, data_dir: self.data_dir.clone(), kv_type, device: device.clone() };
        match Server::start(launch(kv.clone())).await {
            Ok(server) => {
                if kv != "f16" {
                    self.kv_learned(spec, &kv, true);
                }
                Ok(server)
            }
            Err(error) if kv != "f16" => {
                tracing::warn!(%error, kv, "llama-server failed with a quantized KV cache (flash attention); using f16");
                self.kv_learned(spec, &kv, false);
                Server::start(launch("f16".into())).await
            }
            Err(error) => Err(error),
        }
    }

    async fn ensure_installed(&self, spec: &'static Spec) -> Result<PathBuf, RpcError> {
        if self.gpu.is_none() {
            return Err(no_gpu());
        }
        if catalog::installed(spec, &self.models) {
            return Ok(spec.weights.dir(&self.models));
        }
        let _guard = self.installing.lock().await;
        if catalog::installed(spec, &self.models) {
            return Ok(spec.weights.dir(&self.models));
        }
        self.set_state("downloading");
        let publish = self.publish.clone();
        let current = self.install.clone();
        let report: catalog::Report = Arc::new(move |progress: InstallProgress| {
            if let Ok(value) = serde_json::to_value(&progress) {
                publish("model.install", value);
            }
            *current.lock().unwrap_or_else(|p| p.into_inner()) = Some(progress);
        });
        let result = catalog::install(spec, self.models.clone(), report).await;
        *self.install.lock().unwrap_or_else(|p| p.into_inner()) = None;
        self.set_state("unloaded");
        result.map_err(|error| RpcError::new(codes::UNAVAILABLE, format!("installing {}: {error}", spec.id)).retryable())
    }

    /// Start installing in the background (the probe on a first start).
    pub fn install_in_background(self: &Arc<Self>) {
        let engine = self.clone();
        tokio::spawn(async move {
            if let Err(error) = engine.ensure_installed(engine.spec()).await {
                tracing::warn!(%error, "model install failed");
            }
        });
    }

    pub fn installed(&self) -> bool {
        catalog::installed(self.spec(), &self.models)
    }

    pub fn install_progress(&self) -> Option<InstallProgress> {
        self.install.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }

    /// This machine runs no model of its own (no usable GPU).
    pub fn without_gpu(&self) -> bool {
        self.gpu.is_none()
    }

    pub fn is_borrowing(&self) -> bool {
        self.borrowed().is_some()
    }

    /// The app whose model serves here, by name.
    pub fn lender_name(&self) -> Option<String> {
        self.borrowed().map(|lender| lender.name)
    }

    fn extra(&self, source: &str, holder: Value) -> Value {
        let mut extra = json!({"source": source, "holder": holder, "state": self.state(), "gpu_memory_bytes": self.memory});
        if let Some(gpu) = &self.gpu {
            extra["gpu"] = json!(gpu.description);
        }
        if let Some(progress) = self.install_progress() {
            extra["install"] = serde_json::to_value(progress).unwrap_or(Value::Null);
        }
        extra
    }

    /// This plugin's own model, loaded or not.
    fn local_info(&self, server: Option<&Server>) -> ModelInfo {
        let spec = server.map(|s| s.spec).unwrap_or_else(|| self.spec());
        let extra = match server {
            Some(server) => {
                let mut extra = self.extra("local", json!("cortexcode"));
                extra["load_ms"] = json!(server.load_ms);
                extra
            }
            None => self.extra("none", Value::Null),
        };
        ModelInfo {
            model_id: spec.model_id(),
            variant: spec.id.into(),
            path: spec.weights.path(&self.models).display().to_string(),
            family: "qwen3.8".into(),
            tags: vec![tags::CODING.into()],
            quant: spec.quant.into(),
            file_bytes: spec.weights.bytes,
            ctx_total: self.config.ctx * self.config.parallel,
            ctx_native: server.map(|s| s.ctx_native).filter(|n| *n > 0).unwrap_or(262_144),
            slots: self.config.parallel,
            kv_type: server.map(|s| s.kv_type.clone()).unwrap_or_else(|| self.config.kv_type.clone()),
            speculative: "draft-dflash".into(),
            backend: self.gpu.as_ref().map_or("none", |gpu| gpu.backend).into(),
            tensor_api: None,
            engine_revision: ENGINE_REVISION.into(),
            extra,
            // Image input through the vision projector, loaded with the model.
            vision: Some(VisionInfo { available: true, loaded: server.is_some(), idle_unload_s: self.config.idle_unload_s, ..VisionInfo::default() }),
        }
    }

    pub async fn info(&self) -> Result<Value, RpcError> {
        if let (Some(lender), Some(ctx)) = (self.borrowed(), &self.ctx) {
            match borrow::call(ctx, &lender, methods::INFO, json!({}), &mut |_| {}, None).await {
                Ok(mut info) => {
                    // `source: borrowed`: the mesh's card does not show this model as this app's.
                    let borrowed = self.extra("borrowed", json!(lender.software));
                    match info.get_mut("extra").and_then(Value::as_object_mut) {
                        Some(extra) => {
                            for (key, value) in borrowed.as_object().cloned().unwrap_or_default() {
                                extra.insert(key, value);
                            }
                        }
                        None => info["extra"] = borrowed,
                    }
                    return Ok(info);
                }
                Err(error) => tracing::warn!(%error, "the other app's model.info failed; reporting the local model"),
            }
        }
        // No model here: the mesh's card shows none, and nothing offers one.
        if self.gpu.is_none() {
            return Err(no_gpu());
        }
        let server = self.server.try_lock().ok().and_then(|slot| slot.clone());
        to_value(&self.local_info(server.as_deref()))
    }

    fn variants(&self) -> VariantsResult {
        // Without a GPU there is no model to choose.
        if self.gpu.is_none() {
            return VariantsResult { variants: Vec::new() };
        }
        let active = self.spec().id;
        VariantsResult {
            variants: catalog::CATALOG
                .iter()
                .map(|spec| {
                    let installed = catalog::installed(spec, &self.models);
                    Variant {
                        id: spec.id.into(),
                        label: spec.label.into(),
                        path: spec.weights.path(&self.models).display().to_string(),
                        installed,
                        active: spec.id == active,
                        notes: spec.notes.into(),
                        repo: Some(spec.weights.repo.into()),
                        revision: Some(spec.weights.revision.into()),
                        sha256: Some(spec.weights.sha256.into()),
                        verified: installed.then_some(true),
                        bytes: Some(spec.files().iter().map(|file| file.bytes).sum()),
                        mtp: Some(false),
                    }
                })
                .collect(),
        }
    }

    async fn select(&self, id: &str) -> Result<SelectResult, RpcError> {
        let started = Instant::now();
        let spec = catalog::find(id).ok_or_else(|| RpcError::invalid_params(format!("unknown variant `{id}`")))?;
        let previous = self.spec().id.to_owned();
        if previous == spec.id {
            let server = self.server.lock().await.clone();
            return Ok(SelectResult {
                variant: spec.id.into(),
                previous,
                changed: false,
                reload_ms: 0.0,
                unload_ms: 0.0,
                load_ms: 0.0,
                info: self.local_info(server.as_deref()),
            });
        }
        self.ensure_installed(spec).await?;
        let was_loaded = self.server.lock().await.is_some();
        let unload_ms = self.unload("variant switch", Duration::from_secs(600)).await;
        *self.variant.lock().unwrap_or_else(|p| p.into_inner()) = spec;
        if let Some(ctx) = &self.ctx {
            store::save_variant(ctx, spec.id);
        }
        let mut load_ms = 0.0;
        if was_loaded && !self.is_borrowing() {
            let server = self.local().await?;
            load_ms = server.load_ms;
        }
        let server = self.server.lock().await.clone();
        Ok(SelectResult {
            variant: spec.id.into(),
            previous,
            changed: true,
            reload_ms: started.elapsed().as_secs_f64() * 1000.0,
            unload_ms,
            load_ms,
            info: self.local_info(server.as_deref()),
        })
    }

    /// Forward a call to the member's model: its own conversation space (`cortexcode:` keys),
    /// and this plugin's progress events for the caller.
    async fn forward(&self, lender: &Lender, method: &str, mut params: Value, items: Option<&Sink>, cancel: &CancellationToken) -> Result<Value, RpcError> {
        let ctx = self.ctx.as_ref().ok_or_else(|| RpcError::new(codes::UNAVAILABLE, "no frame to reach the mesh through"))?;
        let key = params["cache_key"].as_str().unwrap_or_default().to_owned();
        let priority = serde_json::from_value(params["priority"].clone()).unwrap_or_default();
        borrow::prefix_cache_key(&mut params);
        let streams = matches!(method, methods::CHAT | methods::COMPLETE | methods::PREWARM);
        let mut tracker = streams.then(|| Tracker::new(self.publish.clone(), &key, priority, if method == methods::PREWARM { "prewarm" } else { "generate" }));
        let mut on_item = |item: Value| {
            if let Some(tracker) = tracker.as_mut() {
                tracker.token();
            }
            if let Some(items) = items {
                items.send(item);
            }
        };
        let result = borrow::call(ctx, lender, method, params, &mut on_item, Some(cancel.clone())).await;
        if let Some(tracker) = tracker.as_mut() {
            let finish = match &result {
                Ok(value) => value["finish_reason"].as_str().unwrap_or(if method == methods::PREWARM { "prefilled" } else { "stop" }).to_owned(),
                Err(_) => "failed".into(),
            };
            let generated = result.as_ref().ok().and_then(|v| v["usage"]["completion_tokens"].as_u64()).map(|n| n as u32);
            tracker.done(&finish, generated);
        }
        result
    }

    pub async fn dispatch(self: &Arc<Self>, method: &str, params: Value, items: Option<&Sink>, cancel: CancellationToken) -> Result<Value, RpcError> {
        match method {
            methods::INFO => return self.info().await,
            methods::VARIANTS => return to_value(&self.variants()),
            methods::SELECT => {
                let params: SelectParams = parse_params(params)?;
                return to_value(&self.select(&params.variant).await?);
            }
            methods::CHAT
            | methods::COMPLETE
            | methods::TOKENIZE
            | methods::COUNT
            | methods::SLOTS
            | methods::HIBERNATE
            | methods::HOLD
            | methods::PREWARM => {}
            other => return Err(RpcError::unknown_method(other)),
        }
        // Calls that keep nothing warm need no model loaded for them.
        if matches!(method, methods::HIBERNATE | methods::HOLD) && self.borrowed().is_none() {
            return Ok(local_noop(method, &params));
        }
        // The load of a Qwen that is not loaded is none (the mesh's card asks it): no load for it.
        if method == methods::SLOTS && self.borrowed().is_none() && self.server.try_lock().ok().is_some_and(|slot| slot.is_none()) {
            return to_value(&idle_slots(self.config.parallel));
        }
        let server = match self.target().await? {
            Target::Borrowed(lender) => return self.forward(&lender, method, params, items, &cancel).await,
            Target::Local(server) => server,
        };
        match method {
            methods::CHAT => {
                let params: ChatParams = parse_params(params)?;
                let key = params.cache_key.clone().unwrap_or_default();
                let mut tracker = Tracker::new(self.publish.clone(), &key, params.priority, "generate");
                let result = local::chat(&server, &params, items, &cancel, &mut tracker).await;
                if result.is_err() {
                    tracker.done("failed", None);
                }
                to_value(&result?)
            }
            methods::COMPLETE => {
                let params: CompleteParams = parse_params(params)?;
                let key = params.cache_key.clone().unwrap_or_default();
                let mut tracker = Tracker::new(self.publish.clone(), &key, params.priority, "generate");
                let result = local::complete(&server, &params, items, &cancel, &mut tracker).await;
                if result.is_err() {
                    tracker.done("failed", None);
                }
                to_value(&result?)
            }
            methods::PREWARM => {
                let params: PrewarmParams = parse_params(params)?;
                if params.idle_only && Arc::strong_count(&server) > 2 {
                    return Ok(json!({"cache_key": params.cache_key, "skipped": "busy"}));
                }
                let mut tracker = Tracker::new(self.publish.clone(), &params.cache_key, cc_protocol::contracts::model::Priority::Background, "prewarm");
                local::prewarm(&server, &params, &cancel, &mut tracker).await
            }
            methods::TOKENIZE => {
                let params: TokenizeParams = parse_params(params)?;
                to_value(&local::tokenize(&server, &params.text).await?)
            }
            methods::COUNT => {
                let params: ChatParams = parse_params(params)?;
                to_value(&local::count(&server, &params).await?)
            }
            methods::SLOTS => to_value(&local::slots(&server).await?),
            _ => Ok(local_noop(method, &params)),
        }
    }

    /// Stop the local model now (shutdown).
    pub async fn shutdown(&self) {
        if let Some(server) = self.server.lock().await.take() {
            server.stop();
        }
    }
}

fn no_gpu() -> RpcError {
    RpcError::new(codes::UNAVAILABLE, "this machine has no GPU with 16 GB to run Qwen (CUDA or Vulkan), and no member of the mesh lends one now").retryable()
}

/// The slots of a Qwen that is not loaded: all free.
fn idle_slots(parallel: u32) -> SlotsResult {
    SlotsResult {
        slots: (0..parallel).map(|slot| SlotState { slot, cache_key: None, busy: false, n_tokens: 0, priority: None }).collect(),
        queued: 0,
        hibernated: Vec::new(),
        queued_by_priority: Default::default(),
        busy: 0,
        pool_tokens: 0,
        reserved_tokens: 0,
        metrics: None,
    }
}

/// `model.hibernate` and `model.hold` locally: llama-server keeps each slot's prefix by itself.
fn local_noop(method: &str, params: &Value) -> Value {
    if method == methods::HOLD
        && let Ok(hold) = serde_json::from_value::<HoldParams>(params.clone())
    {
        return json!({"cache_key": hold.cache_key, "hold": hold.hold});
    }
    json!({"cache_key": params["cache_key"], "hibernated": false})
}
