//! The loaded model and every request path of `model@1` this stage serves: installation, memory
//! fit (from the machine, never waiting), loading the engine, templating and output parsing,
//! constraints, variants and switching.

use crate::constraint::{self, Constraint, ToolChoice};
use crate::engine::{self, Command, EngineConfig, EngineHandle, Event, Finish, Outcome, ThinkBudget, Work};
use crate::machine::Machine;
use crate::metrics::Metrics;
use crate::parse::{Detokenizer, OutputParser, Piece, StopScanner};
use crate::repeat::Guard;
use crate::sampler::SamplingPlan;
use crate::template::{self, ChatTemplate};
use crate::variants::{self, Progress, Spec, VerifyCache};
use cc_protocol::contracts::model::{
    ChatDelta, ChatMessage, ChatParams, ChatResult, CompleteParams, CountResult, HoldParams, ModelInfo, PrewarmParams, Priority, Sampling,
    SelectResult, SlotsResult, Thinking, TokenizeParams, TokenizeResult, ToolSpec, Usage, Variant, VariantsResult,
};
use cc_protocol::{ItemSink, RpcError, codes};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

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
    /// Variant id from the catalog (default `bonsai2-27b-pq2`, the only one).
    #[serde(default)]
    pub variant: Option<String>,
    /// Where variants are installed (default `$CORTEX_HOME/models`).
    #[serde(default)]
    pub models_dir: Option<String>,
    /// Upper bound of the memory the plugin may plan for (operator limit; default: the fit's budget).
    #[serde(default)]
    pub memory_limit_bytes: Option<u64>,
    /// Minimum context one conversation needs (default 32768); a machine whose pool cannot give one is a load error.
    #[serde(default)]
    pub min_context_per_slot: Option<u64>,
    /// An operator override of the fitted pool (tokens).
    #[serde(default)]
    pub ctx_total: Option<u64>,
    /// An operator cap on resident conversations (default: the fit's).
    #[serde(default)]
    pub slots: Option<u32>,
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
    pub template: ChatTemplate,
    pub identity: String,
    pub machine: Machine,
    pub metrics: Arc<Metrics>,
    pub load_ms: f64,
    /// `</think>` as tokens.
    pub think_end: Vec<u32>,
    /// [`THINK_BUDGET_CLOSE`] as tokens.
    pub think_close: Vec<u32>,
}

/// A prompt ready for the engine: its tokens and its reuse points.
pub struct Prompt {
    pub tokens: Vec<u32>,
    /// Prompt lengths the engine checkpoints while prefilling (see [`reuse_points`]).
    pub checkpoints: Vec<usize>,
    /// The reuse point that ends the leading system block (one of `checkpoints`).
    pub shared: Option<usize>,
}

/// Where a conversation's later requests can resume: the end of its leading system block and the
/// end of its history before the last message, as token counts of `tokens` (the rendered prompt).
///
/// The last message is the one a caller replaces per call (a stage message, live data) or follows
/// with the answer, so the next request shares the history up to it. Each point is found by
/// rendering the messages before it twice, followed by a user and by an assistant message: the
/// tokens the two renders share are exactly what any continuation renders there, whatever the
/// template does with what comes next. Mechanical, from the template itself (the uzu plugin's).
pub fn reuse_points(tokens: &[u32], prefixes: &[&[Value]], render: &dyn Fn(&[Value]) -> Option<Vec<u32>>) -> Vec<usize> {
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
        let shared = |a: &[u32], b: &[u32]| a.iter().zip(b).take_while(|(x, y)| x == y).count();
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

/// Sets the request's stop flag when the owning future is dropped (cancellation).
struct StopOnDrop(Arc<AtomicBool>);

impl Drop for StopOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

/// Why a stream ended early on the caller's side.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Ended {
    StopString,
    Repeated,
}

/// What one generation produced, before it becomes a contract result.
struct Generated {
    outcome: Outcome,
    parser: OutputParser,
    ended: Option<Ended>,
}

/// The contract's sampling as this plugin applies it (`sampler.rs`).
pub fn sampling_plan(sampling: &Sampling) -> SamplingPlan {
    let seed = sampling.seed.unwrap_or_else(|| {
        let mut bytes = [0u8; 8];
        bytes.copy_from_slice(&uuid::Uuid::new_v4().as_bytes()[..8]);
        u64::from_le_bytes(bytes)
    });
    SamplingPlan {
        temperature: sampling.temperature.max(0.0),
        top_k: sampling.top_k,
        top_p: sampling.top_p,
        min_p: sampling.min_p,
        repeat_penalty: sampling.repeat_penalty.filter(|penalty| (*penalty - 1.0).abs() > f32::EPSILON && *penalty > 0.0),
        presence_penalty: Some(sampling.presence_penalty).filter(|penalty| penalty.abs() > f32::EPSILON),
        seed,
    }
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
}

fn unavailable(message: impl Into<String>) -> RpcError {
    RpcError::new(codes::UNAVAILABLE, message).retryable()
}

/// What this stage of the plugin does not serve yet: a plain error (not retryable), so a caller
/// does not wait for it.
pub fn not_yet(method: &str) -> RpcError {
    RpcError::new(
        codes::UNAVAILABLE,
        format!("{method} is not served by model-bonsai yet; it comes with the next stage of this engine (the uzu engine serves it)."),
    )
}

/// The effort the Bonsai template renders: it has no `high` (the template maps it to `xhigh`) and
/// its `low` thinks about as long as `xhigh` (PrismML's measurement), so `low` renders as `medium`.
/// The thinking budget still follows the requested effort.
pub fn rendered_thinking(thinking: &Thinking) -> Thinking {
    let mut rendered = thinking.clone();
    if rendered.effort.as_deref() == Some("low") {
        rendered.effort = Some("medium".into());
    }
    rendered
}

impl Runtime {
    pub fn new(paths: Paths, config: Config, publisher: Publisher) -> Arc<Self> {
        let (phase, _) = watch::channel(Phase::Starting);
        Arc::new(Self { paths, config, publisher, phase, switch: tokio::sync::Mutex::new(()) })
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
        if !matches!(phase, Phase::Installing(_)) {
            self.publish("model.state", state);
        }
        self.phase.send_replace(phase);
    }

    /// The variant this plugin is configured to run.
    pub fn configured_variant(&self) -> String {
        self.config.variant.clone().unwrap_or_else(|| variants::DEFAULT_VARIANT.to_owned())
    }

    /// Load the configured variant (called once at start, in the background).
    pub async fn start(self: &Arc<Self>) {
        let _switch = self.switch.lock().await;
        let variant = self.configured_variant();
        match self.load(&variant, false).await {
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
                    runtime.phase.send_if_modified(|phase| {
                        if matches!(phase, Phase::Ready(_)) {
                            return false;
                        }
                        *phase = Phase::Installing(progress);
                        true
                    });
                }
            }
        }
    }

    /// Install (when `download` allows), verify, fit and load a variant.
    async fn load(self: &Arc<Self>, variant: &str, download: bool) -> Result<Arc<Loaded>, String> {
        let spec = variants::find(variant).ok_or_else(|| {
            format!("unknown variant `{variant}`; known: {}", variants::CATALOG.iter().map(|spec| spec.id).collect::<Vec<_>>().join(", "))
        })?;
        let dir = spec.local_dir(&self.paths.models_dir);
        if variants::installed(spec, &dir) || download {
            // Downloads only what is missing; verifies every file (records make this instant).
            let cache = VerifyCache::new(&self.paths.models_dir);
            let progress = self.install_progress();
            variants::install(spec, &self.paths.models_dir, &cache, &progress).await?;
        } else {
            return Err(format!(
                "variant `{variant}` is not installed in {} (select it with model.select to download it, or run `cc-model-bonsai install {variant}`)",
                self.paths.models_dir.display()
            ));
        }
        self.set_phase(Phase::Loading(variant.to_owned()));
        let template = ChatTemplate::new(template::template_source(spec.template).ok_or_else(|| format!("template {} is not shipped", spec.template))?)?;
        let machine = Machine::measure();
        let config = EngineConfig {
            weights: dir.join(spec.weights),
            tokenizer: dir.join("tokenizer.json"),
            physical_bytes: machine.physical_bytes,
            memory_limit: self.config.memory_limit_bytes,
            min_context: self.config.min_context_per_slot.unwrap_or(DEFAULT_MIN_CONTEXT_PER_SLOT),
            ctx_total: self.config.ctx_total,
            max_resident: self.config.slots.map(u64::from),
        };
        let started = Instant::now();
        let publisher = self.publisher.clone();
        let handle = tokio::task::spawn_blocking(move || engine::start(config, publisher))
            .await
            .map_err(|error| format!("engine start task failed: {error}"))??;
        let encode = |text: &str| -> Vec<u32> { handle.tokenizer.encode(text, false).map(|encoding| encoding.get_ids().to_vec()).unwrap_or_default() };
        let think_end = encode("</think>");
        let think_close = encode(THINK_BUDGET_CLOSE);
        let plan = &handle.loaded.plan;
        tracing::info!(
            variant,
            ctx_total = handle.loaded.n_ctx,
            resident = handle.loaded.resident,
            checkpoints = plan.checkpoints,
            kv = plan.kv_type.name(),
            load_ms = handle.loaded.load_ms,
            "model loaded"
        );
        Ok(Arc::new(Loaded {
            engine: Arc::new(handle),
            variant: variant.to_owned(),
            spec,
            dir,
            template,
            identity: format!("{}@{}#{}", spec.repo, spec.revision, spec.quant),
            machine,
            metrics: Arc::new(Metrics::default()),
            load_ms: started.elapsed().as_secs_f64() * 1000.0,
            think_end,
            think_close,
        }))
    }

    /// Render and tokenize a conversation, with its reuse points (text only at this stage).
    fn render(&self, loaded: &Loaded, messages: &[ChatMessage], tools: &[ToolSpec], thinking: &Thinking) -> Result<Prompt, RpcError> {
        if messages.is_empty() {
            return Err(RpcError::invalid_params("messages is empty"));
        }
        if messages.iter().any(|message| !message.parts.is_empty()) {
            return Err(RpcError::invalid_params("this model takes no images yet"));
        }
        let tool_values: Vec<Value> = tools.iter().map(template::tool_value).collect();
        // The effort announcement goes after every reuse point (see `ChatTemplate::rendering`).
        let template::Rendering { messages: values, vars } =
            loaded.template.rendering(messages.iter().map(template::message_value).collect(), &tool_values, &rendered_thinking(thinking))?;
        let text = loaded.template.render(&values, &tool_values, &vars)?;
        let tokens = self.encode(loaded, &text)?;
        let render = |messages: &[Value]| -> Option<Vec<u32>> {
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
        Ok(Prompt { tokens, checkpoints, shared })
    }

    fn encode(&self, loaded: &Loaded, text: &str) -> Result<Vec<u32>, RpcError> {
        loaded
            .engine
            .tokenizer
            .encode(text, false)
            .map(|encoding| encoding.get_ids().to_vec())
            .map_err(|error| RpcError::internal(format!("tokenizer: {error}")))
    }

    /// Run one generation, feeding parsed pieces to `sink`.
    #[allow(clippy::too_many_arguments)]
    async fn generate(
        &self,
        loaded: &Arc<Loaded>,
        key: String,
        prompt: Prompt,
        shared_only: bool,
        idle_only: bool,
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
        let request = engine::Request {
            id: 0,
            key,
            prompt: prompt.tokens,
            checkpoints: prompt.checkpoints,
            shared_only,
            idle_only,
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
                    loaded.metrics.request(&usage_of(&outcome));
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

    pub async fn chat(&self, params: ChatParams, items: Option<ItemSink>, cancel: CancellationToken) -> Result<ChatResult, RpcError> {
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
    pub async fn chat_with(&self, params: ChatParams, sink: &mut (dyn FnMut(&Piece) + Send), cancel: CancellationToken) -> Result<ChatResult, RpcError> {
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
            ToolChoice::Named(name) => constraints.push(Constraint::Lark(constraint::tool_call_grammar(&params.tools, Some(name)))),
            // With tools listed and no other constraint, `none` is enforced: no tool-call token.
            ToolChoice::None if !params.tools.is_empty() && constraints.is_empty() => constraints.push(Constraint::Lark(constraint::no_tool_call_grammar())),
            ToolChoice::Auto | ToolChoice::None => {}
        }
        if constraints.len() > 1 {
            return Err(RpcError::invalid_params("json_schema, grammar and a forcing tool_choice are exclusive; send one constraint"));
        }
        let prompt = self.render(&loaded, &params.messages, &params.tools, &params.thinking)?;
        let key = params
            .cache_key
            .clone()
            .unwrap_or_else(|| template::derived_key(&params.messages.iter().map(template::message_value).collect::<Vec<_>>()));
        let parse_tools = !params.tools.is_empty() && choice != ToolChoice::None;
        let parser = OutputParser::new(params.thinking.enabled, parse_tools, &params.tools);
        let grammar = constraints.first().map(Constraint::spec).transpose()?;
        let budget_closes = !loaded.think_end.is_empty() && loaded.think_close.windows(loaded.think_end.len()).any(|window| window == loaded.think_end.as_slice());
        let work = Work::Generate {
            max_tokens: params.max_tokens,
            sampling: sampling_plan(&params.sampling),
            grammar,
            grammar_trigger: params.thinking.enabled.then(|| loaded.think_end.clone()),
            think_budget: (params.thinking.enabled && budget_closes).then(|| ThinkBudget {
                tokens: params.thinking.budget_tokens.unwrap_or_else(|| default_think_budget(params.thinking.effort.as_deref())),
                end: loaded.think_end.clone(),
                close: loaded.think_close.clone(),
            }),
        };
        let generated = self.generate(&loaded, key, prompt, false, params.idle_only, params.priority, work, parser, &params.stop, sink, &cancel).await?;
        Ok(chat_result(generated))
    }

    pub async fn complete(&self, params: CompleteParams, items: Option<ItemSink>, cancel: CancellationToken) -> Result<ChatResult, RpcError> {
        if params.n_probs.is_some() {
            return Err(RpcError::new(codes::INVALID_PARAMS, "n_probs is not served yet"));
        }
        let loaded = self.ready().await?;
        let prompt = Prompt { tokens: self.encode(&loaded, &params.prompt)?, checkpoints: Vec::new(), shared: None };
        let key = params.cache_key.clone().unwrap_or_else(|| format!("complete:{}", uuid::Uuid::new_v4().simple()));
        let work = Work::Generate {
            max_tokens: params.max_tokens,
            sampling: sampling_plan(&params.sampling),
            grammar: params.grammar.clone().map(|grammar| Constraint::Gbnf(grammar).spec()).transpose()?,
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
        let generated = self.generate(&loaded, key, prompt, false, false, params.priority, work, OutputParser::raw(), &params.stop, &mut sink, &cancel).await?;
        Ok(chat_result(generated))
    }

    pub async fn prewarm(&self, params: PrewarmParams, cancel: CancellationToken) -> Result<Value, RpcError> {
        let loaded = self.ready().await?;
        let prompt = self.render(&loaded, &params.messages, &params.tools, &params.thinking)?;
        if params.shared_only && prompt.shared.is_none() {
            return Err(RpcError::invalid_params("shared_only: the messages start with no system block followed by another message"));
        }
        let mut sink = |_: &Piece| {};
        let generated = self
            .generate(&loaded, params.cache_key.clone(), prompt, params.shared_only, params.idle_only, Priority::Background, Work::Prewarm, OutputParser::raw(), &[], &mut sink, &cancel)
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

    /// Keep a conversation resident while its turn runs, or release it (`model.hold`).
    pub fn hold(&self, params: HoldParams) -> Result<Value, RpcError> {
        let Some(loaded) = self.current() else { return Ok(json!({"held": false})) };
        loaded.engine.send(Command::Hold { key: params.cache_key, hold: params.hold }).map_err(unavailable)?;
        Ok(json!({"held": params.hold}))
    }

    /// Drop a conversation for good (its session was deleted).
    pub fn forget(&self, cache_key: String) {
        if let Some(loaded) = self.current() {
            let _ = loaded.engine.send(Command::Forget(cache_key));
        }
    }

    pub async fn tokenize(&self, params: TokenizeParams) -> Result<TokenizeResult, RpcError> {
        let loaded = self.ready().await?;
        Ok(TokenizeResult { tokens: self.encode(&loaded, &params.text)?.into_iter().map(i64::from).collect() })
    }

    pub async fn detokenize(&self, tokens: &[i64]) -> Result<String, RpcError> {
        let loaded = self.ready().await?;
        let ids: Vec<u32> = tokens.iter().filter_map(|id| u32::try_from(*id).ok()).collect();
        loaded.engine.tokenizer.decode(&ids, false).map_err(|error| RpcError::internal(error.to_string()))
    }

    pub async fn count(&self, messages: &[ChatMessage], tools: &[ToolSpec], thinking: &Thinking) -> Result<CountResult, RpcError> {
        let loaded = self.ready().await?;
        Ok(CountResult { tokens: self.render(&loaded, messages, tools, thinking)?.tokens.len() as u32 })
    }

    pub async fn slots(&self) -> Result<SlotsResult, RpcError> {
        let loaded = self.ready().await?;
        let view = loaded.engine.view();
        Ok(SlotsResult {
            queued: view.queued.foreground + view.queued.agent + view.queued.background,
            slots: view.slots,
            hibernated: Vec::new(),
            queued_by_priority: view.queued,
            busy: view.busy,
            pool_tokens: view.pool_tokens,
            reserved_tokens: view.reserved_tokens,
            metrics: Some(loaded.metrics.snapshot()),
        })
    }

    pub fn info(&self, loaded: &Loaded) -> ModelInfo {
        let facts = &loaded.engine.loaded;
        let spec = loaded.spec;
        ModelInfo {
            model_id: spec.model_id(),
            variant: loaded.variant.clone(),
            path: loaded.dir.join(spec.weights).display().to_string(),
            family: spec.family.to_owned(),
            quant: spec.quant.to_owned(),
            file_bytes: spec.weights().map_or(0, |file| file.bytes),
            ctx_total: facts.n_ctx,
            ctx_native: u32::try_from(facts.geometry.native_context).unwrap_or(u32::MAX),
            slots: u32::try_from(facts.resident).unwrap_or(u32::MAX),
            kv_type: facts.plan.kv_type.name().into(),
            speculative: String::new(),
            backend: if cfg!(target_os = "macos") { "metal" } else { "cpu" }.into(),
            tensor_api: None,
            engine_revision: crate::llama::REVISION.into(),
            extra: json!({
                "configured_variant": self.configured_variant(),
                "identity": loaded.identity,
                "repo": spec.repo,
                "revision": spec.revision,
                "weights_sha256": spec.weights().and_then(|file| file.sha256),
                "n_params": facts.n_params,
                "load_ms": loaded.load_ms,
                "engine_load_ms": facts.load_ms,
                "fit": {
                    "plan": facts.plan,
                    "geometry": facts.geometry,
                    "measured": {
                        "gpu_before": facts.gpu_before,
                        "gpu_after": facts.gpu_after,
                        "gpu_now": crate::llama::gpu(),
                        "recurrent_bytes": facts.measured_recurrent_bytes,
                    },
                },
                "machine": loaded.machine,
                "settings": {"batch": crate::fit::BATCH, "micro_batch": crate::fit::MICRO_BATCH},
                "speculation": {"enabled": false, "mtp_layers": facts.geometry.mtp_layers},
                "input_modalities": ["text"],
                "stop_token_ids": facts.stop_token_ids,
                "stage": "generation with checkpoints and constraints; shared blocks, hibernation, images and the endpoint come next",
            }),
            vision: None,
        }
    }

    pub fn variants(&self) -> VariantsResult {
        let active = self.current().map(|loaded| loaded.variant.clone());
        let variants = variants::CATALOG
            .iter()
            .filter(|spec| !spec.test || active.as_deref() == Some(spec.id))
            .map(|spec| {
                let dir = spec.local_dir(&self.paths.models_dir);
                let installed = variants::installed(spec, &dir);
                Variant {
                    id: spec.id.to_owned(),
                    label: spec.label.to_owned(),
                    path: dir.join(spec.weights).display().to_string(),
                    installed,
                    active: active.as_deref() == Some(spec.id),
                    notes: spec.notes.to_owned(),
                    repo: Some(spec.repo.to_owned()),
                    revision: Some(spec.revision.to_owned()),
                    sha256: spec.weights().and_then(|file| file.sha256).map(str::to_owned),
                    verified: installed.then(|| variants::verified_by_record(spec, &self.paths.models_dir)),
                    bytes: Some(spec.total_bytes()),
                    mtp: Some(false),
                }
            })
            .collect();
        VariantsResult { variants }
    }

    /// Switch the active variant: release the model, load the other (downloading it on demand
    /// when not installed). A failed load brings the previous variant back.
    pub async fn select(self: &Arc<Self>, variant: &str) -> Result<SelectResult, RpcError> {
        let _switch = self.switch.lock().await;
        let previous = self.ready().await?;
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

fn usage_of(outcome: &Outcome) -> Usage {
    Usage {
        prompt_tokens: outcome.prompt_tokens,
        cached_tokens: outcome.cached_tokens,
        completion_tokens: outcome.completion_tokens,
        prefill_ms: outcome.prefill_ms,
        decode_ms: outcome.decode_ms,
        queue_ms: outcome.queue_ms,
        draft_accepted: 0,
        draft_total: 0,
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
