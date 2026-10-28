//! The loaded model and every request path of `model@1`: installation, memory fit (from the
//! machine, never waiting), loading the engine, templating and output parsing, hibernation, variants and
//! switching.

use crate::constraint::{self, Constraint, ToolChoice};
use crate::engine::{self, Command, EngineConfig, EngineHandle, Event, Finish, Hibernated, Outcome, SamplingPlan, ThinkBudget, Work};
use crate::fit::{self, Budget, Geometry, Plan};
use crate::machine::Machine;
use crate::metrics::Metrics;
use crate::parse::{Detokenizer, OutputParser, Piece, StopScanner};
use crate::repeat::Guard;
use crate::template::{self, ChatTemplate};
use crate::variants::{self, Progress, Spec, VerifyCache};
use crate::vision::prompt::{ImagePart, PromptImage};
use crate::vision::{self, VisionConfig};
use cc_protocol::contracts::model::{
    CacheKeyParams, ChatDelta, ChatMessage, ChatParams, ChatResult, CompleteParams, ContentPart, CountResult, HoldParams, ModelInfo,
    PrewarmParams, Priority, Sampling, SelectResult, SlotsResult, TokenizeParams, TokenizeResult, Usage, Variant,
    VariantsResult, VisionInfo,
};
use cc_protocol::{ItemSink, RpcError, codes};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
use uzu_engine::engine::language_model::stream::SamplingMethod;

/// The uzu source this plugin embeds (vendor/uzu, see vendor/uzu/REVISION).
pub const ENGINE_REVISION: &str = "uzu 0.6.7 (391abc29abb26f18aa812f6f02e8b023aed11364) + CortexCode patches P1-P7";
/// Minimum context one conversation needs for an agent turn (config `min_context_per_slot`).
pub const DEFAULT_MIN_CONTEXT_PER_SLOT: u64 = 32_768;
/// Reasoning budget (tokens) per thinking effort when the caller names none: the model is made to
/// close its reasoning and answer once a block reaches it (Qwen's documented thinking budget).
pub fn default_think_budget(effort: Option<&str>) -> u32 {
    match effort {
        Some("low") => 2048,
        Some("medium") => 4096,
        Some("xhigh") => 16384,
        _ => 8192,
    }
}

/// What ends reasoning at the budget: Qwen's own closing sentence for a thinking budget.
pub const THINK_BUDGET_CLOSE: &str =
    "\n\nConsidering the limited time by the user, I have to give the solution based on the thinking directly now.\n</think>\n\n";

/// `[plugin.config]` of the model plugin.
#[derive(Debug, Clone, Deserialize, Default, Serialize)]
pub struct Config {
    /// Variant id from the catalog (default `qwen3.8-27b-m`).
    #[serde(default)]
    pub variant: Option<String>,
    /// Where variants are installed (default `$CORTEX_HOME/models`).
    #[serde(default)]
    pub models_dir: Option<String>,
    /// `auto` (draft with the model's speculator when it has one, default) | `off`.
    #[serde(default)]
    pub speculative: Option<String>,
    /// An operator cap on resident conversations. Default: none; as many are resident as the
    /// shared pool holds with their contexts (idle ones are spilled to disk when it is full).
    #[serde(default)]
    pub slots: Option<u32>,
    /// An operator override of the fitted pool (tokens).
    #[serde(default)]
    pub ctx_total: Option<u64>,
    /// Prompt tokens per prefill step (default 1024).
    #[serde(default)]
    pub prefill_chunk: Option<usize>,
    /// Upper bound of the memory the plugin may plan for (operator limit; default: the fit's budget).
    #[serde(default)]
    pub memory_limit_bytes: Option<u64>,
    /// Minimum context one conversation needs (default 32768); a machine whose pool cannot give one is a load error.
    #[serde(default)]
    pub min_context_per_slot: Option<u64>,
    /// Image input: `auto` (default: the model's vision tower is used when installed; `model.select`
    /// installs it) | `off` (text only; the tower is never loaded or installed).
    #[serde(default)]
    pub vision: Option<String>,
    /// Seconds without an image after which the vision tower is unloaded (default 300).
    #[serde(default)]
    pub vision_idle_unload_s: Option<u64>,
    /// Seconds a conversation stays resident after its last request ended before it is spilled to
    /// disk (default 30; 0: kept until the pool needs the room).
    #[serde(default)]
    pub idle_spill_s: Option<u64>,
    /// Bytes the spilled conversations may take on disk; the oldest beyond it are deleted (default 32 GiB; 0: unbounded).
    #[serde(default)]
    pub hibernate_limit_bytes: Option<u64>,
    /// Pixel budget per image; larger images are downscaled (default 2,400,000, about 2,340 tokens).
    #[serde(default)]
    pub max_image_pixels: Option<u64>,
    /// Keys this plugin does not use (the frame may add machine facts); logged, not rejected.
    #[serde(flatten)]
    pub other: serde_json::Map<String, Value>,
}

#[derive(Debug, Clone)]
pub struct Paths {
    pub home: PathBuf,
    pub data_dir: PathBuf,
    pub models_dir: PathBuf,
}

pub fn expand_home(path: &str) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/")
        && let Some(home) = std::env::var_os("HOME")
    {
        return PathBuf::from(home).join(rest);
    }
    PathBuf::from(path)
}

pub type Publisher = engine::Publisher;

pub struct Loaded {
    pub engine: Arc<EngineHandle>,
    pub variant: String,
    pub spec: &'static Spec,
    pub dir: PathBuf,
    pub geometry: Geometry,
    pub plan: Plan,
    pub template: ChatTemplate,
    pub identity: String,
    pub speculation: bool,
    pub machine: Machine,
    pub metrics: Arc<Metrics>,
    pub load_ms: f64,
    pub think_end: Vec<u64>,
    /// [`THINK_BUDGET_CLOSE`] as tokens.
    pub think_close: Vec<u64>,
    /// Image input of this load.
    pub vision: VisionSupport,
}

/// Image input of a load: the variant's vision tower (when it has one and vision is not off). The
/// tower's files may be installed after the load (by `model.select`); availability is checked when
/// asked, and the verified configuration is kept once read.
pub struct VisionSupport {
    pub tower: Option<&'static vision::TowerSpec>,
    /// `<model dir>/vision` when the variant has a tower and vision is enabled.
    pub dir: Option<PathBuf>,
    pub max_pixels: u64,
    pub idle_unload_s: u64,
    /// Why images are not accepted, when the configuration rules them out.
    pub disabled: Option<String>,
    verify_cache: PathBuf,
    config: std::sync::Mutex<Option<Arc<VisionConfig>>>,
}

impl VisionSupport {
    /// The verified configuration when the tower is installed (hashing it once if no record says so).
    pub fn ready(&self) -> Result<Arc<VisionConfig>, String> {
        if let Some(config) = self.config.lock().ok().and_then(|config| config.clone()) {
            return Ok(config);
        }
        if let Some(reason) = &self.disabled {
            return Err(reason.clone());
        }
        let (Some(tower), Some(dir)) = (self.tower, self.dir.as_ref()) else {
            return Err("this model has no vision tower".into());
        };
        let variant_dir = dir.parent().unwrap_or(dir);
        if !vision::install::installed(tower, variant_dir) {
            return Err("the vision tower is not installed (model.select installs it, or `cc-model-uzu install-vision`)".into());
        }
        let cache = VerifyCache::at(self.verify_cache.clone());
        let weights = dir.join(vision::WEIGHTS);
        let digest = match cache.recorded(&weights) {
            Some(digest) => digest,
            None => {
                let digest = variants::digest_file(&weights, true, &mut |_, _| {})?;
                cache.record_digest(&weights, &digest);
                digest
            }
        };
        if !tower.sha256.is_empty() && digest != format!("sha256:{}", tower.sha256) {
            return Err(format!("{} has {digest}, expected sha256:{}", weights.display(), tower.sha256));
        }
        let config = Arc::new(VisionConfig::read(dir)?);
        if let Ok(mut slot) = self.config.lock() {
            *slot = Some(config.clone());
        }
        Ok(config)
    }

    /// The configuration when it was read already.
    pub fn cached(&self) -> Option<Arc<VisionConfig>> {
        self.config.lock().ok().and_then(|config| config.clone())
    }

    /// Whether images are accepted: the tower is enabled and installed (cheap; its digest is
    /// verified by [`VisionSupport::ready`] when images are first used).
    pub fn available(&self) -> Result<(), String> {
        if self.config.lock().ok().is_some_and(|config| config.is_some()) {
            return Ok(());
        }
        if let Some(reason) = &self.disabled {
            return Err(reason.clone());
        }
        let (Some(tower), Some(dir)) = (self.tower, self.dir.as_ref()) else {
            return Err("this model has no vision tower".into());
        };
        if !vision::install::installed(tower, dir.parent().unwrap_or(dir)) {
            return Err("the vision tower is not installed (model.select installs it, or `cc-model-uzu install-vision`)".into());
        }
        Ok(())
    }
}

/// A prompt ready for the engine: its tokens, the images among them, and its reuse points.
pub struct Prompt {
    pub tokens: Vec<u64>,
    pub images: Vec<PromptImage>,
    pub vision: Option<Arc<VisionConfig>>,
    /// Prompt lengths the engine checkpoints while prefilling (see [`reuse_points`]).
    pub checkpoints: Vec<usize>,
    /// The reuse point that ends the leading system block (one of `checkpoints`), which other
    /// conversations share (see `engine::Request::shared`).
    pub shared: Option<usize>,
    /// Prefill only up to `shared`, for other conversations (a prewarm's `shared_only`).
    pub shared_only: bool,
    /// Run only while no other conversation is served (a prewarm's `idle_only`).
    pub idle_only: bool,
}

impl Prompt {
    pub fn text(tokens: Vec<u64>) -> Self {
        Self { tokens, images: Vec::new(), vision: None, checkpoints: Vec::new(), shared: None, shared_only: false, idle_only: false }
    }
}

/// Where a conversation's later requests can resume: the end of its leading system block and the
/// end of its history before the last message, as token counts of `tokens` (the rendered prompt).
///
/// The last message is the one a caller replaces per call (a stage message, live data) or follows
/// with the answer, so the next request shares the history up to it. Each point is found by
/// rendering the messages before it twice, followed by a user and by an assistant message: the
/// tokens the two renders share are exactly what any continuation renders there, whatever the
/// template does with what comes next (Qwen's opens the next turn with `<|im_start|>`; a template
/// that renders the history differently by what follows yields a shorter, still exact, point).
/// Mechanical, from the template itself: no knowledge of roles' markup or of the caller.
pub fn reuse_points(tokens: &[u64], prefixes: &[&[Value]], render: &dyn Fn(&[Value]) -> Option<Vec<u64>>) -> Vec<usize> {
    let mut points = Vec::new();
    for prefix in prefixes {
        if prefix.is_empty() {
            continue;
        }
        let continued = |message: Value| {
            let mut messages = prefix.to_vec();
            messages.push(message);
            render(&messages)
        };
        let (Some(user), Some(assistant)) =
            (continued(json!({"role": "user", "content": "A"})), continued(json!({"role": "assistant", "content": "A"})))
        else {
            continue;
        };
        let shared = |a: &[u64], b: &[u64]| a.iter().zip(b).take_while(|(x, y)| x == y).count();
        // Within what the prompt itself tokenized identically (a merge across the point never counts).
        let point = shared(&user, &assistant).min(shared(&user, tokens));
        if point > 0 && point < tokens.len() {
            points.push(point);
        }
    }
    points.sort_unstable();
    points.dedup();
    points
}

/// Reuse points counted before the images' placeholders were expanded, in the expanded prompt
/// (`images` in prompt order): every image before a point moves it by its run less the placeholder.
pub fn expanded_points(points: &[usize], images: &[PromptImage]) -> Vec<usize> {
    points
        .iter()
        .map(|&point| images.iter().fold(point, |point, image| if image.start < point { point + image.tokens() - 1 } else { point }))
        .collect()
}

fn has_images(messages: &[ChatMessage]) -> bool {
    messages.iter().any(|message| message.parts.iter().any(|part| matches!(part, ContentPart::Image { .. })))
}

#[derive(Clone)]
enum Phase {
    Starting,
    Installing(Progress),
    Loading(String),
    Ready(Arc<Loaded>),
    Failed(String),
}

pub struct Runtime {
    pub paths: Paths,
    pub config: Config,
    publisher: Publisher,
    phase: watch::Sender<Phase>,
    switch: tokio::sync::Mutex<()>,
    /// Conversations a predecessor hibernated, adopted once the model is loaded.
    adopted: std::sync::Mutex<Vec<Hibernated>>,
}

/// Sets the request's stop flag when the owning future is dropped (cancellation).
struct StopOnDrop(Arc<AtomicBool>);

impl Drop for StopOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

fn unavailable(message: impl Into<String>) -> RpcError {
    RpcError::new(codes::UNAVAILABLE, message).retryable()
}

/// Why a stream ended early on the caller's side.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Ended {
    StopString,
    Repeated,
}

/// The contract's sampling as uzu's (structural mapping; documented in the README).
pub fn sampling_plan(sampling: &Sampling) -> SamplingPlan {
    let seed = sampling.seed.unwrap_or_else(|| {
        let mut bytes = [0u8; 8];
        bytes.copy_from_slice(&uuid::Uuid::new_v4().as_bytes()[..8]);
        u64::from_le_bytes(bytes)
    });
    if sampling.temperature <= 0.0 {
        return SamplingPlan { method: SamplingMethod::Greedy, seed, penalty_window_includes_state: true, allows_speculation: true };
    }
    let repetition = sampling.repeat_penalty.filter(|penalty| (*penalty - 1.0).abs() > f32::EPSILON);
    let presence = Some(sampling.presence_penalty).filter(|penalty| penalty.abs() > f32::EPSILON);
    let window = if presence.is_some() {
        Some(engine::PRESENCE_WINDOW)
    } else if repetition.is_some() {
        Some(engine::REPETITION_WINDOW)
    } else {
        None
    };
    SamplingPlan {
        method: SamplingMethod::Stochastic {
            temperature: Some(sampling.temperature),
            top_k: (sampling.top_k > 0).then_some(sampling.top_k),
            top_p: (sampling.top_p > 0.0 && sampling.top_p < 1.0).then_some(sampling.top_p),
            min_p: (sampling.min_p > 0.0).then_some(sampling.min_p),
            repetition_penalty: repetition,
            presence_penalty: presence,
            suffix_repetition_length: window,
        },
        seed,
        // A presence penalty applies to the generated tokens (as in vLLM and the OpenAI API).
        penalty_window_includes_state: presence.is_none(),
        allows_speculation: window.is_none(),
    }
}

/// What one generation produced, before it becomes a contract result.
struct Generated {
    outcome: Outcome,
    parser: OutputParser,
    ended: Option<Ended>,
}

impl Runtime {
    pub fn new(paths: Paths, config: Config, publisher: Publisher) -> Arc<Self> {
        let (phase, _) = watch::channel(Phase::Starting);
        Arc::new(Self {
            paths,
            config,
            publisher,
            phase,
            switch: tokio::sync::Mutex::new(()),
            adopted: std::sync::Mutex::new(Vec::new()),
        })
    }

    fn publish(&self, topic: &str, data: Value) {
        (self.publisher)(topic, data);
    }

    fn set_phase(&self, phase: Phase) {
        let state = match &phase {
            Phase::Starting => json!({"state": "starting"}),
            Phase::Installing(progress) => json!({"state": "installing", "progress": progress}),
            Phase::Loading(variant) => json!({"state": "loading", "variant": variant}),
            Phase::Ready(loaded) => json!({"state": "ready", "variant": loaded.variant}),
            Phase::Failed(error) => json!({"state": "failed", "error": error}),
        };
        // Every progress tick is a `model.install` event; the state changes only on entering it.
        let entering = !matches!(phase, Phase::Installing(_)) || !matches!(&*self.phase.borrow(), Phase::Installing(_));
        if entering {
            self.publish("model.state", state);
        }
        self.phase.send_replace(phase);
    }

    /// Load the configured variant (called once at start, in the background).
    pub async fn start(self: &Arc<Self>) {
        let _switch = self.switch.lock().await;
        let variant = self.config.variant.clone().unwrap_or_else(|| variants::DEFAULT_VARIANT.to_owned());
        // A first start has no model on this Mac: it is downloaded now, its progress shown as
        // `model.install` events (the card and the model settings show them), instead of a load
        // that fails and leaves the app without a model.
        match self.load(&variant, true).await {
            Ok(loaded) => self.set_phase(Phase::Ready(loaded)),
            Err(error) => {
                tracing::error!(%error, "model load failed");
                self.set_phase(Phase::Failed(error));
            }
        }
    }

    /// The loaded model, once it has loaded (or switched).
    pub async fn ready(&self) -> Result<Arc<Loaded>, RpcError> {
        let mut rx = self.phase.subscribe();
        loop {
            match &*rx.borrow_and_update() {
                Phase::Ready(loaded) => return Ok(loaded.clone()),
                Phase::Failed(error) => return Err(unavailable(format!("the model failed to load: {error}"))),
                _ => {}
            }
            if rx.changed().await.is_err() {
                return Err(unavailable("the model runtime stopped"));
            }
        }
    }

    /// The download under way, if the model is being installed (no model to serve yet).
    pub async fn installing_or_ready(&self) -> Result<Result<Arc<Loaded>, Progress>, RpcError> {
        let mut rx = self.phase.subscribe();
        loop {
            match &*rx.borrow_and_update() {
                Phase::Ready(loaded) => return Ok(Ok(loaded.clone())),
                Phase::Installing(progress) => return Ok(Err(progress.clone())),
                Phase::Failed(error) => return Err(unavailable(format!("the model failed to load: {error}"))),
                _ => {}
            }
            if rx.changed().await.is_err() {
                return Err(unavailable("the model runtime stopped"));
            }
        }
    }

    pub fn current(&self) -> Option<Arc<Loaded>> {
        match &*self.phase.borrow() {
            Phase::Ready(loaded) => Some(loaded.clone()),
            _ => None,
        }
    }

    fn install_progress(self: &Arc<Self>) -> impl Fn(Progress) + Send + Sync + 'static {
        let runtime = Arc::downgrade(self);
        move |progress: Progress| {
            if let Some(runtime) = runtime.upgrade() {
                runtime.publish("model.install", serde_json::to_value(&progress).unwrap_or(Value::Null));
                if progress.phase == "download" || progress.phase == "verify" {
                    // Downloads before a switch (and optional vision installs) leave the
                    // current engine serving. Progress has its own `model.install` event.
                    let entered = runtime.phase.send_if_modified(|phase| {
                        if matches!(phase, Phase::Ready(_)) {
                            return false;
                        }
                        let entering = !matches!(phase, Phase::Installing(_));
                        *phase = Phase::Installing(progress.clone());
                        entering
                    });
                    if entered {
                        runtime.publish("model.state", json!({"state": "installing", "variant": progress.variant}));
                    }
                }
            }
        }
    }

    /// Install (when `download` allows), verify, fit and load a variant.
    async fn load(self: &Arc<Self>, variant: &str, download: bool) -> Result<Arc<Loaded>, String> {
        let spec = variants::find(variant).ok_or_else(|| {
            format!(
                "unknown variant `{variant}`; known: {}",
                variants::CATALOG.iter().map(|spec| spec.id).collect::<Vec<_>>().join(", ")
            )
        })?;
        let dir = spec.local_dir(&self.paths.models_dir);
        let cache = VerifyCache::new(&self.paths.models_dir);
        let was_installed = variants::installed(spec, &dir);
        if was_installed || download {
            // Downloads only what is missing; verifies every file (records make this instant).
            let progress = self.install_progress();
            variants::install(spec, &self.paths.models_dir, &cache, &progress).await?;
            // A model downloaded now brings its vision tower in the same step (a first start
            // downloaded the model alone, and nothing fetched the tower later).
            if !was_installed {
                self.install_vision_or_warn(spec).await;
            }
        } else {
            return Err(format!(
                "variant `{variant}` is not installed in {} (select it with model.select to download it, or run `cc-model-uzu install {variant}`)",
                self.paths.models_dir.display()
            ));
        }
        self.set_phase(Phase::Loading(variant.to_owned()));
        let vision_support = self.vision_support(spec, &dir)?;
        // The plan never reserves the vision tower: it is loaded only when an image needs it, after
        // a check of the device's actual headroom (engine.rs).
        let geometry = Geometry::read(&dir)?;
        let plan = self.fit(&geometry)?;
        let speculation = match self.config.speculative.as_deref().unwrap_or("auto") {
            "auto" | "on" => geometry.has_speculator,
            "off" => false,
            other => return Err(format!("unknown speculative mode `{other}` (auto | off)")),
        };
        let identity = format!("{}@{}", spec.repo, spec.revision);
        let hibernate_dir = self.paths.data_dir.join("hibernated");
        let metrics = Arc::new(Metrics::default());
        let state_capacity = u32::try_from(plan.ctx_total.min(geometry.native_context)).unwrap_or(u32::MAX);
        let config = EngineConfig {
            model_dir: dir.clone(),
            state_capacity,
            pool_tokens: plan.ctx_total,
            max_resident: self.config.slots.map(|cap| cap.max(1) as usize),
            conversation_tokens: plan.conversation_tokens,
            speculation,
            prefill_chunk: self.config.prefill_chunk.unwrap_or(engine::PREFILL_CHUNK).max(16),
            hibernate_dir,
            idle_spill: Duration::from_secs(self.config.idle_spill_s.unwrap_or(engine::DEFAULT_IDLE_SPILL_S)),
            hibernate_limit: self.config.hibernate_limit_bytes.unwrap_or(engine::DEFAULT_HIBERNATE_LIMIT),
            checkpoints: usize::try_from(plan.checkpoints).unwrap_or(usize::MAX),
            identity: identity.clone(),
            vision_dir: vision_support.dir.clone(),
            vision_max_pixels: vision_support.max_pixels,
            vision_idle_unload: Duration::from_secs(vision_support.idle_unload_s),
            memory_budget: Arc::new(std::sync::atomic::AtomicU64::new(plan.budget.usable())),
            bytes_per_token: plan.bytes_per_token,
            refit: {
                let runtime = self.clone();
                Arc::new(move |kv_bytes_per_token| runtime.fit(&Geometry { kv_bytes_per_token, ..geometry }))
            },
        };
        let template = ChatTemplate::new(
            template::template_source(spec.template).ok_or_else(|| format!("template {} is not shipped", spec.template))?,
        )?;
        let started = Instant::now();
        let publisher = self.publisher.clone();
        let costs: Arc<dyn engine::CostSink> = metrics.clone();
        let handle = tokio::task::spawn_blocking(move || engine::start(config, publisher, costs))
            .await
            .map_err(|error| format!("engine start task failed: {error}"))??;
        // The engine sized its pool by the KV format uzu chose at load; report that plan.
        let geometry = Geometry { kv_bytes_per_token: handle.loaded.kv_bytes_per_token, ..geometry };
        let plan = handle.loaded.plan.clone();
        let think_end: Vec<u64> = handle
            .tokenizer
            .encode("</think>", false)
            .map(|encoding| encoding.get_ids().iter().map(|id| u64::from(*id)).collect())
            .unwrap_or_default();
        let think_close: Vec<u64> = handle
            .tokenizer
            .encode(THINK_BUDGET_CLOSE, false)
            .map(|encoding| encoding.get_ids().iter().map(|id| u64::from(*id)).collect())
            .unwrap_or_default();
        let adopted = self.adopted.lock().map(|mut records| std::mem::take(&mut *records)).unwrap_or_default();
        if !adopted.is_empty() {
            let _ = handle.send(Command::Adopt(adopted));
        }
        let loaded = Arc::new(Loaded {
            engine: Arc::new(handle),
            variant: variant.to_owned(),
            spec,
            dir,
            geometry,
            plan,
            template,
            identity,
            speculation,
            machine: Machine::measure(),
            metrics,
            load_ms: started.elapsed().as_secs_f64() * 1000.0,
            think_end,
            think_close,
            vision: vision_support,
        });
        Ok(loaded)
    }

    fn vision_mode(&self) -> Result<bool, String> {
        match self.config.vision.as_deref().unwrap_or("auto") {
            "auto" | "on" => Ok(true),
            "off" => Ok(false),
            other => Err(format!("unknown vision mode `{other}` (auto | off)")),
        }
    }

    fn vision_support(&self, spec: &'static Spec, dir: &std::path::Path) -> Result<VisionSupport, String> {
        let enabled = self.vision_mode()?;
        let max_pixels = self.config.max_image_pixels.unwrap_or(vision::DEFAULT_MAX_IMAGE_PIXELS).max(1024);
        Ok(VisionSupport {
            tower: spec.vision,
            dir: (enabled && spec.vision.is_some()).then(|| vision::install::tower_dir(dir)),
            max_pixels,
            idle_unload_s: self.config.vision_idle_unload_s.unwrap_or(vision::DEFAULT_IDLE_UNLOAD_S),
            disabled: (!enabled).then(|| "image input is switched off (plugin config `vision = \"off\"`)".to_owned()),
            verify_cache: VerifyCache::new(&self.paths.models_dir).path().to_path_buf(),
            config: std::sync::Mutex::new(None),
        })
    }

    /// [`Self::install_vision`], where a failure (e.g. offline) leaves the model text-only and does
    /// not stop the caller; `model.install` and `model.info` say why.
    async fn install_vision_or_warn(self: &Arc<Self>, spec: &'static Spec) {
        if let Err(error) = self.install_vision(spec).await {
            tracing::warn!(%error, variant = spec.id, "the vision tower could not be installed; images stay unavailable");
            self.publish("model.install", json!({"variant": spec.id, "phase": "failed", "file": "vision", "detail": error}));
        }
    }

    /// Install the variant's vision tower when it has one, vision is enabled and it is missing.
    async fn install_vision(self: &Arc<Self>, spec: &'static Spec) -> Result<(), String> {
        let Some(tower) = spec.vision else { return Ok(()) };
        if !self.vision_mode()? {
            return Ok(());
        }
        let dir = spec.local_dir(&self.paths.models_dir);
        if vision::install::installed(tower, &dir) {
            return Ok(());
        }
        let cache = VerifyCache::new(&self.paths.models_dir);
        let progress = self.install_progress();
        vision::install::install(tower, spec.id, &dir, &cache, &progress).await.map(|_| ())
    }

    /// The memory fit, from the machine itself (the GPU's working-set limit, physical RAM less a
    /// quarter and the kernel's wired limit less an eighth of RAM), never from what happens to be free: the model loads at once. The pool is shared, so
    /// no conversation count is planned; a machine whose pool cannot hold one conversation with the
    /// minimum agent context is a plain load error.
    fn fit(&self, geometry: &Geometry) -> Result<Plan, String> {
        let min = self.config.min_context_per_slot.unwrap_or(DEFAULT_MIN_CONTEXT_PER_SLOT);
        let machine = Machine::measure();
        let budget = Budget::new(metal_working_set(), machine.physical_bytes, machine.wire_limit_bytes, self.config.memory_limit_bytes);
        let mut plan = fit::fit(geometry, &budget);
        if let Some(ctx) = self.config.ctx_total {
            plan.ctx_total = ctx;
        }
        if plan.ctx_total < min.min(geometry.native_context) + plan.conversation_tokens {
            return Err(format!(
                "this machine's memory leaves a {}-token pool, below one conversation's state and the {}-token minimum an agent turn needs",
                plan.ctx_total, min
            ));
        }
        Ok(plan)
    }

    /// Conversations resident at once at most: the pool's own bound, or the operator's cap.
    pub fn resident_capacity(&self, plan: &Plan) -> u64 {
        let capacity = plan.resident_capacity();
        self.config.slots.map_or(capacity, |cap| capacity.min(u64::from(cap.max(1))))
    }

    // ---- request paths ------------------------------------------------------------------------

    /// Render and tokenize a conversation. Image parts become their pad-token runs, sized from the
    /// image dimensions alone (the part's width and height, else the file header): nothing is
    /// encoded here.
    async fn render(&self, loaded: &Arc<Loaded>, messages: &[ChatMessage], tools: &[cc_protocol::contracts::model::ToolSpec], thinking: &cc_protocol::contracts::model::Thinking) -> Result<Prompt, RpcError> {
        if messages.is_empty() {
            return Err(RpcError::invalid_params("messages is empty"));
        }
        let tool_values: Vec<Value> = tools.iter().map(template::tool_value).collect();
        // The effort announcement goes after every reuse point (see `ChatTemplate::rendering`).
        let template::Rendering { messages: values, vars } =
            loaded.template.rendering(messages.iter().map(template::message_value).collect(), &tool_values, thinking)?;
        let text = loaded.template.render(&values, &tool_values, &vars)?;
        let tokens = self.encode(loaded, &text)?;
        let render = |messages: &[Value]| -> Option<Vec<u64>> {
            let text = loaded.template.render(messages, &tool_values, &vars).ok()?;
            self.encode(loaded, &text).ok()
        };
        // The leading system block (the template merges leading system messages into one), and the
        // history before the last message.
        let lead = values.iter().take_while(|value| value["role"] == "system" || value["role"] == "developer").count();
        let shared = reuse_points(&tokens, &[&values[..lead]], &render).first().copied();
        let mut checkpoints = reuse_points(&tokens, &[&values[..values.len() - 1]], &render);
        checkpoints.extend(shared);
        checkpoints.sort_unstable();
        checkpoints.dedup();
        if !has_images(messages) {
            return Ok(Prompt { checkpoints, shared, ..Prompt::text(tokens) });
        }
        let support = loaded.clone();
        let vision = tokio::task::spawn_blocking(move || support.vision.ready())
            .await
            .map_err(|error| RpcError::internal(error.to_string()))?
            .map_err(|reason| RpcError::invalid_params(format!("this model cannot take images now: {reason}")))?;
        let mut parts = Vec::new();
        for part in messages.iter().flat_map(|message| message.parts.iter()) {
            if let ContentPart::Image { sha256, path, width, height, .. } = part {
                let path = expand_home(path);
                let (width, height) = match (width, height) {
                    (Some(width), Some(height)) => (*width, *height),
                    _ => {
                        let bytes = std::fs::read(&path)
                            .map_err(|error| RpcError::invalid_params(format!("cannot read image {}: {error}", path.display())))?;
                        vision::preprocess::dimensions(&bytes).map_err(RpcError::invalid_params)?
                    }
                };
                let grid = vision::preprocess::merged_grid(&vision, width, height, loaded.vision.max_pixels)
                    .map_err(RpcError::invalid_params)?;
                parts.push(ImagePart { sha256: sha256.to_lowercase(), path, grid });
            }
        }
        let (tokens, images) =
            vision::prompt::expand(&tokens, vision.image_token_id, &parts).map_err(RpcError::invalid_params)?;
        let checkpoints = expanded_points(&checkpoints, &images);
        let shared = shared.and_then(|point| expanded_points(&[point], &images).first().copied());
        Ok(Prompt { tokens, images, vision: Some(vision), checkpoints, shared, shared_only: false, idle_only: false })
    }

    fn encode(&self, loaded: &Loaded, text: &str) -> Result<Vec<u64>, RpcError> {
        loaded
            .engine
            .tokenizer
            .encode(text, false)
            .map(|encoding| encoding.get_ids().iter().map(|id| u64::from(*id)).collect())
            .map_err(|error| RpcError::internal(format!("tokenizer: {error}")))
    }

    /// Run one generation, feeding parsed pieces to `sink`.
    #[allow(clippy::too_many_arguments)]
    async fn generate(
        &self,
        loaded: &Arc<Loaded>,
        key: String,
        prompt: Prompt,
        priority: Priority,
        work: Work,
        mut parser: OutputParser,
        stops: &[String],
        sink: &mut (dyn FnMut(&Piece) + Send),
        cancel: &CancellationToken,
    ) -> Result<Generated, RpcError> {
        let (events, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let stop = Arc::new(AtomicBool::new(false));
        let _stop_on_drop = StopOnDrop(stop.clone());
        let Prompt { tokens: prompt, images, vision, checkpoints, shared, shared_only, idle_only } = prompt;
        let request = engine::Request {
            id: 0,
            key,
            prompt,
            checkpoints,
            shared,
            shared_only,
            idle_only,
            images,
            vision,
            priority,
            work,
            events,
            stop: stop.clone(),
            arrival: Instant::now(),
        };
        loaded.engine.send(Command::Run(Box::new(request))).map_err(unavailable)?;
        let mut detokenizer = Detokenizer::new(loaded.engine.tokenizer.clone());
        let mut scanner = StopScanner::new(stops);
        let mut guards = [Guard::default(), Guard::default(), Guard::default()];
        let mut ended = None;
        let mut feed = |text: &str, parser: &mut OutputParser, ended: &mut Option<Ended>, stop: &AtomicBool| {
            let passed = scanner.push(text);
            if scanner.stopped && ended.is_none() {
                *ended = Some(Ended::StopString);
                stop.store(true, Ordering::SeqCst);
            }
            for piece in parser.push(&passed) {
                let (guard, text) = match &piece {
                    Piece::Reasoning(text) => (0, text.as_str()),
                    Piece::Content(text) => (1, text.as_str()),
                    Piece::ToolArgs { text, .. } => (2, text.as_str()),
                    Piece::ToolStart { .. } => (3, ""),
                };
                if guard < 3 && guards[guard].push(text) && ended.is_none() {
                    *ended = Some(Ended::Repeated);
                    stop.store(true, Ordering::SeqCst);
                }
                sink(&piece);
            }
        };
        loop {
            let event = tokio::select! {
                event = rx.recv() => event,
                _ = cancel.cancelled() => {
                    stop.store(true, Ordering::SeqCst);
                    return Err(RpcError::cancelled());
                }
            };
            match event {
                Some(Event::Started { .. }) => {}
                Some(Event::Token(token)) => {
                    if ended.is_some() {
                        continue;
                    }
                    let text = detokenizer.push(token);
                    if !text.is_empty() {
                        feed(&text, &mut parser, &mut ended, &stop);
                    }
                }
                Some(Event::Done(outcome)) => {
                    if ended.is_none() {
                        let rest = detokenizer.flush();
                        feed(&rest, &mut parser, &mut ended, &stop);
                        let held = scanner.flush();
                        for piece in parser.push(&held) {
                            sink(&piece);
                        }
                    }
                    for piece in parser.finish() {
                        sink(&piece);
                    }
                    let usage = usage_of(&outcome);
                    loaded.metrics.request(&usage);
                    return Ok(Generated { outcome, parser, ended });
                }
                Some(Event::Failed(failure)) => {
                    return Err(match failure {
                        engine::Failure::Invalid(message) => RpcError::invalid_params(message),
                        engine::Failure::Unavailable(message) => unavailable(message),
                        engine::Failure::Cancelled => RpcError::cancelled(),
                        engine::Failure::Engine(message) => RpcError::internal(message),
                    });
                }
                None => return Err(unavailable("the engine stopped")),
            }
        }
    }

    pub async fn chat(
        &self,
        params: ChatParams,
        items: Option<ItemSink>,
        cancel: CancellationToken,
    ) -> Result<ChatResult, RpcError> {
        let mut sink = move |piece: &Piece| {
            if let Some(items) = &items {
                let delta = match piece {
                    Piece::Reasoning(text) => ChatDelta { kind: "reasoning".into(), text: text.clone(), index: None },
                    Piece::Content(text) => ChatDelta { kind: "content".into(), text: text.clone(), index: None },
                    Piece::ToolArgs { index, text } => ChatDelta { kind: "tool_call".into(), text: text.clone(), index: Some(*index) },
                    Piece::ToolStart { .. } => return,
                };
                if let Ok(value) = serde_json::to_value(&delta) {
                    items.send(value);
                }
            }
        };
        self.chat_with(params, &mut sink, cancel).await
    }

    /// `model.chat` with pieces delivered to `sink`.
    pub async fn chat_with(
        &self,
        params: ChatParams,
        sink: &mut (dyn FnMut(&Piece) + Send),
        cancel: CancellationToken,
    ) -> Result<ChatResult, RpcError> {
        let loaded = self.ready().await?;
        let choice = constraint::tool_choice(params.tool_choice.as_ref(), &params.tools)?;
        let mut constraints = Vec::new();
        if let Some(schema) = &params.json_schema {
            constraints.push(Constraint::JsonSchema(schema.to_string()));
        }
        if let Some(grammar) = &params.grammar {
            constraints.push(Constraint::Gbnf(grammar.clone()));
        }
        match &choice {
            ToolChoice::Required => constraints.push(Constraint::Lark(constraint::tool_call_grammar(&params.tools, None))),
            ToolChoice::Named(name) => {
                constraints.push(Constraint::Lark(constraint::tool_call_grammar(&params.tools, Some(name))))
            }
            // With tools listed and no other constraint, `none` is enforced: no tool-call token.
            ToolChoice::None if !params.tools.is_empty() && constraints.is_empty() => {
                constraints.push(Constraint::Lark(constraint::no_tool_call_grammar()))
            }
            ToolChoice::Auto | ToolChoice::None => {}
        }
        if constraints.len() > 1 {
            return Err(RpcError::invalid_params(
                "json_schema, grammar and a forcing tool_choice are exclusive; send one constraint",
            ));
        }
        let mut prompt = self.render(&loaded, &params.messages, &params.tools, &params.thinking).await?;
        prompt.idle_only = params.idle_only;
        let key = params.cache_key.clone().unwrap_or_else(|| {
            template::derived_key(&params.messages.iter().map(template::message_value).collect::<Vec<_>>())
        });
        let sampling = sampling_plan(&params.sampling);
        let parse_tools = !params.tools.is_empty() && choice != ToolChoice::None;
        let parser = OutputParser::new(params.thinking.enabled, parse_tools, &params.tools);
        let grammar = constraints.first().map(Constraint::config).transpose()?;
        let work = Work::Generate {
            max_tokens: params.max_tokens,
            sampling,
            grammar,
            grammar_trigger: params.thinking.enabled.then(|| loaded.think_end.clone()),
            think_budget: (params.thinking.enabled && !loaded.think_end.is_empty() && loaded.think_close.windows(loaded.think_end.len()).any(|w| w == loaded.think_end.as_slice())).then(|| ThinkBudget {
                tokens: params.thinking.budget_tokens.unwrap_or_else(|| default_think_budget(params.thinking.effort.as_deref())),
                end: loaded.think_end.clone(),
                close: loaded.think_close.clone(),
            }),
        };
        let generated =
            self.generate(&loaded, key, prompt, params.priority, work, parser, &params.stop, sink, &cancel).await?;
        Ok(chat_result(generated))
    }

    pub async fn complete(&self, params: CompleteParams, items: Option<ItemSink>, cancel: CancellationToken) -> Result<ChatResult, RpcError> {
        if params.n_probs.is_some() {
            return Err(RpcError::new(
                codes::INVALID_PARAMS,
                "n_probs is not available: uzu samples on the GPU and never hands logits to the host",
            ));
        }
        let loaded = self.ready().await?;
        let prompt = Prompt::text(self.encode(&loaded, &params.prompt)?);
        let key = params.cache_key.clone().unwrap_or_else(|| format!("complete:{}", uuid::Uuid::new_v4().simple()));
        let work = Work::Generate {
            max_tokens: params.max_tokens,
            sampling: sampling_plan(&params.sampling),
            grammar: params.grammar.clone().map(|grammar| Constraint::Gbnf(grammar).config()).transpose()?,
            grammar_trigger: None,
            think_budget: None,
        };
        let mut sink = move |piece: &Piece| {
            if let (Some(items), Piece::Content(text)) = (&items, piece)
                && let Ok(value) = serde_json::to_value(ChatDelta { kind: "content".into(), text: text.clone(), index: None })
            {
                items.send(value);
            }
        };
        // Raw text: no reasoning split, no tool parsing, and no trimming of the continuation.
        let parser = OutputParser::raw();
        let generated = self.generate(&loaded, key, prompt, params.priority, work, parser, &params.stop, &mut sink, &cancel).await?;
        Ok(chat_result(generated))
    }

    pub async fn prewarm(&self, params: PrewarmParams, cancel: CancellationToken) -> Result<Value, RpcError> {
        let loaded = self.ready().await?;
        let mut prompt = self.render(&loaded, &params.messages, &params.tools, &params.thinking).await?;
        if params.shared_only {
            if prompt.shared.is_none() {
                return Err(RpcError::invalid_params("shared_only: the messages start with no system block followed by another message"));
            }
            prompt.shared_only = true;
        }
        prompt.idle_only = params.idle_only;
        let mut sink = |_: &Piece| {};
        let generated = self
            .generate(&loaded, params.cache_key.clone(), prompt, Priority::Background, Work::Prewarm, OutputParser::raw(), &[], &mut sink, &cancel)
            .await?;
        let outcome = generated.outcome;
        Ok(json!({
            "cache_key": params.cache_key,
            "prompt_tokens": outcome.prompt_tokens,
            "cached_tokens": outcome.cached_tokens,
            "prefill_ms": outcome.prefill_ms,
            "queue_ms": outcome.queue_ms,
            "slot": outcome.slot,
        }))
    }

    pub async fn tokenize(&self, params: TokenizeParams) -> Result<TokenizeResult, RpcError> {
        let loaded = self.ready().await?;
        Ok(TokenizeResult { tokens: self.encode(&loaded, &params.text)?.into_iter().map(|id| id as i64).collect() })
    }

    pub async fn detokenize(&self, tokens: &[i64]) -> Result<String, RpcError> {
        let loaded = self.ready().await?;
        let ids: Vec<u32> = tokens.iter().filter_map(|id| u32::try_from(*id).ok()).collect();
        loaded.engine.tokenizer.decode(&ids, false).map_err(|error| RpcError::internal(error.to_string()))
    }

    pub async fn count(&self, params: ChatParams) -> Result<CountResult, RpcError> {
        let loaded = self.ready().await?;
        let prompt = self.render(&loaded, &params.messages, &params.tools, &params.thinking).await?;
        Ok(CountResult { tokens: prompt.tokens.len() as u32 })
    }

    pub async fn slots(&self) -> Result<SlotsResult, RpcError> {
        let loaded = self.ready().await?;
        let view = loaded.engine.view();
        Ok(SlotsResult {
            queued: view.queued.foreground + view.queued.agent + view.queued.background,
            slots: view.slots,
            hibernated: view.hibernated,
            queued_by_priority: view.queued,
            busy: view.busy,
            pool_tokens: view.pool_tokens,
            reserved_tokens: view.reserved_tokens,
            metrics: Some(loaded.metrics.snapshot()),
        })
    }

    pub async fn hibernate(&self, params: CacheKeyParams) -> Result<Value, RpcError> {
        let loaded = self.ready().await?;
        let (reply, answer) = tokio::sync::oneshot::channel();
        loaded.engine.send(Command::Hibernate { key: params.cache_key, reply }).map_err(unavailable)?;
        match answer.await.map_err(|_| unavailable("the engine stopped"))? {
            Ok(value) => Ok(value),
            Err((code, message)) if code == "not_found" => Err(RpcError::not_found(message)),
            Err((_, message)) => Err(RpcError::internal(message)),
        }
    }

    /// Keep a conversation resident while its turn runs, or release it (`model.hold`). Before the
    /// model is loaded there is nothing to hold: the answer says so.
    pub fn hold(&self, params: HoldParams) -> Result<Value, RpcError> {
        let Some(loaded) = self.current() else { return Ok(json!({"held": false})) };
        loaded.engine.send(Command::Hold { key: params.cache_key, hold: params.hold }).map_err(unavailable)?;
        Ok(json!({"held": params.hold}))
    }

    /// Drop a conversation for good (its session was deleted), resident or on disk.
    pub fn forget(&self, cache_key: String) {
        if let Some(loaded) = self.current() {
            let _ = loaded.engine.send(Command::Forget(cache_key));
        }
    }

    /// Hibernate every idle conversation and return the records (handed to a successor instance).
    pub async fn hibernate_all(&self) -> Vec<Hibernated> {
        let Some(loaded) = self.current() else { return Vec::new() };
        let (reply, answer) = tokio::sync::oneshot::channel();
        if loaded.engine.send(Command::HibernateAll { reply }).is_err() {
            return Vec::new();
        }
        answer.await.unwrap_or_default()
    }

    /// Conversations a predecessor hibernated: adopted now, or at load.
    pub fn adopt(&self, records: Vec<Hibernated>) {
        if let Some(loaded) = self.current() {
            let _ = loaded.engine.send(Command::Adopt(records));
        } else if let Ok(mut adopted) = self.adopted.lock() {
            adopted.extend(records);
        }
    }

    pub fn info(&self, loaded: &Loaded) -> ModelInfo {
        let device = loaded.engine.device_now();
        let spec = loaded.spec;
        let available = loaded.vision.available();
        ModelInfo {
            model_id: spec.model_id(),
            variant: loaded.variant.clone(),
            path: loaded.dir.display().to_string(),
            family: spec.family.to_owned(),
            quant: spec.quant.to_owned(),
            file_bytes: spec.total_bytes(),
            ctx_total: u32::try_from(loaded.plan.ctx_total).unwrap_or(u32::MAX),
            ctx_native: u32::try_from(loaded.geometry.native_context).unwrap_or(u32::MAX),
            slots: u32::try_from(self.resident_capacity(&loaded.plan)).unwrap_or(u32::MAX),
            kv_type: "bf16".into(),
            speculative: if loaded.speculation { "dflash".into() } else { String::new() },
            backend: loaded.engine.backend.to_lowercase(),
            tensor_api: device.matrix_units,
            engine_revision: ENGINE_REVISION.into(),
            extra: json!({
                "configured_variant": self.configured_variant(),
                "identity": loaded.identity,
                "repo": spec.repo,
                "revision": spec.revision,
                "weights_sha256": spec.weights().and_then(|file| file.sha256),
                "load_ms": loaded.load_ms,
                "engine_load_ms": loaded.engine.loaded.load_ms,
                "fit": {"plan": loaded.plan, "geometry": loaded.geometry, "measured": {"device": device, "peak_after_load_bytes": loaded.engine.loaded.peak_memory_bytes}},
                "machine": loaded.machine,
                "speculation": {"enabled": loaded.speculation, "speculator": loaded.engine.loaded.has_speculator, "kind": "dflash (uzu speculator bundled with the model)"},
                "settings": {"prefill_chunk": self.config.prefill_chunk.unwrap_or(engine::PREFILL_CHUNK), "state_capacity": loaded.plan.ctx_total.min(loaded.geometry.native_context)},
                "stop_token_ids": loaded.engine.loaded.stop_token_ids,
                "input_modalities": if available.is_ok() { json!(["text", "image"]) } else { json!(["text"]) },
            }),
            vision: loaded.vision.tower.map(|_| {
                let factor = u64::from(loaded.vision.cached().map_or(32, |config| config.factor()));
                VisionInfo {
                    available: available.is_ok(),
                    loaded: loaded.engine.view().vision_loaded,
                    max_image_pixels: loaded.vision.max_pixels,
                    max_image_tokens: u32::try_from(loaded.vision.max_pixels / (factor * factor)).unwrap_or(u32::MAX),
                    idle_unload_s: loaded.vision.idle_unload_s,
                    detail: available.clone().err(),
                }
            }),
        }
    }

    /// The variant this plugin is configured to run (a different loaded variant is a test instance).
    fn configured_variant(&self) -> String {
        self.config.variant.clone().unwrap_or_else(|| variants::DEFAULT_VARIANT.to_owned())
    }

    pub fn variants(&self) -> VariantsResult {
        let active = self.current().map(|loaded| loaded.variant.clone());
        // Test-only variants are not offered for selection; one is listed only while it runs (a test instance).
        let variants = variants::CATALOG
            .iter()
            .filter(|spec| !spec.test || active.as_deref() == Some(spec.id))
            .map(|spec| {
                let dir = spec.local_dir(&self.paths.models_dir);
                let installed = variants::installed(spec, &dir);
                Variant {
                    id: spec.id.to_owned(),
                    label: spec.label.to_owned(),
                    path: dir.display().to_string(),
                    installed,
                    active: active.as_deref() == Some(spec.id),
                    notes: spec.notes.to_owned(),
                    repo: Some(spec.repo.to_owned()),
                    revision: Some(spec.revision.to_owned()),
                    sha256: spec.weights().and_then(|file| file.sha256).map(str::to_owned),
                    verified: installed.then(|| variants::verified_by_record(spec, &self.paths.models_dir)),
                    bytes: Some(spec.total_bytes()),
                    mtp: None,
                }
            })
            .collect();
        VariantsResult { variants }
    }

    /// Switch the active variant: wait for running requests, release the model, load the other
    /// (downloading it on demand when not installed).
    pub async fn select(self: &Arc<Self>, variant: &str) -> Result<SelectResult, RpcError> {
        let _switch = self.switch.lock().await;
        let Some(previous) = self.current() else {
            // No model is loaded (none was installed, or it failed): download and load this one.
            let started = Instant::now();
            return match self.load(variant, true).await {
                Ok(loaded) => {
                    let info = self.info(&loaded);
                    self.set_phase(Phase::Ready(loaded));
                    let load_ms = started.elapsed().as_secs_f64() * 1000.0;
                    Ok(SelectResult { variant: variant.to_owned(), previous: String::new(), changed: true, reload_ms: load_ms, unload_ms: 0.0, load_ms, info })
                }
                Err(error) => {
                    self.set_phase(Phase::Failed(error.clone()));
                    Err(RpcError::internal(format!("cannot load `{variant}`: {error}")))
                }
            };
        };
        // The variant's vision tower is installed with it (its own small download).
        if let Some(spec) = variants::find(variant) {
            self.install_vision_or_warn(spec).await;
        }
        if previous.variant == variant {
            return Ok(SelectResult {
                variant: variant.to_owned(),
                previous: previous.variant.clone(),
                changed: false,
                reload_ms: 0.0,
                unload_ms: 0.0,
                load_ms: 0.0,
                info: self.info(&previous),
            });
        }
        let spec = variants::find(variant).ok_or_else(|| RpcError::invalid_params(format!("unknown variant `{variant}`")))?;
        // Install before unloading, so a failed download leaves the current model serving.
        let cache = VerifyCache::new(&self.paths.models_dir);
        let progress = self.install_progress();
        variants::install(spec, &self.paths.models_dir, &cache, &progress)
            .await
            .map_err(|error| RpcError::internal(format!("cannot install `{variant}`: {error}")))?;
        let started = Instant::now();
        self.set_phase(Phase::Loading(variant.to_owned()));
        // Drain: requests already admitted finish; new ones wait for the new model.
        loop {
            let view = previous.engine.view();
            if view.busy == 0 && view.queued.foreground + view.queued.agent + view.queued.background == 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let previous_variant = previous.variant.clone();
        let engine = previous.engine.clone();
        drop(previous);
        tokio::task::spawn_blocking(move || engine.shutdown()).await.map_err(|error| RpcError::internal(error.to_string()))?;
        let unload_ms = started.elapsed().as_secs_f64() * 1000.0;
        let load_started = Instant::now();
        match self.load(variant, true).await {
            Ok(loaded) => {
                let load_ms = load_started.elapsed().as_secs_f64() * 1000.0;
                let info = self.info(&loaded);
                self.set_phase(Phase::Ready(loaded));
                Ok(SelectResult {
                    variant: variant.to_owned(),
                    previous: previous_variant,
                    changed: true,
                    reload_ms: started.elapsed().as_secs_f64() * 1000.0,
                    unload_ms,
                    load_ms,
                    info,
                })
            }
            Err(error) => {
                // Bring the previous variant back so the plugin keeps serving.
                tracing::error!(%error, "switching failed; reloading the previous variant");
                match self.load(&previous_variant, false).await {
                    Ok(loaded) => self.set_phase(Phase::Ready(loaded)),
                    Err(again) => self.set_phase(Phase::Failed(again)),
                }
                Err(RpcError::internal(format!("cannot load `{variant}`: {error}")))
            }
        }
    }

    pub async fn shutdown(&self) {
        if let Some(loaded) = self.current() {
            let engine = loaded.engine.clone();
            drop(loaded);
            let _ = tokio::task::spawn_blocking(move || engine.shutdown()).await;
        }
    }
}

/// Metal's recommended working set (0 when no Metal device is available).
fn metal_working_set() -> u64 {
    use metal::MTLDevice;
    <dyn MTLDevice>::system_default().map(|device| device.recommended_max_working_set_size()).unwrap_or(0)
}

fn usage_of(outcome: &Outcome) -> Usage {
    Usage {
        prompt_tokens: outcome.prompt_tokens,
        cached_tokens: outcome.cached_tokens,
        completion_tokens: outcome.completion_tokens,
        prefill_ms: outcome.prefill_ms,
        decode_ms: outcome.decode_ms,
        queue_ms: outcome.queue_ms,
        draft_accepted: outcome.draft_accepted,
        draft_total: outcome.draft_total,
        slot: Some(outcome.slot),
    }
}

fn chat_result(generated: Generated) -> ChatResult {
    let Generated { outcome, parser, ended } = generated;
    let finish_reason = match (ended, outcome.finish) {
        (Some(Ended::Repeated), _) => "repeated",
        (Some(Ended::StopString), _) => "stop",
        (None, Finish::Length) => "length",
        (None, Finish::Stopped) => "cancelled",
        (None, Finish::Stop | Finish::Prefilled) => {
            if parser.calls.is_empty() {
                "stop"
            } else {
                "tool_calls"
            }
        }
    };
    ChatResult {
        content: parser.content.clone(),
        reasoning: parser.reasoning.clone(),
        tool_calls: parser.calls.clone(),
        finish_reason: finish_reason.into(),
        usage: usage_of(&outcome),
        probs: None,
    }
}
